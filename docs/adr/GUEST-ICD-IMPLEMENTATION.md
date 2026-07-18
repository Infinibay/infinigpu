# ADR: infinigpu guest Vulkan ICD (own thin driver on Mesa's runtime)

**Status:** ACCEPTED (direction), IN PROGRESS (Phase 0). Supersedes the "reuse Mesa Venus"
option for 3D — see [`../spikes/venus-nvidia-a5000.md`](../spikes/venus-nvidia-a5000.md).

## Context

Today the guest reaches the A5000 for 3D through **one hand-rolled ioctl**
(`DRM_IOCTL_INFINIGPU_SUBMIT3D`, `guest/include/infinigpu_drm.h`): userspace names a *fixed*
workload (`CLEAR`/`TRIANGLE`) and the host replays it (`infinigpu-device::submit_vulkan` →
`infinigpu-replay::HostGpu`). Real guest apps still fall back to **lavapipe (llvmpipe, CPU)** — there
is no hardware Vulkan ICD bound to our device. This ADR is the plan to close that: a real (thin)
guest **Vulkan ICD** so unmodified apps (`vulkaninfo` → `vkcube` → DXVK) drive the A5000.

### Why not reuse Mesa Venus (Path A — rejected here)

The Venus-on-NVIDIA de-risk spike (2026-07-18) found Path A is a **conditional NO-GO on this host**:
the host driver **550.163.01** cannot CPU-`mmap` exported host-visible memory (venus needs it;
lands reliably only in **570.86.10**), and the host also misses the venus-on-NVIDIA-Intel floors
(kernel 6.16+, QEMU 11+, virglrenderer 1.2+). Path A also re-introduces the exact venus/virglrenderer
dependency infinigpu exists to eliminate, and pins us to NVIDIA's undocumented, still-buggy venus
support. Full evidence + sources: [`../spikes/venus-nvidia-a5000.md`](../spikes/venus-nvidia-a5000.md).
Path B (own ICD + own decoder) **already runs on the current 550 / 6.14 / vfio-user-QEMU stack**.

## Decision

Build **our own thin Vulkan ICD (Path B')** on **Mesa's `src/vulkan/runtime` common framework** —
using Mesa's driver *scaffolding* (loader glue, dispatch codegen, `vk_*` base objects, `vk_common_*`
fallbacks, later `wsi_common`), **not** Mesa's venus *ICD* and **not** virglrenderer. The ICD
serializes guest Vulkan into **our own wire** (an extension of `SUBMIT_CMD`/`VulkanWorkload`), which
the host replays on the A5000 via the existing `HostGpu` (ash) executor.

### Build placement — in-tree Mesa fork (the crux)

Mesa's Vulkan runtime is shipped **only** as internal static archives (`static_library()` +
`declare_dependency` + `link_whole`) — **never installed**, no `libvulkan_runtime.so`, no headers, no
pkg-config, no stable ABI (verified against tag `mesa-25.0.7`; no `libvulkan_util*`/`libvulkan_runtime*`
on this host). **There is no supported out-of-tree path.** Every shipping Mesa Vulkan driver (anv,
radv, nvk, panvk, venus, lavapipe, …) lives in-tree under `src/*/vulkan`.

Therefore:

- The ICD is a **new driver dir `src/infinigpu/vulkan/`** built inside a **Mesa fork pinned to the
  tag `mesa-25.0.7`** (matches the host's Mesa 25.0.7, so the guest venus/host loader versions line
  up), maintained as a **rebasable branch / patch series** — the same way NVK/PanVK were developed.
- **Our ICD source lives in THIS repo** under `guest/icd/` (reviewable, versioned with the wire it
  speaks). A build harness (`guest/icd/build.sh`) clones/pins Mesa into `~/.cache/infinigpu/mesa`,
  injects `guest/icd/` as `src/infinigpu/vulkan/`, registers `-Dvulkan-drivers=infinigpu`, and builds
  `libvulkan_infinigpu.so` + its ICD manifest. We do **not** vendor the ~500 MB Mesa tree.

Crib the build+ICD shape from **venus** (`src/virtio/vulkan/meson.build`, the 26-line `vn_icd.c`,
`idep_vulkan_lite_runtime`) and the runtime-object idioms from **lavapipe** (`lvp_device.c`).

### Wire architecture — narrow faithful serialization (not fat replay, not full Venus)

- **Fat replay** (today's named workloads) has **no migration path** to arbitrary apps — the host
  would need to understand every pipeline state up front. Rejected as the end state.
- **Full Venus** (1:1 encoders for ~200 calls) is too big.
- **Chosen:** a **narrow 1:1 wire** for exactly the objects a triangle needs (memory, image, buffer,
  pipeline, ~8 leaf commands, submit) + a **guest↔host handle table**, grown **per app** (each new
  app fails on a missing opcode → add that encoder/decoder). This is "the seed of Venus at ~5% of the
  surface." **SPIR-V is forwarded untouched** + pipeline/layout state; the host's real NVIDIA driver
  compiles it (SPIR-V is vendor-neutral). The hard part is handle mapping, not the shader bytes.

## Plan (phased)

### Phase 0 — Mesa fork + skeleton → `vulkaninfo` binds infinigpu  *(bring-up)*

Fork Mesa 25.0.7, add `src/infinigpu/vulkan/` (~6 files) + the meson registration. Implement the
**~10 hand-written entrypoints** for load → enumerate → create-device (the rest resolve NULL via
`--weak` and the runtime fills `vk_common_*`):

`vk_icdGetInstanceProcAddr`/`infinigpu_GetInstanceProcAddr`, `CreateInstance`, `DestroyInstance`,
the `physical_devices.enumerate` callback (opens `/dev/dri/renderD128`, checks `drmGetVersion()->name
== "infinigpu"`, adds one device), `GetPhysicalDeviceProperties2`/`Features2`/
`QueueFamilyProperties2`/`MemoryProperties2`, `CreateDevice`, `DestroyDevice`, a stub
`infinigpu_queue_submit` (`vk_queue.driver_submit`).

**Deliverable:** `VK_DRIVER_FILES=…/infinigpu_icd.json vulkaninfo` shows `driverID`/`deviceName` =
infinigpu on the A5000-backed render node, not lavapipe. Proves the ICD loads + binds our device.

### Phase 1 — First triangle, no WSI (narrow-faithful wire)  *(the real 3D rung)*

Implement the ~20 entrypoints for render-to-image + readback (agent-scoped): `AllocateMemory` +
`MapMemory2KHR` (→ our GEM BO mmap), `CreateImage`/`ImageView` + memreq/bind, a readback
`CreateBuffer` + bind, `CreateDescriptorSetLayout` (trivial), `CreateGraphicsPipelines` (forward
SPIR-V + `vk_graphics_pipeline_state`), the leaf commands (`BeginCommandBuffer`, `CmdBeginRendering`,
`CmdBindPipeline`, `CmdSetViewport/Scissor`, `CmdDraw`, `CmdEndRendering`, `CmdPipelineBarrier2`,
`CmdCopyImageToBuffer2`, `EndCommandBuffer`), and `driver_submit` → **wire flush**. Use **dynamic
rendering** (skip render-pass/framebuffer objects).

In parallel, host + wire + guest-KMD work:
- **Wire:** extend the `SUBMIT_CMD` trailing region into a structured op stream + a handle table
  (`infinigpu-abi`).
- **Host:** generalize `submit_vulkan`/`HostGpu` to accept **forwarded SPIR-V + pipeline state + the
  op stream** (today it renders a fixed `TRIANGLE_SPV`; the machinery — SPIR-V→module→pipeline→
  submit→readback — already exists in `render_triangle_inner`).
- **Guest KMD:** a richer submit ioctl carrying the serialized stream + referenced BO handles.

**Deliverable:** a real Vulkan program renders a triangle **through the ICD**, read back = lit
pixels, GPU-executed on the A5000 — **replacing `submit3d_test.c` with the real Vulkan API**.

### Phase 2 — WSI → `vkcube`  *(present path)*

Reuse Mesa `wsi_common` with a **headless / `VK_KHR_display`** backend (`wsi_device_init` + a few
callbacks), mapping *present* onto our existing DRM scanout/remoting path — **not** a hand-rolled
`VK_KHR_swapchain`. **Deliverable:** `vkcube` runs in the guest on the A5000; frames flow to the
infiniPixel stream.

### Phase 3 — Broaden → DXVK / vkd3d-proton  *(Windows apps on Linux guest)*

Grow the faithful op-set per app (descriptor updates, dynamic state, more pipelines). Gate on the
`vulkaninfo` extension list; DXVK stresses BDA/descriptor-indexing/timeline-semaphore — add as demanded.

## Consequences

- **New build surface:** a C Mesa-fork ICD alongside the Rust stack + the C KMD. Bounded to
  `guest/icd/` + a rebasable Mesa branch; rebasing onto newer Mesa tags touches only our ~6 files
  (the runtime API is internal and does churn — the main ongoing cost).
- **No external runtime dependency** at product runtime: the shipped `libvulkan_infinigpu.so`
  statically links Mesa's runtime; no venus/virglrenderer, runs on the current host driver.
- Handle-table + faithful wire built once in Phase 1 pays off for every later app.

## Build harness (no-sudo on this host)

meson/ninja/gcc/bison/flex present. Missing deps resolved without sudo: `mako` via `pip --user`/uv
venv; `libdrm-dev` headers extracted from the `.deb` into a local prefix on `PKG_CONFIG_PATH`;
wayland/x11/glx/gallium disabled in a minimal configure (`-Dvulkan-drivers=infinigpu
-Dgallium-drivers= -Dplatforms=... -Dglx=disabled`). See `guest/icd/build.sh`.

## References

- Spike ledger: [`../spikes/venus-nvidia-a5000.md`](../spikes/venus-nvidia-a5000.md)
- 3D accel ADR (venus-path, now superseded for the product path): [`3D-ACCEL-IMPLEMENTATION.md`](3D-ACCEL-IMPLEMENTATION.md)
- Mesa Vulkan runtime: https://docs.mesa3d.org/vulkan/index.html · dispatch · base-objs · graphics-state
- Vulkan-Loader driver interface: https://github.com/KhronosGroup/Vulkan-Loader/blob/main/docs/LoaderDriverInterface.md
- Crib sources (tag mesa-25.0.7): `src/virtio/vulkan/{meson.build,vn_icd.c}`, `src/gallium/frontends/lavapipe/lvp_device.c`
