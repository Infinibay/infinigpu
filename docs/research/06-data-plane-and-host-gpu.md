# 06 — Data-Plane & Host GPU Execution (the core engineering problem)

**Scope:** how guest GPU work physically reaches the host NVIDIA A5000 (GA102, Ampere),
gets replayed, is time-shared across many VMs, and is presented back — for a 100%
custom Rust stack that studies but does not *adopt* an existing QEMU GPU driver.

## Bottom line up front

For our hardware and licensing constraints there is exactly one viable family of
approaches: **userspace API-remoting**. The A5000 cannot be hardware-partitioned —
GA102 does **not** support MIG (only datacenter A100/H100-class parts do), and the only
SR-IOV/vGPU path on this card requires NVIDIA's licensed vGPU host driver, which our
core forbids ([NVIDIA vGPU product matrix](https://docs.nvidia.com/vgpu/19.0/product-support-matrix/index.html),
[MIG is A100/H100-class only](https://sagar-parmar.medium.com/beyond-partitioning-a-deep-dive-into-nvidia-gpu-time-slicing-533705821f0d)).
So the GPU stays a single un-partitioned device owned by **one host arbiter process**,
and every guest's graphics API stream is serialized over a transport, decoded, and
replayed against the host's real Vulkan driver. This is precisely the Venus /
virglrenderer / gfxstream model — we should build our own but crib their architecture
wholesale. The recommended MVP is **Linux-guest → Linux-host, Vulkan-only, one command
ring, one GPU**.

## 1. Transport / data-plane (guest ↔ host)

Three candidate substrates, in decreasing order of "already solves our problem":

**virtio-gpu virtqueues.** The de-facto standard. A PCI virtio device with a *control*
virtqueue (command submission) and *cursor* virtqueue; the guest places command buffers
in a ring, kicks a doorbell (virtqueue notify), the host consumes descriptors and writes
completions/fences back
([QEMU virtio-gpu](https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html)).
Crucially it already defines the whole **resource + blob + fence + transfer** model we
would otherwise reinvent:
- **Blob resources** are the zero-copy primitive. Three memory types:
  `VIRTIO_GPU_BLOB_MEM_GUEST` (guest-allocated pages), `VIRTIO_GPU_BLOB_MEM_HOST3D`
  (host GPU allocation), and `VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST` (both)
  ([blob resources patch](https://patchwork.kernel.org/project/dri-devel/patch/20200814024000.2485-11-gurchetansingh@chromium.org/)).
  On the host, QEMU wraps guest pages in a **udmabuf** so the host GPU can DMA directly
  from guest memory with no copy; a `hostmem` PCI window (typically 256M–8G) lets
  host-allocated GPU memory be mapped straight into the guest address space
  ([QEMU virtio-gpu options](https://qemu.readthedocs.io/en/v10.0.3/system/devices/virtio-gpu.html)).
- **Non-blob path** needs explicit `VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D` /
  `TRANSFER_FROM_HOST_3D` copies to sync guest↔host resource contents — a real
  bandwidth tax we avoid by using blobs.
- **Fences** ride the same queue: the guest submits `RESOURCE_FLUSH` and waits on a
  **dma-fence**; the host doesn't ACK until the host GPU blit is complete, preventing
  tearing when guest and host share blob storage
  ([blob sync mechanism](https://lore.kernel.org/qemu-devel/20210901211014.2800391-3-vivek.kasireddy@intel.com/T/)).

**ivshmem** (inter-VM shared memory) gives you a raw shared BAR + optional
doorbell/interrupt via a small server, but *no* resource/fence/transfer semantics —
you'd hand-roll all of it
([ivshmem spec](https://www.qemu.org/docs/master/specs/ivshmem-spec.html)). Fine as a
pure ring transport, poor as a GPU protocol.

**vfio-user** lets you implement an arbitrary PCI device entirely in a userspace process
over a UNIX socket, with sparse mmap'd BARs so hot paths like doorbells are direct-mapped
while control registers trap
([vfio-user spec](https://www.qemu.org/docs/master/interop/vfio-user.html)). Rust
binding exists (`libvfio-user`). This is the most flexible if we invent our own device
ABI, at the cost of writing the guest driver from scratch.

**The Rust angle matters here.** The ecosystem is already migrating this exact stack to
Rust: **`rutabaga_gfx`** (crosvm) is a Rust VGI that dispatches virtio-gpu hypercalls to
pluggable context types; **`vhost-device-gpu`** (rust-vmm) runs the whole graphics
backend in a *separate* process over vhost-user for isolation, using `virglrenderer-rs`
bindings
([vhost-device-gpu](https://crates.io/crates/vhost-device-gpu),
[rutabaga_gfx](https://crosvm.dev/doc/rutabaga_gfx/index.html)). A separate backend
process is the right security posture — the GPU arbiter is a large attack surface and we
want it out of the VMM address space.

## 2. The API-remoting model in detail

**Intercept → serialize → replay.** In the guest, a driver/ICD/layer intercepts the
graphics API and *serializes* each call into a compact wire format; the stream crosses
the transport; the host *decodes* and *replays* it against a real GPU driver.

- **Venus** (Vulkan): the guest Mesa `venus` driver encodes Vulkan commands into a ring
  shared with the host; the host **virglrenderer** decodes and calls a real host Vulkan
  driver. Encoders/decoders are **code-generated** from the venus-protocol spec —
  `vn_protocol_driver_*` in Mesa, `vn_protocol_renderer_*` in virglrenderer — so adding
  a Vulkan entrypoint is a codegen change, not hand-written marshalling
  ([Venus docs](https://docs.mesa3d.org/drivers/venus.html),
  [Collabora Venus](https://www.collabora.com/news-and-blog/blog/2022/10/19/a-look-at-vulkan-extensions-in-venus/)).
  Venus is a *thin* layer — near-native because it just maps handles, not re-implements
  the API — but it **requires blob resources** and, per Mesa's own docs, "violates the
  spec and relies on implementation-defined behaviors," so it is tightly coupled to
  specific host driver versions
  ([Venus docs](https://docs.mesa3d.org/drivers/venus.html)). Notably virglrenderer's
  Venus path is tested against the **NVIDIA proprietary driver** as a host backend — a
  direct proof point that API-remoting onto an NVIDIA host works without vGPU
  ([Venus docs](https://docs.mesa3d.org/drivers/venus.html)).
- **VirGL** (OpenGL): translates GL → an intermediate stream → host GL. Heavier: work is
  done *twice* (guest + host), and the host decodes all guests on effectively one thread,
  so multiple GL apps collapse each other's performance
  ([Collabora 2025](https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/)).
- **gfxstream** (GLES/Vulkan, Google): auto-generated encoders/decoders ("Cereal"), a
  **1:1 thread model** (each guest encoder thread gets a host decoder thread), a
  `ResourceTracker` mapping guest↔host handles, and an io_uring-style command ring
  ([gfxstream README](https://android.googlesource.com/platform/hardware/google/gfxstream/+/fbc9e43e236777dacf23c0d4bf71dc414df984a9/README.md)).
  The 1:1 threading is the key scalability fix over VirGL's single decode thread.
- **cross-domain** (Wayland passthrough) is orthogonal — display integration, not the 3D API.

**Contrast — DRM native context.** Instead of mediating the *high-level* API, mediate the
*low-level kernel UAPI*: the guest presents a virtual GPU that looks like the real host
GPU to Mesa, and only the ioctl stream is forwarded. Less CPU overhead, less code, but it
needs a bespoke guest+host driver *per GPU family*, and the upstreamed ones are
Freedreno / AMDGPU / Intel / Asahi — **there is no NVIDIA native context**
([QEMU native context v12](https://lists.nongnu.org/archive/html/qemu-devel/2025-05/msg05596.html),
[AMDGPU native context](https://www.phoronix.com/news/AMDGPU-VirtIO-Native-Mesa-25.0)).
For NVIDIA, API-remoting (Venus-style) is the only tractable route because the host
driver is a closed blob we can only reach through Vulkan/GL/CUDA.

**The genuinely hard parts** (be blunt — these are where prototypes die):
1. **State & resource lifetime tracking.** Every guest handle (buffer, image, pipeline,
   descriptor set, fence) needs a host twin, created/destroyed in the right order, and
   cleaned up if a guest crashes mid-stream. `ResourceTracker`-style bookkeeping is the
   bulk of the code.
2. **Synchronization / fences across the VM boundary.** Guest fence → host GPU work →
   dma-fence signal → guest wakeup, without deadlock. The helix.ml multi-desktop writeup
   hit exactly this: a global `renderer_blocked` semaphore froze *all* contexts when one
   display lagged; FIFO command queues blocked blob-unmaps behind later commands
   ([helix.ml](https://blog.helix.ml/p/gpu-virtualization-architecture-for)).
3. **Presentation latency** (see §3).
4. **Windows guests are the hard wall.** There is *no* production virtio-gpu 3D guest
   driver for Windows; gfxstream's own docs note Windows guest support is incomplete
   because virtio-gpu guest drivers are missing
   ([gfxstream/Windows](https://deepwiki.com/arehnman/virtio-win-mesa/7.7-virtualized-and-layered-drivers-(virtio-gpu-venus-gfxstream))).
   Microsoft's WDDM **GPU-PV** does exactly what we want (marshal D3D/dxgkrnl calls from
   guest to a host kernel driver over VMBus) but it is **Hyper-V/VMBus-only** and not
   available to KVM/QEMU guests
   ([MS GPU-PV](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization)).
   Delivering D3D on a Windows KVM guest means writing our own WDDM user-mode + kernel-mode
   driver pair that serializes DXGI/D3D onto our transport — a multi-quarter effort. **NEEDS
   VERIFICATION** on the exact WDDM version floor for a paravirtual adapter.

## 3. Presentation path (framebuffer → guest scanout → console)

The replayed frame lives in a host GPU image; it must appear on the guest's virtual
scanout *and* on Infinibay's SPICE/VNC console. With blob resources the swapchain image
is backed by a **shared dma-buf**: the guest issues `SET_SCANOUT_BLOB` + `RESOURCE_FLUSH`,
waits on the flush fence, and the host either (a) composites the dma-buf into its own UI
(QEMU GTK/SPICE) or (b) **imports it and encodes it** — the helix.ml stack captures the
GPU texture, blits to a surface, and H.264-encodes to stream to the client
([scanout import](https://lwn.net/Articles/998774/),
[helix.ml](https://blog.helix.ml/p/gpu-virtualization-architecture-for)). Synchronization
is by pinning the dma-buf during `prepare_fb` and releasing it on the flush fence so guest
and host never touch the buffer simultaneously
([blob sync](https://lore.kernel.org/qemu-devel/20210901211014.2800391-3-vivek.kasireddy@intel.com/T/)).
For Infinibay this dovetails with the **existing SPICE/VNC relay** (backend ports
6100-6119): the host arbiter's composited/encoded output is what we already know how to
ship to the browser. Latency budget is dominated by encode + one extra copy if we can't
keep the whole path zero-copy.

## 4. Intelligent multiplexing of one GPU across many VMs

Hard truth: **we do not schedule the SMs.** With API-remoting onto the closed NVIDIA
driver, each VM's replay is just another host process/Vulkan context, and the **GPU
firmware + driver time-slice contexts** for us. Ampere (post-Pascal) supports hardware
context-switch preemption; graphics has fine-grained (pixel/instruction-level) preemption
and compute has instruction-level preemption
([concurrency mechanisms](https://arxiv.org/pdf/2110.00459)). NVIDIA's own sharing knobs:
**time-slicing** (serial, round-robin, fair-ish, but adds context-switch jitter/latency
and no memory isolation) and **MPS** (kernels from multiple processes run *concurrently*,
higher SM utilization, but you **cannot assign priorities**)
([MPS/time-slicing](https://sagar-parmar.medium.com/demystifying-nvidia-mps-how-multi-process-service-improves-gpu-sharing-and-performance-9f633878318a),
[Ampere concurrency](https://arxiv.org/pdf/2110.00459)).

So our "intelligent" scheduling lives in the **host arbiter's submission policy**, layered
on top of the driver's own time-slicing:
- **Per-VM queue priority** via `VK_EXT_global_priority` / `VK_KHR_global_priority`, and
  distinct host contexts per VM so the driver can preempt between them.
- **Token-bucket / deficit throttling** in the arbiter: meter each VM's submitted GPU
  work (draw/dispatch count or measured GPU time via timestamps) and gate its ring
  consumption to enforce quotas and fairness — this is the software knob we fully control.
- **VRAM partitioning by admission control**: track each VM's allocations and *refuse*
  device-memory allocations past its quota. There is no hardware VRAM isolation without
  MIG, so isolation is only as strong as the arbiter — a VM can't see another's memory
  (separate contexts) but a buggy/greedy VM can starve VRAM if we don't cap it.
- The kernel **`drm_sched`** fair scheduler (CFS-inspired, now out of RFC) is the right
  *reference model* for our arbiter's fairness logic, even though on NVIDIA the actual
  hardware scheduling is firmware-side and opaque to us
  ([fair DRM scheduler](https://blogs.igalia.com/tursulin/fair-er-drm-gpu-scheduler/),
  [Phoronix](https://www.phoronix.com/news/Fair-DRM-Scheduler-Post-RFC)).

One arbiter process owning the single GPU device, with one host Vulkan context per guest,
is the safe topology: isolation between guests is enforced by separate contexts + our
admission control, not by hardware.

## 5. NVIDIA-specific host execution without proprietary mediation

We drive the A5000 from **host userspace Vulkan**, headless. NVIDIA's Vulkan supports
true offscreen rendering (no X/Wayland surface) and exports results as dma-buf via
external-memory extensions (`VK_KHR_external_memory_fd`, or EGL's
`EGL_MESA_image_dma_buf_export` on the GL path); Vulkan headless is cleaner on NVIDIA than
EGL/GL, which historically wanted X for multi-GPU
([EGL without X](https://developer.nvidia.com/blog/egl-eye-opengl-visualization-without-x-server/),
[headless Vulkan multi-GPU caveat](https://forums.developer.nvidia.com/t/headless-vulkan-with-multiple-gpus/222832)).

The **open kernel module** (`nvidia-open`) supports Turing-and-newer, i.e. our GA102
A5000, and is GSP-firmware-based; critically the **user-space components (Vulkan/GL/CUDA)
are byte-identical regardless of module flavor**, so our render server behaves the same on
open or proprietary modules
([open kernel modules](https://download.nvidia.com/XFree86/Linux-x86_64/595.58.03/README/kernel_open.html)).
The open module does **not** unlock vGPU either — which is fine, because our whole design
sidesteps vGPU. Net: the module choice is irrelevant to the data plane; we get a normal
Vulkan device and that's all we need.

## Recommended first prototype (concrete)

**Target: Linux guest → Linux host, Vulkan only, one GPU, two guests.**

Data plane
- **Transport:** our own virtio-gpu-*style* device (control ring + doorbell + fence),
  implemented as a **Rust vhost-user backend process** (model on `vhost-device-gpu` /
  `rutabaga_gfx`, but our code). Keeps the arbiter out of the VMM address space.
- **Zero-copy:** blob resources backed by udmabuf for the swapchain image; avoid
  `TRANSFER_*_HOST_3D` copies on the hot path.

Rendering
- **Guest:** a minimal Vulkan command **serializer** — start as a thin Vulkan layer/ICD
  that encodes a *subset* of Vulkan (enough for a compositor + one 3D app) onto the ring.
  Study Venus's codegen approach; generate encoders rather than hand-write.
- **Host arbiter:** decode the ring, maintain a `ResourceTracker` (guest handle → host
  Vulkan handle), replay against a **headless NVIDIA Vulkan** context (one per guest),
  signal the dma-fence on completion.

Presentation
- Blob-backed scanout dma-buf → host imports → encode (H.264/VideoToolbox-equivalent, or
  raw for v0) → **feed Infinibay's existing SPICE/VNC relay**.

Scheduling
- v0: rely on the NVIDIA driver's own context time-slicing; give each guest its own host
  context with `VK_EXT_global_priority`.
- v1: add a token-bucket throttle in the arbiter (meter GPU timestamps) + VRAM admission
  cap per guest to prove fairness/quota.

**Minimum viable slice (prove the loop before anything else):** *one* Linux guest, *one*
host GPU context, forward a **single Vulkan workload** (headless compute or one spinning
triangle) through *one* command ring with *one* fence, using *one* blob-backed image, and
present it once into the SPICE console. That end-to-end round trip —
serialize→transport→decode→replay→fence→present — is the whole risk. Only after it's solid
do we add the second VM and the arbiter's scheduler. **Defer Windows/D3D entirely** to a
later phase; it needs a custom WDDM driver pair and is an order of magnitude more work than
the Linux/Vulkan slice.

## Sources

- QEMU virtio-gpu device: https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html
- QEMU virtio-gpu options (v10.0.3): https://qemu.readthedocs.io/en/v10.0.3/system/devices/virtio-gpu.html
- virtio-gpu blob resources (kernel patch): https://patchwork.kernel.org/project/dri-devel/patch/20200814024000.2485-11-gurchetansingh@chromium.org/
- virtio-gpu blob synchronization (dma-fence): https://lore.kernel.org/qemu-devel/20210901211014.2800391-3-vivek.kasireddy@intel.com/T/
- drm/virtio scanout buffer import (LWN): https://lwn.net/Articles/998774/
- ivshmem spec: https://www.qemu.org/docs/master/specs/ivshmem-spec.html
- vfio-user protocol spec: https://www.qemu.org/docs/master/interop/vfio-user.html
- Mesa Venus driver docs: https://docs.mesa3d.org/drivers/venus.html
- Collabora — Venus Vulkan extensions: https://www.collabora.com/news-and-blog/blog/2022/10/19/a-look-at-vulkan-extensions-in-venus/
- Collabora — state of GFX virtualization (2025): https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/
- gfxstream README: https://android.googlesource.com/platform/hardware/google/gfxstream/+/fbc9e43e236777dacf23c0d4bf71dc414df984a9/README.md
- gfxstream/Venus/Windows layered drivers (DeepWiki): https://deepwiki.com/arehnman/virtio-win-mesa/7.7-virtualized-and-layered-drivers-(virtio-gpu-venus-gfxstream)
- rutabaga_gfx (Rust): https://crosvm.dev/doc/rutabaga_gfx/index.html
- vhost-device-gpu (Rust, rust-vmm): https://crates.io/crates/vhost-device-gpu
- QEMU virtio-gpu DRM native context (v12): https://lists.nongnu.org/archive/html/qemu-devel/2025-05/msg05596.html
- AMDGPU virtio native context merged (Phoronix): https://www.phoronix.com/news/AMDGPU-VirtIO-Native-Mesa-25.0
- Microsoft GPU paravirtualization (GPU-PV): https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization
- NVIDIA open GPU kernel modules README: https://download.nvidia.com/XFree86/Linux-x86_64/595.58.03/README/kernel_open.html
- NVIDIA EGL without X (headless GL): https://developer.nvidia.com/blog/egl-eye-opengl-visualization-without-x-server/
- Headless Vulkan multi-GPU caveat (NVIDIA forums): https://forums.developer.nvidia.com/t/headless-vulkan-with-multiple-gpus/222832
- Fair(er) DRM GPU scheduler (Igalia): https://blogs.igalia.com/tursulin/fair-er-drm-gpu-scheduler/
- Fair DRM scheduler post-RFC (Phoronix): https://www.phoronix.com/news/Fair-DRM-Scheduler-Post-RFC
- NVIDIA MPS deep dive: https://sagar-parmar.medium.com/demystifying-nvidia-mps-how-multi-process-service-improves-gpu-sharing-and-performance-9f633878318a
- NVIDIA time-slicing vs MIG deep dive: https://sagar-parmar.medium.com/beyond-partitioning-a-deep-dive-into-nvidia-gpu-time-slicing-533705821f0d
- Characterizing concurrency mechanisms for NVIDIA GPUs (arXiv): https://arxiv.org/pdf/2110.00459
- NVIDIA vGPU supported products matrix: https://docs.nvidia.com/vgpu/19.0/product-support-matrix/index.html
- Helix — GPU virtualization for multi-desktop containers: https://blog.helix.ml/p/gpu-virtualization-architecture-for
- DRM GPU scheduler source (sched_main.c): https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/scheduler/sched_main.c
