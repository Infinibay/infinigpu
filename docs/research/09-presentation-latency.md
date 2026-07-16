# 09 — Presentation Path & Latency Budget (render → scanout → console)

**Scope:** the rendered frame lives in a host GPU image (a Vulkan `VkImage` produced by
the arbiter replaying a guest's stream). It must reach two consumers: **(a)** the guest's
own virtual scanout (so the guest OS compositor believes it has a display) and **(b)**
Infinibay's console — today a SPICE/VNC relay shipping pixels to the user. This doc pins
the zero-copy transport, the encoder reality on the A5000, the exact Infinibay touch-point,
and an honest end-to-end latency budget, then recommends a Phase-0 (prove-the-loop) and
Phase-1 (low-latency) pipeline.

## Verdict

**PARTIALLY-CONFIRMED.** The Wave-1 claim — *blob dma-buf scanout → host import → encode →
feed Infinibay's existing SPICE/VNC relay* — holds in its load-bearing parts but needs two
corrections. (1) The zero-copy export chain is real and NVENC is **license-free with no
session cap on the RTX A5000** (a *pro* card is a "qualified" GPU — this actually
*strengthens* the design). (2) But NVENC does **not** read a Vulkan image natively — it
needs a Vulkan→CUDA external-memory hop — and "feed the existing relay" is only true for
the Phase-0 reuse path; the low-latency path **bypasses** SPICE with a direct hardware-encoded
stream. The existing "relay" is a **dumb TCP tunnel**, not a framebuffer sink, so Phase 1 adds
a new touch-point rather than feeding the old one.

## 1. One buffer, two destinations

The zero-copy primitive is the **blob resource** (Wave-1 doc 06): the guest's swapchain
image is a blob whose host storage is a single dma-buf the arbiter owns. When the guest
finishes a frame it issues `VIRTIO_GPU_CMD_SET_SCANOUT_BLOB` to name that blob as the
active scanout, then `RESOURCE_FLUSH` and waits on a dma-fence
([QEMU virtio-gpu](https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html),
[blob scanout via dmabuf fd, QEMU v15 patch](https://patchwork-proxy.ozlabs.org/project/qemu-devel/patch/20240622215511.154763-11-dmitry.osipenko@collabora.com/)).
Because the **same** dma-buf is what the arbiter also hands to the console encoder, one
buffer serves both the guest scanout *and* the host console — no extra copy to "get the
frame out." That is the whole point of blobs.

**Tearing avoidance is fence-mediated, not lock-mediated.** The device *pins* the dma-buf
when it begins a scanout/flush (QEMU's `prepare_fb` equivalent) and *releases* it only when
the flush dma-fence signals; the guest does not recycle or overwrite the buffer until it
observes that fence. So guest write and host read never overlap on the same buffer
generation ([blob sync / dma-fence,
Kasireddy](https://lore.kernel.org/qemu-devel/20210901211014.2800391-3-vivek.kasireddy@intel.com/T/)).
For double/triple buffering the guest simply owns N blobs and rotates; the fence gates reuse.

## 2. Zero-copy from Vulkan image to encoder input — with the catch

Exporting the host image is standard and works on NVIDIA: allocate the `VkImage`/`VkDeviceMemory`
with `VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT` (or a DRM-modifier handle) and call
`vkGetMemoryFdKHR` to get a POSIX fd via **`VK_KHR_external_memory_fd`**
([Vulkan external memory guide](https://docs.vulkan.org/guide/latest/extensions/external.html),
[VK_KHR_external_memory_fd ref](https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_external_memory_fd.html)).
That fd is the dma-buf the console consumer imports.

**The catch that Wave-1 glossed:** NVENC's input surface types are **CUDA device
pointer / CUDA array / DirectX / OpenGL texture — there is no `VK_IMAGE`
input type.** FFmpeg's NVENC path confirms this: for `AV_PIX_FMT_CUDA` it registers
`NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR`
([FFmpeg nvenc.c](https://ffmpeg.org/doxygen/6.1/nvenc_8c_source.html),
[NVIDIA FFmpeg/NVENC guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/ffmpeg-with-nvidia-gpu/index.html)).
So the real chain is:

```
VkImage --vkGetMemoryFdKHR--> dma-buf fd --cuImportExternalMemory-->
   CUDA external memory --cuExternalMemoryGetMapped{Buffer,MipmappedArray}-->
   CUdeviceptr/CUarray --NvEncRegisterResource(CUDADEVICEPTR)--> NVENC
```

This is a **handle-import, not a pixel-copy** — still zero-copy of data. Two frictions are
real and should be budgeted: **(i)** CUDA↔Vulkan external-memory interop is fussy about
tiling/modifiers — NVIDIA's own forum notes the shared image often must be
`VK_IMAGE_TILING_LINEAR` (or described precisely as an array) to map cleanly into CUDA
([Vulkan/CUDA tiling caveat](https://forums.developer.nvidia.com/t/does-vulkan-cuda-interop-only-work-with-vk-image-tiling-optimal-vulkan-image-linux/236523),
[Vulkan-CUDA interop walkthrough](https://medium.com/@mikolaj.gucki/vulkan-cuda-memory-interoperability-5442f3b43c3d));
**(ii)** NVENC input is usually NV12, but the SDK accepts **ARGB/ABGR and converts to YUV in
fixed-function hardware** — the NVENC Application Note lists "Encoding support ARGB content = Y"
across all architectures
([NVENC Application Note](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-application-note/index.html)).
So our RGBA swapchain image can go straight in; the RGB→NV12 CSC is inside the encoder's
cost, not a separate blit. Net: **the "zero-copy to encode" claim survives, but through a
CUDA interop shim, not a native Vulkan feed.** (If we ever prefer to skip CUDA, Vulkan Video
encode extensions exist and *are* native to `VkImage`, but NVIDIA's Vulkan-Video-encode
maturity is worse than NVENC — **NEEDS VERIFICATION** before betting on it.)

## 3. Encoding: NVENC is the right, license-free choice on the A5000

**Session limits — the pro card wins.** The classic "3 concurrent NVENC sessions" cap is a
*driver* limit on **consumer GeForce** only, since raised to 8
([Tom's Hardware](https://www.tomshardware.com/news/nvidia-increases-concurrent-nvenc-sessions-on-consumer-gpus),
[TechPowerUp](https://www.techpowerup.com/268495/nvidia-silently-increases-geforce-nvenc-concurrent-sessions-limit-to-3)).
The NVENC Application Note splits GPUs into **"qualified"** (Quadro/professional/datacenter —
concurrency limited only by *hardware resources*) vs **"non-qualified"** (GeForce — capped
at 8/system)
([NVENC Application Note](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-application-note/index.html)).
The **RTX A5000 is a professional Ampere card → "qualified" → no driver session cap**, so we
do **not** need the `keylase/nvidia-patch` DLL hack that consumer setups use
([nvidia-patch](https://github.com/keylase/nvidia-patch)). This is a genuine advantage of the
target hardware for a many-VM VDI host: dozens of console streams are license-free, limited
only by the GA102's two NVENC blocks' throughput (practical ceiling is a handful of 4K60
streams or many more 1080p30 desktop streams, per the NVIDIA dev forum's own "A6000 hard-pressed
past 4× 4K60" caveat) ([RTX A5000/A6000 NVENC forum](https://forums.developer.nvidia.com/t/rtx-a5000-a6000-simultaneous-nvenc-sessions/245252)).
**NEEDS VERIFICATION:** the A5000 SKU on NVIDIA's exact current "qualified" list, but every
secondary source treats the whole pro Ampere line as unrestricted.

**Latency — encode is *not* the bottleneck.** Measured NVENC encode time for 1080p60 H.265
in ultra-low-latency config (CBR, no B-frames, single-frame VBV) is **~1–3 ms on Ada, and
similar low-single-digit ms on Ampere**
([Remio HW encoder comparison](https://remio.net/blog/hardware-encoder-comparison)). The
scary "100 ms" figures floating around forums are an **FFmpeg pipeline artifact** — surface
queue depth / `async_depth` / lookahead buffering 6–7 frames — not the encoder's intrinsic
per-frame cost; set `delay=0`, `async_depth=1`, no lookahead and it collapses to the ~ms
range ([NVENC HEVC ULL expectations,
NVIDIA forum](https://forums.developer.nvidia.com/t/nvenc-hevc-ultra-low-latency-with-ffmpeg-libraries-what-should-be-my-expectations/143954)).

**vs alternatives.** VA-API/QuickSync (2–4 ms) and Apple VideoToolbox (2–3 ms) are in the
same class but irrelevant here — we *have* NVENC on the render GPU, so encoding on the same
device the frame already lives on avoids a cross-device copy. Software x264 `ultrafast` is
~8–12 ms/frame *and* burns CPU we want for the arbiter
([Remio comparison](https://remio.net/blog/hardware-encoder-comparison)); it stays as the
**no-KVM / no-NVENC fallback** only. NVENC reading the CUDA-imported image directly keeps the
whole encode on-GPU.

## 4. The Infinibay touch-point — what the relay actually is

I read the code. Infinibay's console is **`backend/app/services/console/SpiceProxyService.ts`** —
a **transparent TCP relay**, not a pixel pipeline. `tryListen()` does literally
`client.pipe(upstream); upstream.pipe(client)` between a client-facing port
(`SPICE_PROXY_PORT_MIN/MAX`, default **6100–6199** in code; CLAUDE.md's 6100–6119 is stale)
and QEMU's SPICE/VNC server port. It allocates one listener per VM, caps fan-out, and idles
out — it does **zero** encoding or framebuffer handling. The pixels are produced and encoded
by **QEMU's built-in SPICE/VNC server**; the frontend (`frontend/src/utils/spiceConnect.js`)
just hands the user a `.vv` file that launches a **native** `remote-viewer`/virt-viewer
pointed at the relay port. VNC uses QEMU's `-vnc … -vga std` (`infinization/src/display/VncConfig.ts`).

That reframes the integration:

- **To reuse the relay unchanged (Phase 0):** our device must present a scanout that
  **QEMU's own display/SPICE path** can see. A vhost-user-gpu-style device already plugs into
  QEMU's `dpy_gl_scanout_dmabuf` / `vg_send_scanout_dmabuf`, and QEMU forwards the dma-buf to
  spice-server, which streams it
  ([QEMU egl-headless dmabuf](https://github.com/qemu/qemu/blob/master/ui/egl-headless.c),
  [spice opengl/virgl/dmabuf](https://lists.gnu.org/archive/html/qemu-devel/2016-02/msg05344.html)).
  Over the network (our case — the viewer is remote, behind the TCP tunnel), spice-server
  **reads the surface back and re-encodes** via its GStreamer video encoder (H.264/VP8/VP9,
  configured `tune=zerolatency`, `speed-preset` realtime, intra-refresh)
  ([SPICE GStreamer H.264](https://lists.freedesktop.org/archives/spice-devel/2016-March/026936.html)).
  So Phase 0 costs a GPU→sysmem readback + CPU/VA-API encode, but touches **no Infinibay code**.

- **For low-latency 3D (Phase 1):** do **not** feed SPICE. The arbiter already holds the
  dma-buf; export→CUDA→**NVENC** on the same GPU and emit an H.264/AV1 elementary stream to a
  **browser** client (WebCodecs `<video>` / WebRTC). This is a **new** sibling service in
  `backend/app/services/console/` (call it an *encoded console stream* relay) plus a browser
  player replacing the `.vv` download. It reuses the port-allocation/idle/auth scaffolding of
  `SpiceProxyService` but carries an encoded video stream, not the SPICE protocol. This is the
  correction to Wave-1: you don't "feed the existing relay," you add a parallel encoded path.

## 5. Cursor, multi-monitor, pacing

- **Cursor** rides virtio-gpu's dedicated **cursor virtqueue** (`UPDATE_CURSOR`/`MOVE_CURSOR`)
  — a hardware-cursor plane, so pointer motion updates at input latency **without re-encoding
  the frame**; SPICE likewise has a separate cursor channel. Keep the cursor off the encoded
  video plane in both phases — re-encoding for cursor movement is a classic VDI latency sin.
- **Multi-monitor** = multiple virtio-gpu **scanouts** (spec allows up to 16); each is its own
  blob + its own encode session. On the "qualified" A5000 the extra sessions are free.
- **vsync / pacing.** Do **not** hard-lock the guest to host vblank. Present-on-flush, and let
  the arbiter **pace the encoder to the client's refresh** (e.g. cap 60 fps) and **skip
  unchanged frames** using dirty regions — the single biggest win for desktop VDI, and exactly
  what SPICE already does (dirty-rect diffs, promote moving regions to a video stream).

## 6. The common case: pure 2D desktop (no 3D app)

Most VDI frames have **no** 3D app — it's a desktop compositor. This path must be cheap:

- **Linux guest:** virtio-gpu **2D** (dumb GEM buffers) → `SET_SCANOUT` of guest pages wrapped
  in **udmabuf**; the host reads/encodes directly. No Vulkan replay at all for a static desktop.
- **Windows guest:** the **IddCx** indirect-display driver (Wave-1 doc 03's early milestone)
  hands Windows a swapchain of desktop frames; our device presents them into the same
  dma-buf→encode pipeline — **pixels only, zero in-guest 3D**, which is precisely the 2D case.
- Because the desktop is mostly static, **damage tracking + frame-skip dominate**: encode only
  changed tiles, drop cadence when idle. Here Phase-0's reuse-SPICE approach is not a
  compromise — SPICE is *built* for desktop diffing and is arguably better than naive
  full-frame NVENC for a mostly-static screen. Full-frame hardware NVENC (Phase 1) wins for the
  **3D/video** workload, not the idle desktop.

## 7. End-to-end interactive latency budget (1080p60, LAN)

| Stage | Cost (LAN) | Notes |
|---|---|---|
| Client input → host | 1–5 ms | one-way LAN; WAN adds RTT/2 |
| Guest render next frame | up to ~16.6 ms | the frame boundary; app-dependent, usually the biggest term |
| Guest→host replay + `RESOURCE_FLUSH` + fence | 1–5 ms | ring submit + arbiter replay; heavier for complex 3D |
| Export→CUDA import→**NVENC** | 2–5 ms | Ampere ~single-digit ms incl. RGB→NV12 CSC |
| Host→client network | 1–5 ms | one-way LAN |
| Client jitter buffer | 5–15 ms | tunable; the honest cost of smoothness |
| Client HW decode | 2–5 ms | WebCodecs/HW decoder |
| Client compositor + display | 8–16 ms | ≥ one refresh interval |
| **Motion-to-photon total** | **~40–70 ms LAN** | matches tuned Moonlight/Parsec on LAN |

**Skeptic's takeaway:** encode is ~2–5 ms of a ~40–70 ms budget. **Frame cadence, jitter
buffer, and client display dominate** — so NVENC's few-ms latency is *not* where the loop
lives or dies, and a **raw/uncompressed Phase 0 is perfectly fine for correctness** (1080p60
RGBA ≈ 4 Gbit/s — fine on localhost/10GbE to prove the pipe, unusable for real deploy). Get the
buffer/fence/present *correct* first; optimize bytes later.

## 8. Recommended pipeline

**Phase 0 — prove the loop (reuse everything).**
Present the arbiter's blob dma-buf through a **vhost-user-gpu-style scanout into QEMU's own
display path**; let QEMU's SPICE/VNC server encode; ship it over the **unchanged**
`SpiceProxyService` TCP tunnel to the existing `.vv` native viewer. For the very first slice,
even simpler: uncompressed RGBA over the fence, one blob, one scanout, localhost. **Zero new
Infinibay relay code.** Validates `SET_SCANOUT_BLOB` → `RESOURCE_FLUSH` → dma-fence → present.

**Phase 1 — low-latency encoded (new touch-point).**
Arbiter exports the same dma-buf → `cuImportExternalMemory` → **NVENC** (license-free on the
A5000, ARGB input, ULL preset, `delay=0`, CBR, no B-frames) → a **new `console` sibling
service** streaming H.264/AV1 over WebSocket/WebRTC to a **browser player** (WebCodecs),
reusing `SpiceProxyService`'s port/idle/auth scaffolding. Add damage-tracked frame-skip,
per-VM encoder pacing, and a HW-cursor plane. Keep **software x264 (zerolatency)** as the
no-KVM/no-NVENC fallback, and keep the Phase-0 SPICE path as the **2D-desktop-optimized**
option for idle sessions.

## Sources

- QEMU virtio-gpu device: https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html
- virtio-gpu blob scanout via dmabuf fd (QEMU v15 patch): https://patchwork-proxy.ozlabs.org/project/qemu-devel/patch/20240622215511.154763-11-dmitry.osipenko@collabora.com/
- virtio-gpu blob sync / dma-fence (Kasireddy): https://lore.kernel.org/qemu-devel/20210901211014.2800391-3-vivek.kasireddy@intel.com/T/
- Vulkan external memory & synchronization guide: https://docs.vulkan.org/guide/latest/extensions/external.html
- VK_KHR_external_memory_fd reference: https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_external_memory_fd.html
- Vulkan↔CUDA memory interoperability walkthrough: https://medium.com/@mikolaj.gucki/vulkan-cuda-memory-interoperability-5442f3b43c3d
- Vulkan/CUDA interop tiling caveat (NVIDIA forum): https://forums.developer.nvidia.com/t/does-vulkan-cuda-interop-only-work-with-vk-image-tiling-optimal-vulkan-image-linux/236523
- FFmpeg nvenc.c (CUDADEVICEPTR input type): https://ffmpeg.org/doxygen/6.1/nvenc_8c_source.html
- NVIDIA FFmpeg with GPU (hwaccel cuda / zero-copy): https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/ffmpeg-with-nvidia-gpu/index.html
- NVENC Application Note (session limits, ARGB input): https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-application-note/index.html
- Tom's Hardware — NVIDIA lifts NVENC session limits on consumer GPUs: https://www.tomshardware.com/news/nvidia-increases-concurrent-nvenc-sessions-on-consumer-gpus
- TechPowerUp — GeForce NVENC concurrent sessions to 3: https://www.techpowerup.com/268495/nvidia-silently-increases-geforce-nvenc-concurrent-sessions-limit-to-3
- keylase/nvidia-patch (consumer session unlock): https://github.com/keylase/nvidia-patch
- RTX A5000/A6000 simultaneous NVENC sessions (NVIDIA forum): https://forums.developer.nvidia.com/t/rtx-a5000-a6000-simultaneous-nvenc-sessions/245252
- Remio — hardware video encoder comparison (NVENC/AMF/QSV/VT/x264 latency): https://remio.net/blog/hardware-encoder-comparison
- NVENC HEVC ultra-low-latency expectations (FFmpeg buffering pitfall): https://forums.developer.nvidia.com/t/nvenc-hevc-ultra-low-latency-with-ffmpeg-libraries-what-should-be-my-expectations/143954
- QEMU egl-headless dmabuf source: https://github.com/qemu/qemu/blob/master/ui/egl-headless.c
- QEMU spice opengl/virgl/dmabuf support: https://lists.gnu.org/archive/html/qemu-devel/2016-02/msg05344.html
- SPICE GStreamer H.264 encoder (zerolatency): https://lists.freedesktop.org/archives/spice-devel/2016-March/026936.html
- Moonlight/Sunshine streaming FAQ (host latency): https://github.com/moonlight-stream/moonlight-docs/wiki/Frequently-Asked-Questions
- Infinibay relay (read in-repo): backend/app/services/console/SpiceProxyService.ts, frontend/src/utils/spiceConnect.js, infinization/src/display/VncConfig.ts
