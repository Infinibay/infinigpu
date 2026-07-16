# 21 — Cross-Vendor Media Codec (encode/decode abstraction for the remote datapath)

**Scope:** the media encode/decode block is the single biggest vendor-specific seam in the
stack. NVENC/NVDEC are NVIDIA-only; the whole point of the API-remoting core (doc 02, doc 06)
is that the guest never sees the physical GPU, so the *encoder* must not smuggle a vendor
dependency back into the design. This doc surveys the 2026 options per vendor, recommends a
`MediaCodec` trait with a probe-driven backend selector, pins the zero-copy story per vendor,
generalizes the guest decode-offload path (doc 17) beyond NVDEC, and defines codec negotiation
so the remote protocol (doc 18) is vendor-agnostic. It builds directly on doc 09's finding that
the frame already lives in a host `VkImage` and that the SPICE readback is the thing to avoid.

## Verdict

**CONFIRMED, with one hardware caveat for our test rig.** Vulkan Video (`VK_KHR_video_encode_queue` /
`video_decode_queue`) is the right cross-vendor default: it runs on the *same `VkDevice`* the
arbiter renders on — **zero interop hop, no CUDA, no dma-buf export** — and as of 2026 all three
open/vendor drivers ship it. VA-API is the correct broad fallback (dma-buf import is standard and
well-trodden). Vendor-native (NVENC first, since we test on the A5000) stays an *optional optimized*
backend. **The caveat:** our RTX A5000 is Ampere (GA102), which has **no AV1 hardware encoder** — AV1
encode arrived on NVIDIA only with Ada (RTX 40)
([NVIDIA Ada AV1 blog](https://developer.nvidia.com/blog/improving-video-quality-and-performance-with-av1-and-nvidia-ada-lovelace-architecture/)).
So on the A5000 we encode **H.264 + HEVC only**, and because NVIDIA's Vulkan-Video-*encode* is
newer/less battle-tested than NVENC, NVENC-via-CUDA-interop remains the pragmatic bring-up encoder
even though Vulkan Video is the architectural default. That is a *backend selection* difference, not
a rearchitecture — exactly what the trait is for.

## 1. The three option classes, per vendor, 2026 status

### (a) Vulkan Video — `VK_KHR_video_{encode,decode}_queue`
Cross-vendor by construction and native to `VkImage`, so it composes with our Vulkan render/replay
with no memory export. Encode was finalized for H.264/H.265 in Dec 2023; **AV1 encode**
(`VK_KHR_video_encode_av1`) landed in Vulkan 1.3.302 (Nov 2024), plus an **intra-refresh** extension
(Jul 2025) and **quantization-map** extension that matter for low-latency VDI
([Khronos AV1 encode](https://www.khronos.org/blog/khronos-announces-vulkan-video-encode-av1-encode-quantization-map-extensions),
[Khronos AV1 decode + H.264/5 encode SDK](https://www.khronos.org/blog/khronos-releases-vulkan-video-av1-decode-extension-vulkan-sdk-now-supports-h.264-h.265-encode)).
Per-driver support (Igalia's tracker, cross-checked against Phoronix):

| Driver | H.264 enc/dec | HEVC enc/dec | AV1 enc/dec | Notes |
|---|---|---|---|---|
| **NVIDIA** (proprietary) | ✅ / ✅ (Linux 535.43.22 / 525.47) | ✅ / ✅ | ✅ (Linux 550.40.80) / ✅ (535.43.24) | AV1 **encode** needs Ada+ silicon; Ampere = decode only |
| **AMD RADV** (Mesa) | ✅ (24.1) / ✅ (23.1.2) | ✅ (24.1) / ✅ | ✅ (25.2) / ✅ (24.0.3) | default for VCN2+ since Mesa 25; AV1 enc needs VCN4 (RDNA3/RX 7000) |
| **Intel ANV** (Mesa) | ✅ (24.3) / ✅ | ✅ (24.3) / ✅ | ❌ via Vulkan / ✅ (25.0) | AV1 **encode** only via oneVPL, not Vulkan yet |

Sources: [Igalia Vulkan Video status](https://blogs.igalia.com/vjaquez/vulkan-video-status/),
[RADV AV1 encode in Mesa 25.2](https://www.phoronix.com/news/RADV-Merges-AV1-Encode),
[Intel ANV AV1 decode on Battlemage/Lunar Lake](https://www.phoronix.com/news/Intel-ANV-Vulkan-AV1-Decode).
One honesty note on Intel: Vulkan encode was **disabled on Gen12.5+ ANV** for a while over
insufficient testing and **re-enabled for H.264/H.265 on Alchemist (Arc A-series) around Mesa 26.2**
([Intel ANV Gen12.5 H.265 encode](https://www.phoronix.com/news/Intel-ANV-Gen125-H265-Encode)) — so
treat ANV Vulkan encode as "works on Alchemist, verify on Battlemage." **NEEDS VERIFICATION** on the
exact driver you deploy per SKU.

### (b) VA-API — the Linux cross-vendor media API
The de-facto Linux standard, and the natural fallback. Intel is native (QSV via the media-driver /
oneVPL runtime); AMD encodes/decodes via Mesa's radeonsi VA-API on VCN2+; **NVIDIA is decode-only**
via `nvidia-vaapi-driver` (NVDEC backend) — encode "is unlikely to ever work" because VA-API doesn't
hand NVDEC enough of the bitstream ([nvidia-vaapi-driver README](https://github.com/elFarto/nvidia-vaapi-driver/blob/master/README.md),
[NVIDIA VA-API 0.0.17](https://ubuntuhandbook.org/index.php/2026/05/nvidia-va-api-driver-0-0-17/)). So
VA-API is a **great fallback encoder on AMD/Intel, a decode-only fallback on NVIDIA**. Note also that
**VDPAU was removed from the Mesa open drivers in 25.3** (radeonsi, nouveau, virtio_gpu, r600, d3d12)
([ArchWiki HW video accel](https://wiki.archlinux.org/title/Hardware_video_acceleration)) — VA-API is
the go-forward Linux path; do not target VDPAU on the guest side.

### (c) Vendor-native — optimized backends
- **NVIDIA NVENC/NVDEC** (via CUDA interop): the mature, license-free path on our A5000 (a "qualified"
  pro card → no session cap, per doc 09). ARGB input accepted, RGB→NV12 in fixed function. This is our
  bring-up encoder.
- **AMD AMF**: on Linux runs with AMD Pro Vulkan and (experimentally) RADV; AV1 encode from VCN4
  (RX 7000), with quality materially improved on VCN5. Notably, **AMF is increasingly a *frontend* over
  the Vulkan Video path** on AMD ([GPUOpen AMF](https://gpuopen.com/advanced-media-framework/)) —
  **NEEDS VERIFICATION**, but if true it means "use Vulkan Video on AMD" and "use AMF" converge, and we
  should just use Vulkan Video directly.
- **Intel oneVPL / Intel VPL** (successor to the dead Media SDK): the *primary* path for Intel **AV1
  encode** (Arc DG2 Gen12.5+, Meteor/Lunar/Battlemage), since Vulkan-Video AV1 encode isn't wired on
  ANV yet ([Intel VPL codecs](https://www.intel.com/content/www/us/en/developer/tools/vpl/overview.html)).

## 2. Recommended abstraction — the `MediaCodec` trait

Default **Vulkan Video** (cross-vendor + zero interop with our renderer + no CUDA dependency), fall
back to **VA-API** (broad Linux HW), and allow a vendor-native **optimized** backend (NVENC first)
where it beats Vulkan Video on maturity or codec coverage. Everything hides behind one trait whose
currency is a *GPU-resident frame that never touches host RAM*:

```rust
/// A frame that lives on the GPU. Backends consume/produce it without host readback.
pub struct GpuFrame {
    pub width: u32,
    pub height: u32,
    pub fourcc: DrmFourcc,          // NV12 / P010 / ARGB8888 …
    pub modifier: u64,              // DRM format modifier (tiling) — MUST be respected on import
    pub planes: SmallVec<[DmaBufPlane; 3]>, // 1 plane if same-VkDevice; ≥1 dmabuf fd if imported
    pub acquire: FrameSync,         // timeline semaphore / dma-fence to wait before the backend reads
}
pub struct DmaBufPlane { pub fd: Option<OwnedFd>, pub offset: u32, pub pitch: u32 }

pub enum Codec { H264, H265, Av1 }

pub struct EncodeConfig {
    pub codec: Codec,
    pub rate_control: RateControl,  // CBR ultra-low-latency by default (doc 09)
    pub bitrate_kbps: u32,
    pub gop: Gop,                   // no B-frames; prefer intra-refresh for ULL
    pub max_refs: u8,
}

pub struct CodecCaps {
    pub encode: EnumSet<Codec>,
    pub decode: EnumSet<Codec>,
    pub max_dim: (u32, u32),
    pub zero_copy: ZeroCopy,        // SameDevice | ExternalMemoryCuda | DmaBufImport
}

/// One live encoder+decoder bound to one host GPU.
pub trait MediaCodec: Send {
    fn caps(&self) -> &CodecCaps;
    /// Encode a GPU-resident frame → Annex-B / OBU bitstream. MUST NOT copy to host memory.
    fn encode(&mut self, frame: &GpuFrame, cfg: &EncodeConfig) -> Result<Bitstream>;
    /// Decode one access unit → GPU-resident surface, kept on-device for compositing + re-encode.
    fn decode(&mut self, au: &[u8], codec: Codec) -> Result<GpuFrame>;
}

/// A backend factory. Selection probes these against a real host GPU at startup.
pub trait CodecBackend {
    fn name(&self) -> &'static str;
    fn priority(&self) -> u8;                       // tie-breaker; higher wins
    fn probe(gpu: &HostGpu) -> Option<Box<dyn MediaCodec>>;  // None = not usable on this GPU
}
```

**Selection logic (probe at startup, per host GPU / DRM render node):**

1. Enumerate host GPUs. For each, register the backend list in priority order:
   `VulkanVideo` → `Nvenc` → `OneVpl` → `Amf` → `VaApi` → `SoftwareX264/SvtAv1` (last resort).
2. `probe()` each in order. Vulkan Video's probe calls `vkGetPhysicalDeviceVideoCapabilitiesKHR`
   for each `VkVideoProfile` (H.264/H.265/AV1 × encode/decode) and only claims a codec the driver
   *actually* advertises — this is what makes "Ampere has no AV1 encode" a **discovered fact**, not a
   hard-coded assumption. NVENC's probe checks for a CUDA context + `nvEncGetEncodeCaps`.
3. Take the first backend that yields a `MediaCodec`, but **merge codec caps across backends on the
   same GPU** where zero-copy allows: e.g. on an Arc GPU, `encode = {H264,H265}` from Vulkan Video
   ∪ `{AV1}` from oneVPL, all fed from the same dma-buf. The selector emits a per-GPU `CodecCaps` the
   negotiator (§5) advertises.
4. Policy override: on **NVIDIA**, prefer `Nvenc` over `VulkanVideo` for *encode* until NVIDIA's
   Vulkan-encode maturity is verified (the NVIDIA sample still had POC/corrupt-frame issues in 2025 —
   [3dverse "Vulkan encoder (scary)"](https://docs.3dverse.com/devlog/2025/04/25/making-a-vulkan-encoder));
   on **AMD/Intel**, prefer `VulkanVideo` (RADV is complete; AMF sits on it anyway). This is one table,
   not scattered `#[cfg]`s.

The result: adding a vendor = writing one `CodecBackend` impl + one entry in the priority table. No
caller of `MediaCodec::encode` changes.

## 3. Zero-copy story per backend — keeping the frame on-GPU

Doc 09's central finding: the SPICE path does a GPU→sysmem **readback + CPU re-encode**, and that is
the latency/throughput killer. Every backend below avoids it; they differ only in *how many handle
hops* stand between the rendered `VkImage` and the encoder input.

- **Vulkan Video (default) — zero hops.** The encode queue is a queue on the **same `VkDevice`** that
  rendered the frame. Input image and DPB are `VkImage`s; the render/compositor submission and the
  video-encode submission are ordered by a **pipeline barrier + timeline semaphore** — no fd export, no
  driver crossing, no import. `GpuFrame::planes` carries a single in-device image handle and
  `acquire` is the timeline value. This is the whole reason Vulkan Video is the default: it is the only
  option with **literally no interop**.
- **NVENC (NVIDIA optimized) — one hop (Vulkan→CUDA).** Per doc 09: allocate the `VkImage` with
  `VK_KHR_external_memory_fd`, `vkGetMemoryFdKHR` → dma-buf fd → `cuImportExternalMemory` →
  `CUdeviceptr`/`CUarray` → `NvEncRegisterResource(CUDADEVICEPTR)`. Handle import, **not** a pixel
  copy. Friction: CUDA↔Vulkan interop is fussy about tiling/modifiers (often wants `LINEAR` or a
  precisely described array) — budget it, don't discover it. `ZeroCopy::ExternalMemoryCuda`.
- **VA-API (broad fallback) — one hop (dma-buf import), both directions.**
  - *Encode* a Vulkan-rendered frame: `vkGetMemoryFdKHR` → dma-buf → import as a VA surface with
    `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`, respecting `modifier` + per-plane `pitch`.
  - *Feed a VA-decoded surface into the Vulkan compositor:* `vaSyncSurface()` then
    `vaExportSurfaceHandle()` → `VADRMPRIMESurfaceDescriptor` (dmabuf fds, strides, modifiers) →
    import as `VkImage` via `VK_EXT_external_memory_dma_buf` + `VK_EXT_image_drm_format_modifier`.
    **Gotcha:** on AMD Mesa an NV12 surface can export **disjoint fds per plane** — the importer must
    handle multi-fd planes, and you must `vaSyncSurface` before reading
    ([libva import/export PR](https://github.com/intel/libva/pull/125),
    [Chromium disjoint-plane patch](https://gist.github.com/thubble/235806c4c64b159653de879173d24d9f)).
    `ZeroCopy::DmaBufImport`.

All three keep the frame on-GPU end-to-end. **The one that categorically avoids the SPICE readback
with the least risk is Vulkan Video** (no export at all); NVENC and VA-API avoid it too but pay a
handle-import hop that is fussy about modifiers.

## 4. Guest video-decode offload (doc 17), generalized off NVDEC

Doc 17's premise — a guest playing video shouldn't burn vCPU on software decode — generalizes cleanly
because **the guest never talks to a real decoder**. It talks to *our* virtual decode DDI/API; we
remote the compressed bitstream + reference/parameter sets over the wire ring (doc 18); the **host**
decodes on whatever backend the host GPU has.

- **Guest surface:** Windows guest exposes **DXVA2 / D3D11VA**; Linux guest exposes **VA-API**
  (VDPAU is deprecated and now removed from Mesa's open drivers — don't wire it). Both are decode
  *DDIs* we intercept, exactly analogous to intercepting the 3D API in the render core.
- **Host decode:** route to `MediaCodec::decode` → Vulkan Video decode / NVDEC / VA-API, whichever the
  host GPU offers. Crucially, **NVDEC, AMD VCN, and Intel's decode engine are fixed-function blocks
  separate from the 3D shader cores** — a guest video decode does **not** steal render throughput from
  other tenants' 3D. That is a real multi-tenant win (relevant to doc 16's scheduler).
- **Stay on-GPU:** the decoded surface (a `VkImage` or VA surface) is *not* returned to the guest as
  pixels — it feeds the host compositor and then the **same encode path** (§3) that produces the remote
  stream. So the datapath is: guest bitstream → host decode engine → on-GPU surface → composite →
  encode → client. No readback anywhere. This is "decode remoting," the mirror of "render remoting."

## 5. Codec coverage per vendor + negotiation

**Hardware encode coverage that the protocol must respect (2026):**

| | H.264 enc | HEVC enc | AV1 enc |
|---|---|---|---|
| **NVIDIA Ampere (our A5000)** | ✅ | ✅ | ❌ (Ada+ only) |
| **NVIDIA Ada (RTX 40)** | ✅ | ✅ | ✅ |
| **AMD RDNA2 (VCN3)** | ✅ | ✅ | ❌ |
| **AMD RDNA3+ (VCN4/5)** | ✅ | ✅ | ✅ |
| **Intel Arc (DG2+)** | ✅ | ✅ | ✅ (via oneVPL) |

([NVENC AV1 = Ada](https://developer.nvidia.com/blog/av1-encoding-and-fruc-video-performance-boosts-and-higher-fidelity-on-the-nvidia-ada-architecture/),
[AMD VCN AV1 = RX 7000](https://en.wikipedia.org/wiki/Video_Core_Next),
[Intel Arc AV1 via VPL](https://www.intel.com/content/www/us/en/developer/articles/technical/onevpl-in-ffmpeg-for-great-streaming-on-intel-gpus.html))

**Client decode coverage (the other half of the negotiation)** — browser WebCodecs, 1M-device data:
H.264 is universal; HEVC needs Chrome 107+/Edge/Safari 16.4+/Firefox 133+ (Win) *and* a HW HEVC
decoder; AV1 decodes in most modern browsers (HW on recent Intel/AMD/Apple + NVIDIA Ampere+, SW
fallback else). **AV1 + HEVC together cover 99.73% of devices; H.264 is the legacy safety net for the
rest** ([WebCodecs 2026 codec data](https://webcodecsfundamentals.org/datasets/codec-analysis-2026/)).

**Negotiation algorithm (doc 18 handshake):**
1. Host advertises `encode` set from the selected `MediaCodec::caps()` for the GPU serving this VM.
2. Client advertises its decodable set (`VideoDecoder.isConfigSupported` per codec).
3. Pick `argmax(efficiency)` over the **intersection**, preference **AV1 > HEVC > H.264**, subject to
   a latency/quality policy knob. **H.264 is guaranteed in the intersection** (host: every GPU here
   encodes it; client: universal), so negotiation *never fails* — it only degrades.
4. Worked example — **A5000 host + Chrome client:** host `{H.264, HEVC}` ∩ client `{H.264, HEVC, AV1}`
   = `{H.264, HEVC}` → choose **HEVC**. Same host + a Firefox-on-Linux client lacking HW HEVC →
   `{H.264}` → **H.264**. Swap the host GPU for an RX 7900 and AV1 enters the set with no protocol or
   code change — the trait discovered it.

## 6. Rust ecosystem (build surface)

- **Vulkan Video:** `ash` exposes the video extensions (flagged experimental, tracks a moving spec);
  the `vulkan_video` crate gives safe decode+encode (H.264/H.265) bindings with **no FFmpeg/NVDEC
  dependency**, re-activated for current `ash` in Jan 2025
  ([vulkan_video crate](https://crates.io/crates/vulkan_video)).
- **VA-API:** `cros-libva` (safe libva) + `cros-codecs` (HW decode+encode) — the ChromeOS media stack,
  already Rust ([cros-libva](https://docs.rs/cros-libva), [cros-codecs](https://crates.io/crates/cros-codecs)).
- **NVENC:** via `cudarc` for the CUDA context + thin FFI over the Video Codec SDK, matching doc 09's
  interop chain.
- **FFmpeg** as a reference/oracle: 7.1/6.1 do Vulkan H.264/HEVC encode+decode and AV1 decode; **8.0
  adds Vulkan AV1 encode**; its Vulkan encoders are stated to have *feature-parity with the VA-API
  ones* ([FFmpeg Vulkan encode merge](https://www.phoronix.com/news/FFmpeg-Vulkan-Encode-H.265)) —
  useful to validate our own `MediaCodec` output against.

## 7. Recommendation summary

1. **Default backend = Vulkan Video.** Cross-vendor, native `VkImage`, zero interop, no CUDA. RADV is
   feature-complete (incl. AV1 encode on VCN4+); NVIDIA and Intel ship H.264/HEVC. It composes with our
   Vulkan renderer for free.
2. **Fallback = VA-API** on Linux hosts (AMD/Intel encode + decode; NVIDIA decode-only), imported via
   `vaExportSurfaceHandle`/DRM PRIME. Software x264/SVT-AV1 is the no-HW last resort (doc 09).
3. **Optimized = vendor-native, opt-in.** NVENC first (our A5000 bring-up encoder, via Vulkan→CUDA
   external memory); oneVPL to unlock Intel AV1 encode; AMF only if it beats RADV's Vulkan path
   (it likely doesn't — it *is* the Vulkan path). All behind the same `MediaCodec` trait.
4. **Probe, don't assume.** `vkGetPhysicalDeviceVideoCapabilitiesKHR` / `nvEncGetEncodeCaps` at
   startup produce the real per-GPU `CodecCaps`; the A5000's missing AV1 encoder is *discovered*.
5. **Negotiate host-encode ∩ client-decode, AV1 > HEVC > H.264, H.264 guaranteed.** The remote
   protocol carries only a codec both ends agreed on, and never fails to a working stream.

The vendor-specific seams are thus reduced to exactly four, all behind the trait: the codec API
(`CodecBackend` impls), the zero-copy import (`GpuFrame` + per-backend import), AV1-encode
availability (capability probe), and DPB/reference management (inside `encode`/`decode`). Adding AMD
or Intel is a backend + a priority-table row — not a rearchitecture.

## Sources

- Khronos — AV1 decode in Vulkan Video, SDK H.264/H.265 encode: https://www.khronos.org/blog/khronos-releases-vulkan-video-av1-decode-extension-vulkan-sdk-now-supports-h.264-h.265-encode
- Khronos — Vulkan Video AV1 encode + quantization-map extensions: https://www.khronos.org/blog/khronos-announces-vulkan-video-encode-av1-encode-quantization-map-extensions
- Khronos — H.264/H.265 encode finalized: https://www.khronos.org/blog/khronos-finalizes-vulkan-video-extensions-for-accelerated-h.264-and-h.265-encode
- Igalia — Vulkan Video per-driver status (versions): https://blogs.igalia.com/vjaquez/vulkan-video-status/
- Phoronix — RADV merges AV1 Vulkan encode (Mesa 25.2): https://www.phoronix.com/news/RADV-Merges-AV1-Encode
- airlied — RADV VK_KHR_video_encode_av1 support: https://airlied.blogspot.com/2025/07/radv-vkkhrvideoencodeav1-support.html
- Phoronix — Intel ANV Gen12.5 H.265 encode re-enabled (Mesa 26.2): https://www.phoronix.com/news/Intel-ANV-Gen125-H265-Encode
- Phoronix — Intel ANV Vulkan AV1 decode on Battlemage/Lunar Lake: https://www.phoronix.com/news/Intel-ANV-Vulkan-AV1-Decode
- NVIDIA Vulkan driver support page: https://developer.nvidia.com/vulkan-driver
- NVIDIA — In-depth Vulkan Video support (blog): https://developer.nvidia.com/blog/gpu-accelerated-video-processing-with-nvidia-in-depth-support-for-vulkan-video/
- 3dverse — "H.264 Vulkan Video Encoding (Scary)" maturity notes: https://docs.3dverse.com/devlog/2025/04/25/making-a-vulkan-encoder
- NVIDIA — AV1 on Ada Lovelace (Ampere has no AV1 encode): https://developer.nvidia.com/blog/improving-video-quality-and-performance-with-av1-and-nvidia-ada-lovelace-architecture/
- NVIDIA — AV1 encoding + optical flow on Ada: https://developer.nvidia.com/blog/av1-encoding-and-fruc-video-performance-boosts-and-higher-fidelity-on-the-nvidia-ada-architecture/
- Wikipedia — NVENC codec/architecture matrix: https://en.wikipedia.org/wiki/NVENC
- Wikipedia — AMD Video Core Next (VCN AV1 = VCN4/RX 7000): https://en.wikipedia.org/wiki/Video_Core_Next
- AMD GPUOpen — Advanced Media Framework (AMF): https://gpuopen.com/advanced-media-framework/
- Intel — VPL video codecs / AV1 encode (Arc): https://www.intel.com/content/www/us/en/developer/tools/vpl/overview.html
- Intel — VPL in FFmpeg / AV1 hardware encode: https://www.intel.com/content/www/us/en/developer/articles/technical/onevpl-in-ffmpeg-for-great-streaming-on-intel-gpus.html
- elFarto nvidia-vaapi-driver README (decode-only, no encode): https://github.com/elFarto/nvidia-vaapi-driver/blob/master/README.md
- UbuntuHandbook — nvidia-vaapi-driver 0.0.17 (2026): https://ubuntuhandbook.org/index.php/2026/05/nvidia-va-api-driver-0-0-17/
- ArchWiki — Hardware video acceleration (VA-API/VDPAU, Mesa 25.3 VDPAU removal): https://wiki.archlinux.org/title/Hardware_video_acceleration
- libva PR #125 — flexible DRM object import/export: https://github.com/intel/libva/pull/125
- Chromium disjoint-plane dma-buf patch (AMD Mesa NV12): https://gist.github.com/thubble/235806c4c64b159653de879173d24d9f
- Intel — media pipeline interop / dma-buf memory sharing: https://www.intel.com/content/www/us/en/docs/oneapi/optimization-guide-gpu/2024-1/memory-sharing-with-media.html
- Phoronix — FFmpeg Vulkan H.264/H.265 encode merged: https://www.phoronix.com/news/FFmpeg-Vulkan-Encode-H.265
- Rendi — FFmpeg 8.0 Vulkan AV1 encode / VP9 decode: https://www.rendi.dev/blog/ffmpeg-8-0-part-3-failed-attempts-to-use-vulkan-for-av1-encoding-vp9-decoding
- vulkan_video Rust crate (safe Vulkan Video bindings): https://crates.io/crates/vulkan_video
- ash-rs (Vulkan bindings, video extensions): https://github.com/ash-rs/ash
- cros-libva (safe libva Rust): https://docs.rs/cros-libva
- cros-codecs (Rust HW decode/encode): https://crates.io/crates/cros-codecs
- WebCodecs codec analysis 2026 (client decode coverage, AV1+HEVC=99.73%): https://webcodecsfundamentals.org/datasets/codec-analysis-2026/
- enable-chromium-hevc-hardware-decoding (browser HEVC support matrix): https://github.com/StaZhu/enable-chromium-hevc-hardware-decoding
- Doc 09 (this corpus) — presentation path, NVENC + Vulkan→CUDA interop, SPICE readback: docs/research/09-presentation-latency.md
- Doc 06 (this corpus) — blob dma-buf transport & host GPU execution: docs/research/06-data-plane-and-host-gpu.md
