# Spike: does Venus drive NVIDIA's proprietary Vulkan on the A5000?

**Status: RUN 2026-07-18 — CONDITIONAL NO-GO on this host as-is.** Path A (reuse Mesa Venus) is
*viable in principle* on NVIDIA but is **blocked on this exact host** by a stack of version-pinned
upgrades (driver **570.86+**, kernel **6.16+**, QEMU **11+**, virglrenderer **1.2+**), and even the
driver piece needs a reboot of the A5000 host that currently runs the live infinigpu VM. See
**"What this run actually established"** below. This is the Phase-0 go/no-go gate for
[`docs/adr/3D-ACCEL-IMPLEMENTATION.md`](../adr/3D-ACCEL-IMPLEMENTATION.md).

## What this run actually established (2026-07-18, driver 550.163.01)

Ran on the dev host **without** touching the driver (staged the stock venus stack in a scratchpad:
distro `qemu-system-x86_64` 9.2.1, virglrenderer 1.1.0, `virgl_render_server` extracted from the
`virgl-server` deb and pointed at via `RENDER_SERVER_EXEC_PATH`, an Ubuntu 25.04 cloud-init guest).

1. **Extensions are NOT the gate — and they are present on 550.** `vulkaninfo` forced onto NVIDIA's
   ICD (`VK_DRIVER_FILES=nvidia_icd.json`) on 550.163.01 / A5000 advertises everything the *Linux*
   venus host path needs: `VK_KHR_external_memory_fd`, `VK_EXT_external_memory_host`,
   `VK_EXT_image_drm_format_modifier`, `VK_EXT_queue_family_foreign`, and a **HOST_VISIBLE +
   DEVICE_LOCAL** memory type (`memoryTypes[5]`, mappable VRAM). Only `VK_EXT_external_memory_dma_buf`
   is absent, and Mesa's Linux venus path does not require it (that's the *Android* requirement).
2. **The real gate is a runtime capability, invisible to `vulkaninfo`:** venus allocates
   `HOST_VISIBLE` memory, chains `VkExportMemoryAllocateInfo`, exports an fd, and the render server
   must **CPU-`mmap` that exported device memory**. NVIDIA only allowed this in **565.57.01**
   (flaky, GPU-dependent) and reliably in **570.86.10**. **550.x cannot** — and **550.127.05 is
   named among the failing drivers** in virglrenderer issue #524. So a 550 spike is a guaranteed
   `VK_ERROR_MEMORY_MAP_FAILED`, exactly as the header warned.
3. **venus never even started on this NVIDIA host** — an independent blocker *before* the mmap gate:
   QEMU's `virtio-gpu-gl` (venus rides on it) requires a **GL-capable display backend**. `-display
   none` is rejected (`display backend does not have OpenGL support enabled`); `-display egl-headless`
   **fails on NVIDIA** (`egl: no drm render node available` — the proprietary driver exposes no
   GBM-capable render node). So Path A also needs NVIDIA GBM working for QEMU, or a newer QEMU.
4. **This host misses multiple other venus-on-NVIDIA-Intel floors** (Mesa docs, NVIDIA + Intel CPU):
   kernel **6.16+** (host has **6.14**), QEMU **11.0+** with `-accel kvm,honor-guest-pat=on` (host
   has **9.2.1**), virglrenderer **≥1.2.0** (host has **1.1.0**). Host CPU is **GenuineIntel**.

**Net:** Path A on this hardware = upgrade driver + kernel + QEMU + virglrenderer, solve NVIDIA-GBM,
then accept NVIDIA's still-buggy, undocumented venus-host support (corruption bugs reported as late as
595.x) — i.e. exactly the version-skew fragility the ADR flagged, made concrete. Path B (own
ICD + own decoder) already runs on the *current* 550 / 6.14 / vfio-user-QEMU stack.

Re-run with `scripts/spike-venus-nvidia.sh /path/to/guest.qcow2` **after** the host is upgraded, to
capture a real GO (or confirm NVIDIA's residual venus bugs are tolerable).

## Why this exists

The entire 3D plan reuses **Mesa Venus** (guest) + **virglrenderer-venus** (host decoder) to run
guest Vulkan on the A5000. Venus is CI-validated mostly on Intel/AMD Mesa hosts; NVIDIA-proprietary
as a Venus *host* is the unproven load-bearing assumption. This spike answers it with the **stock**
stack (no infinigpu code), so a NO-GO kills the reuse premise cheaply — before weeks of decoder work —
and forces the ADR's fallback (own `vn_protocol_renderer` / own thin guest ICD) or an AMD/Intel-first
pivot.

## Preconditions

- [ ] **Host NVIDIA driver ≥ 570.86** (the Mesa-documented Venus-host floor). The installed baseline
  was **550.163.01**, which is BELOW the floor — a spike on it is a **guaranteed false NO-GO**. Pin
  ≥ 570.86 (fleet baseline 570.153.02 or 575.x), reboot, confirm with `nvidia-smi`.
- [ ] Distro `qemu-system-x86_64` with `virtio-gpu-gl` (venus=), virglrenderer built `-Dvenus=true`,
  `/usr/share/vulkan/icd.d/{nvidia_icd,virtio_icd.x86_64}.json` present, `/dev/kvm`.
- [ ] Guest = Ubuntu 25.04+ (virtio-gpu already `DRIVER_RENDER` → `/dev/dri/renderD128` exists) with
  Mesa 25.x venus ICD; in-guest force `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json`.

## The four-rung ladder

| Rung | Workload (in guest) | Pass criterion | Result | Notes |
|------|---------------------|----------------|--------|-------|
| 1 | `VN_DEBUG=init vulkaninfo` | `driverID=VK_DRIVER_ID_MESA_VENUS`, `deviceName='NVIDIA RTX A5000'`, `apiVersion≥1.3` (NOT llvmpipe/lavapipe) | ☐ | on fail, read the missing host extension from the `VN_DEBUG=init` host log |
| 2 | `vkcube` + host `nvidia-smi dmon` | the qemu PID shows non-zero GPU-Util/VRAM (silicon, not llvmpipe) | ☐ | |
| 3 **(crux)** | `HOST_VISIBLE\|HOST_COHERENT` compute round-trip | GPU writes guest-mappable memory; `memcpy` readback byte-correct | ☐ | NVIDIA-Venus's historical weak point (host-visible dma-buf export) — this is what DXVK/vkd3d staging buffers need |
| 4 | `wine` + DXVK `d3d11-triangle`, `DXVK_HUD=devinfo` | renders; HUD shows the Venus device | ☐ | de-risks the whole Windows/D3D path on Linux with zero WDK work |

## Decision

> **GO** iff all four rungs pass on a host pinned ≥ 570.86.
> **NO-GO for Path A** if Rung 1 or Rung 3 fails → take the 3D-ADR **Fallback** (own
> `vn_protocol_renderer` keeping stock guest Mesa, or own thin guest ICD) **or** pivot host silicon
> to AMD/Intel-first (Path A works there unchanged).

- **Driver version tested:** 550.163.01 (below the 570.86 floor — see run notes above).
- **Decision:** ☑ **CONDITIONAL NO-GO on this host as-is.** Path A stays viable but requires
  driver ≥ 570.124.06 + kernel ≥ 6.16 + QEMU ≥ 11 + virglrenderer ≥ 1.2, a reboot of the live-GPU
  host, and solving NVIDIA-GBM for QEMU's GL backend. Rung 1/3 could not be reached on 550
  (venus does not start; and the mmap gate is known-absent on 550.x per virglrenderer #524).
- **Negotiated NVIDIA host-extension set:** not applicable — the gate is a runtime mmap capability,
  not an extension. 550 advertises the needed extensions but cannot CPU-map exported host-visible
  memory; that capability lands in 565.57.01 (flaky) / 570.86.10 (reliable).
- **Date / operator:** 2026-07-18, autonomous run (staged stock venus stack, no driver change).

### Sources (venus-on-NVIDIA driver floor)
- Mesa Venus docs — 570.86 tested-host floor, Linux vs Android reqs, the illegal-`vkMapMemory`
  assumption: https://docs.mesa3d.org/drivers/venus.html
- virglrenderer #524 — `vkMapMemory` fails on NVIDIA; **550.127.05 fails**, **570.86.10 fixes**:
  https://gitlab.freedesktop.org/virgl/virglrenderer/-/issues/524
- NVIDIA forum — CPU-mmap of exported dma-buf added in 565.57.01, GPU-dependent, venus swapchain
  fails: https://forums.developer.nvidia.com/t/unable-to-map-dmabuf-exported-memory-to-cpu-visible-buffer-permission-denied/281768
- Collabora, state of gfx virtualization (Jan 2025) — NVIDIA venus issues still open:
  https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/
- vulkan.gpuinfo.org — RTX A5000 on 550.40.79 advertises the four external-memory extensions:
  https://vulkan.gpuinfo.org/displayreport.php?id=34417

On a future GO (post-upgrade): authorize `crates/infinigpu-replay/src/venus/` + the BAR2
`HOST_VISIBLE` aperture (3D-ADR Phase 2). Staying NO-GO here points to the 3D-ADR **Fallback** —
our own thin guest ICD + own decoder — which already runs on the current host.
