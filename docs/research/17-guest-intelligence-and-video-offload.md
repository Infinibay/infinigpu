# 17 — Guest-Side Intelligence & the Video Offload Path (the VDI density specializations)

**Scope:** the *guest* half of the "intelligent driver both host and guest side," and the video
decode/encode path. These are the two levers that let one A5000 carry *many* desktops. The host
arbiter (doc 06) and its capacity manager (doc 16) decide *who gets GPU time*; this doc is about
**not asking for GPU time you don't need**, and about **routing video to the dedicated media
engines so it never competes with 3D at all.**

## Verdict

Density in VDI is not won by a cleverer multiplexer — it is won by *doing less work per desktop*.
Real remote-display stacks (SPICE, Citrix HDX/Thinwire, RDP, Moonlight) spend most of their
engineering on **suppressing work the client will never see**: static screens submit nothing,
changed screens submit only their damaged rectangles, and video is handled by a separate code path.
We adopt the same posture and add the piece a *virtualized* GPU can do that a pure display protocol
cannot: **remote the guest's video decode to the host's NVDEC block** and keep the frame on-GPU
through compositing and NVENC. Because NVDEC/NVENC are fixed-function engines physically separate
from the SM array, an office desktop watching a Teams call can consume **≈0 SM time** — the SMs stay
reserved for the handful of CAD/AI users. That asymmetry *is* the density argument.

---

## 1. Guest-side intelligence: minimize wasted work

The guest driver (Linux DRM/KMS KMD, doc 04; Windows IddCx, doc 03) is the first place work can be
killed. Four suppression mechanisms, all backed by mechanisms the guest OS already exposes.

### 1.1 Idle / no-change detection + present-on-demand

A static desktop must submit **nothing**. The mechanism differs per OS but the OS gives us the
signal for free:

- **Windows / IddCx.** IddCx hands the driver each composited frame with a dirty-rect list. A frame
  with `MoveRegionCount == 0` and `DirtyRectCount == 1` whose single rect is all-zero **means "no
  change since the previous frame"** — an explicit idle signal from the OS. After the desktop goes
  static the OS re-presents the same frame only `StaticDesktopReencodeFrameCount` times (giving the
  encoder a chance to refine quality) and then **stops presenting entirely until the next update**
  ([IddCx debugging / static-desktop behavior](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-debugging),
  [IddCxSwapChainGetDirtyRects](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/nf-iddcx-iddcxswapchaingetdirtyrects)).
  Present-on-demand is therefore *built into* the Windows path — our driver just honors it and stops
  kicking the ring.
- **Linux / DRM-KMS.** A Wayland/X compositor that has nothing to repaint issues no atomic
  page-flip, so our KMD's `atomic_commit` is simply never called — the absence of a flush *is* the
  idle signal. No polling.
- **Guest driver policy.** Maintain a per-scanout `dirty_accumulator` and a `last_flush_ns`. If no
  damage arrives for `idle_hold` (e.g. 100 ms) the driver drops that scanout's submission rate to
  0 fps (present-on-demand), tearing down nothing and leaving only the cursor plane (§1.5) live. A
  single input event or damage rect re-arms it. This is the cheapest possible state for the
  overwhelmingly common case — a mostly-idle office desktop.

### 1.2 Damage tracking: submit only changed rectangles

When something *does* change, send the rectangles, not the frame. Every serious VDI protocol does
this — Citrix Thinwire diffs regions and applies a video codec only to the moving part while keeping
text as cached bitmaps; SPICE ships dirty-rect diffs and promotes moving regions to a stream
([Citrix Thinwire](https://docs.citrix.com/en-us/citrix-virtual-apps-desktops/graphics/thinwire/thinwire.html),
[SPICE adaptive streaming](https://lists.freedesktop.org/archives/spice-devel/2013-February/012422.html)).
The guest OSes hand us the rectangles directly:

- **Linux:** the `FB_DAMAGE_CLIPS` plane property carries the damaged clip list in the atomic commit;
  the `drm_atomic_helper_damage_iter_init/next` helpers walk it, and drivers fall back to a full-plane
  update if the framebuffer itself changed (`drm_plane_state.ignore_damage_clips`)
  ([DRM KMS helpers](https://docs.kernel.org/gpu/drm-kms-helpers.html),
  [DRM KMS](https://docs.kernel.org/gpu/drm-kms.html)). Our KMD reads those clips in `atomic_commit`.
- **Windows:** `IddCxSwapChainGetDirtyRects` (and, pre-IddCx-1.7, `IddCxSwapChainGetMoveRegions`)
  return the changed regions per frame; since **IddCx 1.7 move regions are folded into the dirty-rect
  list** so there is one list to consume
  ([IddCx 1.7 updates](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/iddcx1.7-updates),
  [IDDCX_METADATA](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/ns-iddcx-iddcx_metadata)).

**Wire impact (extends ADR-0004).** The existing `SET_SCANOUT_BLOB` / `RESOURCE_FLUSH` messages gain
an optional trailing **damage-rect array** (`{x,y,w,h}` clips + a `full_frame` flag). Framing stays
payload-agnostic; the host encoder consumes the clip list to encode only the affected tiles. For a
blinking cursor in a text editor this is a few hundred pixels instead of a 1080p frame.

### 1.3 Adaptive frame pacing

Never render faster than the client will display, and never send two identical frames. Present is
**flush-driven, not vblank-locked** (doc 09 §5). The guest keeps a `target_fps` derived from
(a) the remote client's advertised refresh (e.g. 60), and (b) the host's current budget (§2). A
submission pacer coalesces damage that arrives inside one `1/target_fps` window into a single flush,
and a cheap per-tile content hash drops a flush whose tiles are byte-identical to the last. SPICE
derives its rate exactly this way — it lowers frame rate as server-side pipe congestion / frame
drops rise
([SPICE adaptive streaming](https://lists.freedesktop.org/archives/spice-devel/2013-February/012422.html));
Citrix HDX scales cadence up to 120 fps for motion and down to near-zero when static
([Thinwire](https://docs.citrix.com/en-us/citrix-virtual-apps-desktops/graphics/thinwire/thinwire.html)).
We do the same but the pacing input is a real budget signal from the host, not just a network guess.

### 1.4 Foreground / focus awareness

The guest is the only party that knows *which window the user is looking at*. The driver reports, on
the control ring, the **focused-app class**, the **active monitor**, and **which scanout holds the
focused window**. The host uses this to (a) raise that VM's replay context to a higher
`VK_EXT_global_priority` band, and (b) spend encode bitrate/fps on the focused scanout while
starving background/occluded heads. In a multi-monitor knowledge-worker session the unfocused heads
drop to a few fps at lower quality — invisible to the user, large savings for the pool. This mirrors
gaze/foreground-driven bitrate steering used in cloud gaming
([EyeNexus / gaze-driven VR streaming](https://arxiv.org/pdf/2509.11807)).

### 1.5 Hardware-cursor plane

Pointer motion must never re-render or re-encode the frame — the classic VDI latency sin (doc 09
§5). The cursor rides its **own dedicated ring/plane** (virtio-gpu's cursor virtqueue, doc 04;
IddCx's hardware-cursor support, doc 03). The host composites the cursor as a separate overlay at
input latency, so a user sweeping the mouse across a static desktop generates cursor-position
messages (bytes) and **zero** frame encodes.

---

## 2. Cooperative host↔guest feedback (extends the ADR-0004 control ring)

A greedy guest that submits at full rate regardless of load forces the host into *hard* gating —
which is how helix.ml's multi-desktop stack deadlocked (`renderer_blocked` froze every context when
one lagged, doc 06 §2). The fix is a **cooperative** loop: the host advertises budget, the guest
throttles *itself* before the host has to. Model it on WebRTC congestion control (GCC/TWCC:
delay-gradient + explicit feedback drive the sender's bitrate)
([TWCC](https://flussonic.com/blog/news/transport-cc),
[GCC in cloud gaming](https://dl.acm.org/doi/abs/10.1145/3746027.3755439)) — but in-band over the
shared-memory control ring, so feedback is microseconds, not an RTCP round-trip.

**New control-ring message classes** (low-frequency, `postcard`-encoded per ADR-0004):

- `BUDGET_ADVERTISE` (host→guest, ~10 Hz): `{ gpu_time_tokens_per_sec, vram_headroom_bytes,
  priority_band, suggested_max_fps, congestion_level: {Green|Yellow|Red} }`. The host publishes the
  VM's current token refill rate and VRAM ceiling — a mirror of the arbiter's token bucket (doc 06
  §4) exposed to the guest.
- `GUEST_HINT` (guest→host, ~10 Hz or on change): `{ focused_app_class, active_scanout,
  demanded_fps, damage_rate, idle: bool, vram_working_set, decode_sessions }` — the §1/§4 telemetry.
- `BACKPRESSURE` (host→guest, event): a fast "slow down NOW" when the ring/encoder queue is filling,
  distinct from the smooth budget signal — the analog of SPICE's frame-drop detector.

**Guest reaction policy — a degradation ladder** (applied top-down as `congestion_level` worsens,
so behavior is *graceful*, not a cliff):

1. **Green:** honor `suggested_max_fps`, full quality.
2. **Yellow:** cap fps to the client refresh; coalesce sub-frame damage; pause background-scanout and
   occluded-window submissions.
3. **Red:** drop to damage-only, reduce internal render scale / encode quality, defer non-visible
   work (offscreen compositor buffers), and for non-focused VMs fall to present-on-demand.
4. **VRAM pressure** (independent axis): when `vram_headroom_bytes` shrinks, the guest proactively
   releases cached/idle allocations and stops speculative swapchain growth *before* the host's
   admission control (doc 06 §4) has to `VK_ERROR_OUT_OF_DEVICE_MEMORY` it.

Because the guest self-throttles from an advertised budget, the host rarely needs the blunt
instruments (hard token-gating, context suspension); those become the fallback for a *misbehaving*
guest, not the steady-state control loop. A cooperative guest is a first-class citizen; a greedy one
is contained but not trusted.

---

## 3. Video decode/encode offload — the single highest-value specialization

### 3.1 Why this is a density multiplier, not a feature

Office/knowledge VDI is **dominated by video**: Teams/Zoom/YouTube, all day. Video runs on the GPU's
**NVDEC (decode) and NVENC (encode)** blocks, which are **fixed-function engines physically separate
from the SM array** ([Ampere GA102 whitepaper](https://www.nvidia.com/content/PDF/nvidia-ampere-ga-102-gpu-architecture-whitepaper-v2.1.pdf),
[NVENC](https://en.wikipedia.org/wiki/NVENC)). Two consequences:

1. **Video decode consumes no SM time.** A 1080p30 H.264 stream is a small fraction of one NVDEC
   block; decoding a dozen of them still leaves the SMs entirely free for the CAD/AI persona.
2. **Doing it host-side halves the video GPU work.** The naive path is: guest decodes in-guest (on
   *its* vGPU/SM budget or CPU), the decoded frame lands in the desktop, and then the host must
   **re-encode the whole desktop** for the console — two video operations plus a readback. The
   offload path is **one decode + one encode**, both on dedicated blocks, both on-GPU.

> *NEEDS VERIFICATION:* exact NVDEC/NVENC block counts per GA102 SKU vary by source (the A5000 is
> commonly cited as 7th-gen NVENC + 5th-gen NVDEC; doc 09 assumes ~2 encode blocks). The
> load-bearing fact — they are separate fixed-function engines, not SMs — is not in dispute.

### 3.2 The routing: guest decode surface → host NVDEC

The guest app already asks the OS for hardware decode through a standard API; we intercept that API
the same way we intercept Vulkan, but the payload is a **compressed bitstream** (megabits/s), not a
3D command stream:

- **Linux:** apps use **VA-API / VDPAU**. We ship a VA-API backend that remotes the bitstream to the
  host. This is a *proven* pattern: crosvm's **virtio-video** device does exactly this — a
  paravirtual video codec device that forwards a guest's decode requests to a host VA-API backend
  (`cros-libva`/`cros-codecs`), using **DMA-BUF to import the host GPU surface into the guest's
  VA-API context so there is no copy**, and hitting 4K60 decode
  ([crosvm video device](https://crosvm.dev/book/devices/video.html),
  [Collabora virtio-video VA-API](https://www.collabora.com/news-and-blog/blog/2024/06/06/a-roadmap-for-virtio-video-on-chromeos-part-3/),
  [crosvm vaapi backend](https://crosvm.dev/doc/devices/virtio/video/decoder/backend/vaapi/index.html)).
  On the host we back it with **NVDEC** — either via the `NVDECODE`/CUVID API directly or via
  `nvidia-vaapi-driver`, an existing VA-API-over-NVDEC shim
  ([elFarto/nvidia-vaapi-driver](https://github.com/elFarto/nvidia-vaapi-driver)).
- **Windows:** apps use **DXVA2 / D3D11VA** (Media Foundation). We expose a decode device that
  remotes those DDI calls to host NVDEC. *Honest cost:* there is no production virtio-video Windows
  guest driver, so this is net-new — but the decode DDI is a far narrower surface than a full WDDM 3D
  render driver (doc 03 M3), so it is a much smaller build than in-guest D3D acceleration.

**Bandwidth win:** we ship ~8 Mbit/s of H.264 across the ring instead of ~3 Gbit/s of decoded 1080p60
RGBA. The compressed stream is *cheaper to transport* than the pixels it becomes.

### 3.3 Decode → compose → encode with **no GPU→sysmem readback**

The whole point is to keep the frame resident in GPU memory from decode to wire:

1. **Decode.** NVDEC writes the decoded surface to CUDA device memory — the `NVDECODE` API maps the
   frame to a `CUdeviceptr`/`CUarray` for CUDA post-processing
   ([Video Codec SDK](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/read-me/index.html)).
2. **Compose.** The desktop (a Vulkan image from 3D replay, an IddCx surface, or a 2D dumb buffer)
   and the decoded video rect are composited **on-GPU**: either a CUDA kernel blits the video into
   the desktop image (imported via `VK_KHR_external_memory_fd` → `cuImportExternalMemory`, doc 09
   §2), or Vulkan imports the NVDEC frame as an external image. The cursor stays on its own plane
   (§1.5).
3. **Encode.** The composited frame feeds **NVENC**. NVIDIA's own zero-copy transcode sample
   (`AppTransZeroCopy`) registers a shared pool of CUDA arrays with **both** NVDEC and NVENC so the
   decoder writes and the encoder reads the same memory with no copy — which "significantly lowers SM
   utilization" versus the copy path
   ([NVIDIA transcoding guide](https://developer.nvidia.com/blog/nvidia-ffmpeg-transcoding-guide/),
   [on-device transcode, no CPU copies](https://forums.developer.nvidia.com/t/how-to-use-nvenc-and-nvdec-for-on-device-transcoding-no-cpu-copies/233194)).
   NVENC accepts ARGB directly and does the RGB→NV12 CSC in fixed-function hardware (doc 09 §2), so
   the RGBA desktop image goes straight in.

Across the entire decode→compose→encode chain there is **zero GPU→sysmem readback** — the density
killer that the Phase-0 SPICE path pays (doc 09 §4).

**Codecs on GA102 (RTX A5000):** NVDEC decodes **H.264, HEVC, VP9, VP8, AV1 (10-bit, up to 8K), MPEG-2,
VC-1**; Ampere added AV1 *decode* over Turing
([NVDEC Ampere matrix](https://videocardz.com/newz/nvidia-updates-nvdec-video-decoding-and-nvenc-encoding-matrixes-for-ampere-gpus)).
NVENC encodes **H.264 and HEVC only — no AV1** (AV1 *encode* is Ada Lovelace)
([AV1 on Ada](https://developer.nvidia.com/blog/improving-video-quality-and-performance-with-av1-and-nvidia-ada-lovelace-architecture/),
[NVENC](https://en.wikipedia.org/wiki/NVENC)). Practical rule: **accept AV1/VP9/HEVC in** from the
guest's browser (an increasing share of Teams/YouTube traffic in 2026), **emit H.264/HEVC out** to
the console. AV1 decode-in is a genuine 2026 win we get for free on this hardware.

### 3.4 The office persona collapses to near-zero SM

Combine §1 (damage-tracked static 2D desktop) with §3 (video routed to NVDEC): an **office VM needs
essentially no in-guest Vulkan replay**. Its desktop is static 2D damage rects; its one "GPU
workload" — the video call — is a remoted decode surface handled entirely on NVDEC and composited
host-side. Dozens of such desktops can share one A5000 while the SMs sit idle, held in reserve for
the few designer/CAD/AI VMs that actually issue 3D/CUDA. **That is the density lever the whole
project exists to pull.**

---

## 4. Guest telemetry → host scheduler (feeding the doc-16 capacity manager)

The capacity manager (doc 16) needs to know "what is available RIGHT NOW" and "what does each guest
demand." Guest telemetry is the demand side. Split it by **timescale**, because a frame pacer and a
persona classifier have wildly different latency needs:

| Tier | Transport | Cadence | Payload | Consumer |
|---|---|---|---|---|
| **Fast** | control ring (`GUEST_HINT`, §2) — in-band | sub-second / on change | per-scanout demanded fps, damage rate, idle flag, focused-app class, VRAM working set, active decode sessions | arbiter token bucket + admission (doc 06 §4); reacts to bursts *this frame* |
| **Slow** | **infiniservice** virtio-serial NDJSON — out-of-band | seconds | persona (office/knowledge/designer), running-app inventory, GPU process list, coarse diurnal trend | capacity manager (doc 16) for policy defaults + login-storm prediction |

**Why split.** The scheduler must react to a burst or a login storm within a frame or two — far
faster than infiniservice's ~30 s metric cadence (CLAUDE.md: `collector.rs`, ~30 s). So the
fast, reactive signal rides the **control ring that already exists** (ADR-0004). But persona
classification, app inventory, and trend data are exactly what the **infiniservice agent already
does** — collect in-guest state and stream it NDJSON over the HMAC-signed `org.infinibay.agent`
channel. Reuse that seam for the slow tier: don't reinvent auth/transport for coarse data, and don't
overload the agent's slow channel with per-frame pacing.

**Persona → default policy** (the capacity manager's starting point, refined by live hints):

- **Office/task:** small VRAM cap, **high latency priority**, video-offload-first, aggressive
  present-on-demand. Latency-critical, tiny GPU-time.
- **Knowledge/power:** multi-monitor with focus-steering (§1.4), moderate VRAM, light 3D best-effort.
- **Designer/CAD/AI:** large VRAM admission, **best-effort GPU-time** (tolerates throttling), lower
  latency priority — the persona that actually burns SMs, and the one that gets the SM budget the
  office desktops are *not* using.

**Login/boot storms.** Because the slow tier reports persona and the diurnal trend, the capacity
manager can anticipate the ~9am storm: **stagger** host context creation, **pre-warm** NVDEC/NVENC
sessions, and admit VMs by persona priority so latency-critical office desktops light up first while
CAD VMs ramp best-effort. The loop closes: the host advertises budget (§2), the guest reports demand
(§4 fast tier), and the capacity manager reconciles the two against real-time VRAM + GPU-time
headroom (doc 16). The guest is never guessing, and the host is never surprised.

## Sources

- DRM/KMS (damage, atomic commit): https://docs.kernel.org/gpu/drm-kms.html
- DRM/KMS helpers (`FB_DAMAGE_CLIPS`, `drm_atomic_helper_damage_iter`, `ignore_damage_clips`): https://docs.kernel.org/gpu/drm-kms-helpers.html
- Windows IddCx — `IddCxSwapChainGetDirtyRects`: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/nf-iddcx-iddcxswapchaingetdirtyrects
- Windows IddCx — debugging / static-desktop reencode + zero-rect idle signal: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-debugging
- Windows IddCx 1.7 updates (move regions folded into dirty rects): https://learn.microsoft.com/en-us/windows-hardware/drivers/display/iddcx1.7-updates
- Windows IddCx — `IDDCX_METADATA`: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/ns-iddcx-iddcx_metadata
- Citrix Thinwire (adaptive display, region codec selection): https://docs.citrix.com/en-us/citrix-virtual-apps-desktops/graphics/thinwire/thinwire.html
- Citrix Intelligent Build to Lossless: https://docs.citrix.com/en-us/citrix-virtual-apps-desktops/graphics/thinwire/intelligent-build-to-lossless.html
- SPICE adaptive video streaming (frame-drop-driven rate, client feedback): https://lists.freedesktop.org/archives/spice-devel/2013-February/012422.html
- WebRTC Transport-Wide Congestion Control (TWCC): https://flussonic.com/blog/news/transport-cc
- Congestion control for cloud gaming (GCC/NADA comparison, ACM MM 2025): https://dl.acm.org/doi/abs/10.1145/3746027.3755439
- Gaze-driven bitrate steering (EyeNexus): https://arxiv.org/pdf/2509.11807
- crosvm virtio-video device (guest decode/encode → host): https://crosvm.dev/book/devices/video.html
- Collabora — VirtIO Video on ChromeOS, VA-API backend + DMA-BUF zero-copy: https://www.collabora.com/news-and-blog/blog/2024/06/06/a-roadmap-for-virtio-video-on-chromeos-part-3/
- crosvm VA-API decoder backend: https://crosvm.dev/doc/devices/virtio/video/decoder/backend/vaapi/index.html
- nvidia-vaapi-driver (VA-API over NVDEC): https://github.com/elFarto/nvidia-vaapi-driver
- NVIDIA Video Codec SDK read-me (NVDECODE → CUDA mapping): https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/read-me/index.html
- NVIDIA FFmpeg transcoding guide (AppTransZeroCopy, on-GPU decode→encode): https://developer.nvidia.com/blog/nvidia-ffmpeg-transcoding-guide/
- NVIDIA dev forum — NVENC+NVDEC on-device transcode, no CPU copies: https://forums.developer.nvidia.com/t/how-to-use-nvenc-and-nvdec-for-on-device-transcoding-no-cpu-copies/233194
- NVDEC/NVENC Ampere matrix (AV1 decode added, no AV1 encode): https://videocardz.com/newz/nvidia-updates-nvdec-video-decoding-and-nvenc-encoding-matrixes-for-ampere-gpus
- AV1 encode arrives on Ada Lovelace (not Ampere): https://developer.nvidia.com/blog/improving-video-quality-and-performance-with-av1-and-nvidia-ada-lovelace-architecture/
- NVENC (generations, GA102, codec support): https://en.wikipedia.org/wiki/NVENC
- NVIDIA Ampere GA102 architecture whitepaper (media engines separate from SMs): https://www.nvidia.com/content/PDF/nvidia-ampere-ga-102-gpu-architecture-whitepaper-v2.1.pdf
- Internal: ADR-0004 (control ring / multi-ring envelope), research/06 (data-plane, arbiter multiplexing), research/09 (presentation & NVENC), research/03 (Windows IddCx), research/04 (Linux DRM/KMS), doc 16 (host capacity manager, sibling), CLAUDE.md (infiniservice virtio-serial NDJSON agent seam)
