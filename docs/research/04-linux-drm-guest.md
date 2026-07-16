# 04 — The Linux Guest GPU Driver (DRM/KMS)

**Focus:** Building infinigpu's own Linux guest kernel driver — the DRM/KMS stack,
virtio-gpu as the reference architecture, the minimum viable display, the eventual
Mesa/Vulkan userspace split, and whether any of it can realistically be Rust today.

**Bottom line up front:** For a Linux guest, "our own GPU driver" is a *paravirtual
DRM/KMS driver* that talks a private command protocol to an infinigpu device model in
the host VMM. The virtio-gpu driver (`drivers/gpu/drm/virtio/`) is the closest existing
reference and we should model the guest KMD on it. A *display-only* first milestone is
genuinely small (modeset + dumb buffers + pageflip). 3D/Vulkan is a separate, much
larger effort that rides Mesa userspace. And the guest KMD should be **C, not Rust**,
for milestone 1 — the Rust DRM abstractions that exist upstream do **not** yet cover
KMS/modeset.

---

## 1. The DRM/KMS stack we implement

A Linux GPU driver is a DRM (Direct Rendering Manager) driver. The guest KMD registers a
`struct drm_driver` and, for display, a set of KMS (Kernel Mode Setting) objects. The
mandatory pieces:

**Driver registration.** Allocate a `drm_device` (via `devm_drm_dev_alloc` / for
virtio, tied to the virtio device probe), fill a `struct drm_driver` with feature flags
(`DRIVER_MODESET | DRIVER_GEM | DRIVER_ATOMIC | DRIVER_RENDER`) and callbacks
(`.dumb_create`, prime import/export, ioctls), then `drm_dev_register()`. virtio-gpu's
`virtio_gpu_probe()` does exactly this: sets DMA params, initializes the device, and
registers with DRM. ([virtgpu_drv.c](https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/virtio/virtgpu_drv.c))

**KMS object model.** The display pipeline is a fixed chain:
`framebuffer → plane → CRTC → encoder → connector`.
([drm-kms](https://docs.kernel.org/gpu/drm-kms.html))

- **`drm_framebuffer`** — wraps a GEM buffer + pixel format/pitch; the pixel source.
- **`drm_plane`** — accepts a framebuffer, does composition/blending; every CRTC needs
  at least a **primary** plane. (Cursor/overlay planes optional.)
- **`drm_crtc`** — combines plane output, owns scanout timing and the mode.
- **`drm_encoder`** — routes CRTC output to a connector; "serves no purpose in the
  userspace API" but is still exposed and must exist, one per active connector.
- **`drm_connector`** — the display endpoint; carries the mode list (from EDID or a
  fixed mode).

**Minimum:** one CRTC + one primary plane + one encoder + one connector per virtual
display head. ([drm-kms](https://docs.kernel.org/gpu/drm-kms.html))

**Atomic modesetting.** New drivers must be atomic (`DRIVER_ATOMIC`). All state changes
(mode, plane assignment, buffer) are bundled into one transactional commit via
`drm_plane_state`/`drm_crtc_state`/`drm_connector_state`. The driver implements
`atomic_check` (validate — no hardware touched, so a test-only commit can fail cleanly)
and `atomic_commit` (apply). ([drm-kms](https://docs.kernel.org/gpu/drm-kms.html))

**GEM buffer objects.** GEM (Graphics Execution Manager) is the buffer-management API.
For a paravirtual driver the practical base is **`drm_gem_shmem`** — shmem-backed,
CPU-mappable, page-list objects; the driver allocates pages (`shmem_read_mapping_page_gfp`)
lazily or up front. ([kms-helpers](https://dri.freedesktop.org/docs/drm/gpu/drm-kms-helpers.html))
This is exactly right for us: the guest framebuffer lives in guest RAM pages that the
host reads.

**Dumb vs accelerated buffers.** *Dumb buffers* are the standard, format-agnostic
scanout-capable buffers created via the `drm_mode_create_dumb` ioctl → driver
`.dumb_create`. They are meant for simple framebuffers with **no acceleration** — perfect
for a display-only driver. Accelerated buffers (tiled, with GPU-specific layouts, bound
into a GPU VM) come later with 3D. ([drm-kms](https://docs.kernel.org/gpu/drm-kms.html))

**Fences / sync (dma-fence).** Cross-device/queue completion is expressed with
`dma_fence`. For display, the atomic helpers already handle it: `prepare_fb` extracts a
buffer's fence and `drm_atomic_set_fence_for_plane()` makes the CRTC wait on it before
flipping, transparently for implicit/explicit fencing.
([kms-helpers](https://dri.freedesktop.org/docs/drm/gpu/drm-kms-helpers.html))
A pure display driver barely touches fences; a 3D driver needs real fence signalling on
command completion plus `drm_syncobj` for userspace.

**PRIME / dma-buf.** PRIME exports/imports GEM buffers as `dma-buf` fds for zero-copy
sharing (compositor ↔ client, or between devices). The driver sets prime import/export
callbacks. Needed for Wayland/X hardware compositing; deferrable past the very first
display bring-up but cheap to wire when using shmem GEM helpers.

---

## 2. virtio-gpu as our reference architecture

virtio-gpu (`drivers/gpu/drm/virtio/`) is a **KMS driver over virtqueues** and is the
best-matching template for a paravirtual guest KMD. ([LWN](https://lwn.net/Articles/637721/))

**Two virtqueues:**
- **controlq (queue 0)** — all resource/scanout/transfer commands.
- **cursorq (queue 1)** — a "fast track" for cursor position/shape so pointer updates
  aren't stuck behind slow control commands.
([virtio-gpu spec](https://docs.oasis-open.org/virtio/virtio/v1.1/csprd01/tex/virtio-gpu.tex))

**Resource model.** The host owns *resources*; the guest performs DMA into them. The
guest queries displays with `VIRTIO_GPU_CMD_GET_DISPLAY_INFO` (scanout count, preferred
resolution; fallback 1024×768 on scanout 0 if none).
([virtio-gpu spec](https://docs.oasis-open.org/virtio/virtio/v1.1/csprd01/tex/virtio-gpu.tex))

**The exact 2D "make a framebuffer visible" flow** (this is the whole display path):
1. `VIRTIO_GPU_CMD_RESOURCE_CREATE_2D` — create a host 2D resource (width/height/format).
2. `VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING` — hand the host a scatter-list of guest RAM
   pages as backing (framebuffer need not be physically contiguous).
3. `VIRTIO_GPU_CMD_SET_SCANOUT` — bind the resource (a rect of it) to a scanout/display.
4. `VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D` — copy updated pixels guest→host resource.
5. `VIRTIO_GPU_CMD_RESOURCE_FLUSH` — flush the resource to screen.
Steps 4–5 repeat every frame; double-buffered pageflip = create two resources and
alternate `SET_SCANOUT`/`RESOURCE_FLUSH`.
([virtio-gpu spec](https://docs.oasis-open.org/virtio/virtio/v1.1/csprd01/tex/virtio-gpu.tex))

**Cursor** is just a 64×64 resource created the same way, then
`VIRTIO_GPU_CMD_UPDATE_CURSOR` on the cursorq.
([virtio-gpu spec](https://docs.oasis-open.org/virtio/virtio/v1.1/csprd01/tex/virtio-gpu.tex))

**Fencing on the wire:** set `VIRTIO_GPU_FLAG_FENCE` on a command and the device only
responds after processing completes — the primitive the driver maps onto `dma_fence`.
([virtio-gpu spec](https://docs.oasis-open.org/virtio/virtio/v1.1/csprd01/tex/virtio-gpu.tex))

**Feature flags** gate capabilities: `VIRTIO_GPU_F_VIRGL` (3D), `_EDID`, `_RESOURCE_UUID`,
`_RESOURCE_BLOB` (host-mappable "blob" resources for zero-copy), `_CONTEXT_INIT`
(multiple typed contexts on one device — the basis for 3D/native-context).
([virtgpu_drv.c](https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/virtio/virtgpu_drv.c))
The 2D display path uses **none** of them — a plain virtio-gpu (no VIRGL) is a working
KMS display driver already. That is the shape of our milestone 1.

**Takeaway for infinigpu:** our guest KMD is structurally a virtio-gpu clone with our
own transport (whatever the device model exposes — a virtio device, or a custom PCI
device with our own ring). We inherit the resource/scanout/transfer/flush model wholesale.

---

## 3. Minimum for a working display, then 3D on top

**Milestone-1 driver = modeset + dumb framebuffer + pageflip/vblank.** Concretely:
- Register `drm_driver` with `DRIVER_MODESET | DRIVER_GEM | DRIVER_ATOMIC`.
- One KMS pipe: primary plane + CRTC + encoder + connector. Use the
  **`drm_simple_display_pipe`** helper (tinydrm-style) — it collapses the plane/CRTC/
  encoder boilerplate into one object with a single fixed mode, the documented path for
  "very simple display hardware."
  ([tinydrm](https://www.kernel.org/doc/html/v4.14/gpu/tinydrm.html))
- `drm_gem_shmem` GEM + `.dumb_create` for scanout buffers.
- `atomic_commit` translates a flip into: `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`
  (our equivalents), then signal the vblank/pageflip-done event so userspace/`drmModePageFlip`
  is unblocked at the next vblank. ([drm-kms](https://docs.kernel.org/gpu/drm-kms.html))

That is a self-contained, testable deliverable: a Linux guest boots, gets a
`/dev/dri/card0`, runs a Wayland compositor or X modesetting DDX purely on software
rendering (llvmpipe), and puts pixels on the host through our device. No 3D, no Mesa
hardware driver, no Vulkan.

**Then 3D rides Mesa userspace.** The kernel driver never does 3D rendering itself; it
becomes a command-submission transport. Three reference models exist, in ascending order
of "how native it feels" and descending order of portability:

- **virgl (Gallium):** guest Mesa `virgl` driver serializes Gallium/TGSI-level commands
  over virtio-gpu 3D to host `virglrenderer`, which replays them as host OpenGL. Needs
  `VIRTIO_GPU_F_VIRGL` + context init. ([virgl](https://docs.mesa3d.org/drivers/virgl.html),
  [Collabora](https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/))
- **Venus:** a guest **Vulkan ICD** (`virtio` driver in Mesa) that serializes *Vulkan*
  command streams into a ring shared with the host, executed by host-side `virglrenderer`
  → real Vulkan. Requires `VIRTGPU_PARAM_CONTEXT_INIT`, upstreamed in **kernel 5.16**.
  Supports Vulkan 1.3 as of early 2025. OpenGL can layer on top via Zink→Venus.
  ([venus](https://docs.mesa3d.org/drivers/venus.html))
- **DRM native context (vDRM):** mediates the *host kernel driver UAPI* instead of a
  graphics API, so the guest runs the host GPU's **real Mesa driver** and the vGPU
  "appears as a native host GPU device." Lowest CPU overhead, near-native performance,
  but needs a guest driver *tailored to the host hardware*.
  ([Collabora](https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/))
  **Crucial for us:** the four native contexts that exist are **Freedreno (upstream),
  AMDGPU (upstream, Mesa 25.0), Intel i915 (MRs open), Asahi (partial)** — **there is no
  NVIDIA native context.** ([Phoronix AMDGPU nctx](https://www.phoronix.com/news/AMDGPU-VirtIO-Native-Mesa-25.0),
  [qemu-devel v12](https://lists.gnu.org/archive/html/qemu-devel/2025-05/msg05596.html))

The **Vulkan ICD** in every model is a guest-side userspace `.so` + JSON manifest that
Mesa/the Vulkan loader picks up; infinigpu would ship its own ICD if we invent our own
3D protocol rather than reusing Venus.

---

## 4. Rust-for-Linux DRM status (and can our guest KMD be Rust?)

Blunt answer: **display driver in pure upstream Rust is not possible today; write the
milestone-1 KMD in C.** The 3D/DRM *render* side is closer.

**What IS upstream (or landing, kernels ~6.15 → 7.2, 2025–2026):** the foundational DRM
Rust abstractions — `drm::Device`, `drm::Driver`, **GEM object abstraction**, **IOCTL**
abstraction, **File** abstraction, and DRM driver registration (via Devres), co-developed
by Danilo Krummrich and Asahi Lina. GPUVM (GPU virtual-memory) abstractions, including a
GPUVM "immediate mode" abstraction, are landing around Linux 7.2.
([LWN 978928](https://lwn.net/Articles/978928/),
[Phoronix 7.2 DRM Rust](https://www.phoronix.com/news/Linux-7.2-DRM-Rust))

**What is NOT upstream: KMS/modeset.** The initial abstraction series "does **not**
include KMS/modeset or GPUVM abstractions." ([LWN 978928](https://lwn.net/Articles/978928/))
KMS Rust bindings exist only **out-of-tree / as RFCs** — the Asahi driver's own KMS
bindings and the "Rust bindings for KMS + RVKMS" RFC — proposing a `Kms` type on
`Driver` with init-only callbacks, but as of the latest public discussion these are not
merged. ([Asahi Rust DRM RFC](https://lore.kernel.org/asahi/20230307-rust-drm-v1-0-917ff5bc80a8@asahilina.net/),
[RVKMS RFC](https://www.mail-archive.com/dri-devel@lists.freedesktop.org/msg486701.html))
So a *display* driver — which is all KMS — would have to carry out-of-tree bindings or
write the KMS layer in C. Dave Airlie has said DRM is roughly a year from *requiring*
Rust for new drivers, but that is a 2026-forward trajectory, not today's reality.
([Phoronix 7.2](https://www.phoronix.com/news/Linux-7.2-DRM-Rust))

**Nova** is the live proof-of-concept for Rust GPU drivers: a Rust NVIDIA driver for
GSP-based GPUs (Turing/RTX 20 and newer — which **includes our Ampere GA102 A5000s**),
split into `nova-core` (PCI, GSP firmware, command queues) and `nova-drm` (the DRM
interfaces). It is explicitly early: "starting out with just a stub driver," "current
work now focuses more on the actual driver," not a full KMS/3D driver yet.
([Nova](https://rust-for-linux.com/nova-gpu-driver), [LWN 978928](https://lwn.net/Articles/978928/))
Its relevance to us is indirect: Nova/NVK is a candidate *host-side* open stack if we
ever want an NVIDIA native context, and it is the ecosystem proving the Rust DRM
abstractions we might later use for our *render* node.

**Verdict on our KMD language:** milestone 1 = **C** (KMS is C-only upstream). Rust
becomes realistic for the *render/3D submission* path (GEM + IOCTL + GPUVM abstractions
exist) or when KMS Rust bindings land. Don't gate the display milestone on Rust.

---

## 5. Recommended first-milestone Linux driver shape

**Milestone 1 — "pixels on screen" (C, display-only):**
- A paravirtual DRM/KMS driver modeled directly on virtio-gpu's 2D path.
- Transport: reuse a virtqueue-style ring (either an actual virtio device ID or our own
  PCI device with a control ring); do **not** reinvent the resource model — copy
  virtio-gpu's create_2d / attach_backing / set_scanout / transfer_to_host / flush.
- `drm_driver` (`MODESET|GEM|ATOMIC`) + `drm_simple_display_pipe` (primary plane + CRTC +
  encoder + connector) + `drm_gem_shmem` + `.dumb_create`.
- Modeset from a host display-info command; pageflip via atomic commit →
  transfer+flush → vblank event.
- Multi-head = multiple scanouts, mirroring what virtio-gpu already supports.
- Deliverable: Linux guest boots to a software-rendered (llvmpipe) desktop displayed on
  the host, cursor working, resize/modeset working.

**Milestone 2 — render node + 3D transport:** add `DRIVER_RENDER`, a command-submission
ioctl, real `dma_fence` signalling, blob/host-mappable buffers, and choose the userspace
model. Fastest path to real GPU acceleration on our NVIDIA hardware is almost certainly
**API-forwarding (a Venus-style Vulkan protocol)** rather than an NVIDIA native context,
because no NVIDIA native context exists and building one implies an open host NVIDIA
stack (Nova/NVK) that is itself immature.

**The guest kernel / userspace (Mesa) split:**

| Layer | Where | What it is | Milestone |
|---|---|---|---|
| Guest KMD | guest kernel, **C** | DRM/KMS: modeset, dumb FBs, GEM, ring transport, fences | **M1** |
| Guest 3D UMD | guest userspace **Mesa** | Gallium driver (virgl-like) for OpenGL, and/or a Vulkan **ICD** (Venus-like) serializing commands to host | M2 |
| Host device model | host VMM (Rust) | our infinigpu device: owns resources, scanouts, time-slices the 2× A5000s | M1 (2D) → M2 (3D renderer) |
| Host renderer | host | replays guest command stream on real NVIDIA GPU (our virglrenderer analog) | M2 |

The kernel driver stays deliberately thin (transport + KMS + buffer management); *all*
rendering intelligence lives in guest Mesa userspace and the host renderer. This mirrors
how every successful virtual-GPU stack is structured and keeps the hard, fast-moving 3D
logic out of the kernel where iteration is cheap and crashes don't take down the guest.

**Honest risk flags:** (1) time-slicing 2× A5000s across many guests is a *host
device-model* problem, not a guest-KMD problem — the guest KMD is largely
scheduler-agnostic, which is good. (2) Windows guests (separate research focus) need an
entirely different WDDM driver; the *protocol* should be designed OS-neutral so both
guest drivers hit the same host device. (3) Reinventing the virtio-gpu wire protocol from
scratch buys us nothing for M1 and costs us the mature spec + host reference; strongly
prefer *extending* the virtio-gpu model over a clean-room protocol until 3D forces
divergence.

---

## Sources

- Linux kernel — Kernel Mode Setting (KMS): https://docs.kernel.org/gpu/drm-kms.html
- Linux kernel — Mode Setting Helper Functions (simple pipe, fences): https://dri.freedesktop.org/docs/drm/gpu/drm-kms-helpers.html
- Linux kernel — tinydrm / drm_simple_display_pipe: https://www.kernel.org/doc/html/v4.14/gpu/tinydrm.html
- virtio-gpu driver source (`virtgpu_drv.c`): https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/virtio/virtgpu_drv.c
- `drm_simple_kms_helper.c`: https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/drm_simple_kms_helper.c
- OASIS virtio-gpu specification (2D resource flow, queues): https://docs.oasis-open.org/virtio/virtio/v1.1/csprd01/tex/virtio-gpu.tex
- LWN — "Add virtio gpu driver": https://lwn.net/Articles/637721/
- Mesa — VirGL driver: https://docs.mesa3d.org/drivers/virgl.html
- Mesa — Virtio-GPU Venus (Vulkan) driver: https://docs.mesa3d.org/drivers/venus.html
- Collabora — "The state of GFX virtualization using virglrenderer" (native context / vDRM): https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/
- Phoronix — AMDGPU VirtIO Native Context merged (Mesa 25.0): https://www.phoronix.com/news/AMDGPU-VirtIO-Native-Mesa-25.0
- qemu-devel — "Support virtio-gpu DRM native context" v12 (native context driver list): https://lists.gnu.org/archive/html/qemu-devel/2025-05/msg05596.html
- LWN — "DRM Rust abstractions and Nova": https://lwn.net/Articles/978928/
- Phoronix — "Linux 7.2 DRM Rust": https://www.phoronix.com/news/Linux-7.2-DRM-Rust
- Rust for Linux — Nova GPU driver: https://rust-for-linux.com/nova-gpu-driver
- Asahi Lina — Rust DRM subsystem abstractions RFC (KMS out-of-tree): https://lore.kernel.org/asahi/20230307-rust-drm-v1-0-917ff5bc80a8@asahilina.net/
- RVKMS / Rust KMS bindings RFC: https://www.mail-archive.com/dri-devel@lists.freedesktop.org/msg486701.html
