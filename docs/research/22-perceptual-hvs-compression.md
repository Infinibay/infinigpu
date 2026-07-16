# 22 — Perceptual / HVS Compression: Spending Bits Where the Eye Looks

**Scope.** The display datapath is decided (docs 09, 11, 16): the host renders each desktop
to a headless Vulkan context, the frame is already an on-GPU blob, and we encode it with
**zero readback** via NVENC now / Vulkan Video later, streaming to a browser WebCodecs client.
This doc designs the **perceptual encoding layer** between "frame on GPU" and "bytes on the
wire": an **attention model** built without eye-tracking, a **per-region QP map** driving the
hardware encoder, a **content classifier** routing text vs. video vs. 3D, and a ranked set of
**human-visual-system (HVS) tricks** specialized for a VDI desktop — each grounded in vision
science and in what the encoders actually expose.

## Verdict up front

**The biggest perceptual wins for a VDI desktop are not the classic per-pixel HVS tricks — they
are temporal (a static desktop is ~0 bits) and structural (route text through a screen-content
path, not a video codec).** Foveation, CSF quantization, and chroma subsampling are real and
stackable, but on a desktop they are *multipliers on the small dynamic fraction of the screen*,
while damage-driven frame-skip and content routing govern the *whole* frame. Both NVENC (emphasis
+ delta QP maps) and Vulkan Video (`VK_KHR_video_encode_quantization_map`, Nov 2024) expose a
**per-block QP map** — that single primitive is enough to drive every spatial HVS lever we design.
The attention model is buildable from signals the guest already reports (doc 17: foreground app,
active monitor, damage rects) plus the host-side cursor — **no eye-tracking hardware required.**

---

## 1. The attention model — foveation without an eye tracker

Real foveated streaming (Tobii-tracked cloud gaming) cuts bandwidth by **>50%** by putting a
Gaussian quality peak on the gaze point and letting quality fall off with eccentricity, because
cone density (and acuity) drops sharply within ~2° of the fovea
([Foveated Video Streaming for Cloud Gaming, ar5iv/1706.04804](https://ar5iv.labs.arxiv.org/html/1706.04804)).
We have no eye tracker. But a desktop is not a game: **the eye's fixation is overwhelmingly
predictable from UI state.** People look where the caret blinks, where the cursor is, and inside
the window they just focused. We approximate the fovea from four cheap, already-available signals
and fuse them into a **saliency field** `S(x,y) ∈ [0,1]`:

| Signal | Source | Why it predicts gaze |
|---|---|---|
| **Cursor position** | host present path (we own the cursor) | Strongest single predictor of attention on a desktop; hands follow eyes |
| **Focused-window bounds** | guest driver, doc 17 (foreground app + active monitor) | The user is working *inside* one window; other monitors/windows are peripheral |
| **Recent damage rects** | guest driver, doc 17 (dirty regions) | Change draws the eye (transient/motion salience); a blinking caret or scrolling text is a change hotspot |
| **Text-caret / active control** | doc-17 damage clustered + small | Editing focus; a tight high-value peak |

```
S(x,y) = max(
   G(x,y; cursor,     σ_cursor),      // 2D Gaussian peak at the cursor, ~5–8° wide
   w_focus  · inside(focused_window),  // plateau over the focused window
   w_damage · decay(recent_damage))    // transient boost on freshly-changed pixels, decays ~300ms
   · monitor_gain(active_monitor)       // non-active monitors get a flat penalty
```

The Gaussian falloff mirrors the acuity model the cloud-gaming work validated — quality should
follow visual acuity "more naturally" than a step function because cone density decays smoothly
([1706.04804](https://ar5iv.labs.arxiv.org/html/1706.04804)). Because we can't confirm the true
fovea, we make the peak **wider and shallower** than an eye-tracked system: protect a larger
central zone, drop the far periphery harder. Fixed/inside-out foveation without tracking is the
FovOptix / "fixed-foveated encoder" regime, which still yields large savings by encoding center
high, mid-periphery lower, far-periphery lowest
([FovOptix, ACM MMSys'24](https://dl.acm.org/doi/10.1145/3625468.3647612);
[Frame-Complexity-Aware Foveated Encoding, IEEE 2025](https://ieeexplore.ieee.org/document/11457547/)).
The model is **damage-gated**: changing peripheral regions keep quality (motion salience overrides
eccentricity), so we never blur a video the user is watching in a corner because the cursor is
elsewhere.

`S` is quantized to a low-res grid (one cell per encoder block, §2) and updated per frame from the
doc-17 telemetry, which the host capacity manager (doc 16) already ingests.

---

## 2. Driving the encoder — the per-region QP map

The saliency field becomes a **QP delta map**: `ΔQP(block) = round((1 − S_block) · QP_span)`, a
positive delta (coarser) in the periphery, ~0 at the attention peak. `QP_span` is set by the
per-VM budget the capacity manager hands down (doc 16 §7 degradation ladder — under contention the
span widens, foveation gets more aggressive). This one array drives **every** spatial HVS lever
below; the encoders expose it directly.

**NVENC (H.264/HEVC/AV1 on the A5000, current path).** NVENC exposes region QP two ways via
`NV_ENC_RC_PARAMS::qpMapMode` + the `NV_ENC_PIC_PARAMS::qpDeltaMap` signed-byte array (one value
per macroblock/CTB, **raster order**):
- **`NV_ENC_QP_MAP_DELTA`** — explicit per-block QP delta added on top of rate control. This is our
  primary lever: we write `ΔQP` directly.
- **`NV_ENC_QP_MAP_EMPHASIS`** — per-block `NV_ENC_EMPHASIS_MAP_LEVEL` "importance" hint; higher
  emphasis → larger negative QP adjustment, scaled by the RC-decided QP. Codec-independent, but
  **mutually exclusive with AQ (spatial/temporal)** and, being applied *after* RC, can cause
  VBV/rate violations
  ([NVENC Programming Guide 13.0](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html);
  [qpDeltaMap forum](https://forums.developer.nvidia.com/t/qpdeltamap-in-nvenc-5-0-1-sdk-samples/39121)).
  Query support with `NV_ENC_CAPS_SUPPORT_EMPHASIS_LEVEL_MAP`.

We use **delta-map** mode so we can keep NVENC's own AQ on for intra-block masking (§5) and layer
our attention deltas on top; emphasis mode is the fallback where delta granularity is unavailable.
Block granularity is 16×16 (H.264) / CTB-sized (HEVC/AV1) — coarse, but a desktop's attention
regions are large, so per-CTB is plenty. *NEEDS VERIFICATION: exact AV1 NVENC map block size on
Ada/Ampere.*

**Vulkan Video (cross-vendor path).** `VK_KHR_video_encode_quantization_map` (Vulkan 1.3.302,
Nov 2024) provides the identical primitive as a first-class object, for **H.264, H.265, and AV1**:
- **delta quantization maps** — "explicit codec-specific control of the final quantization for each
  block," valid in all RC modes including RC-disabled (constant-QP);
- **emphasis maps** — "codec-independent hint on the relative importance of different image blocks,"
  valid only when RC is enabled and not the default mode
  ([Khronos announce](https://www.khronos.org/blog/khronos-announces-vulkan-video-encode-av1-encode-quantization-map-extensions);
  [VK_KHR_video_encode_quantization_map spec](https://registry.khronos.org/vulkan/specs/latest/man/html/VK_KHR_video_encode_quantization_map.html)).

The map is supplied as an image handle alongside each encode input picture — **stays on-GPU**,
preserving our zero-readback invariant: the saliency compute shader writes the QP map into a GPU
image and hands it straight to the encoder. Beta NVIDIA + AMD drivers already support it. This is
the forward path once we move off the NVENC C API to `ash`/Vulkan Video (doc 09).

**AV1 segmentation (fallback / SW encoder).** AV1's frame can classify blocks into up to **8
segments**, each with its own QP offset, plus superblock/coding-block QP offset; SVT-AV1 exposes a
per-64×64-block QP-offset-map file for ROI, using the alternate-quantizer segment feature — so even
a software AV1 encoder can consume our attention map at 64×64 granularity
([SVT-AV1 Parameters](https://gitlab.com/AOMediaCodec/SVT-AV1/-/blob/v1.9.0-rc1/Docs/Parameters.md);
[A Technical Overview of AV1, arXiv 2008.06091](https://arxiv.org/pdf/2008.06091)).

---

## 3. Content-adaptive region classification & hybrid routing

A desktop frame is not one signal — it is **text/UI on a flat background** with a few **photographic
or video** rectangles (Teams, a browser video, a thumbnail) and, rarely, a **3D/CAD viewport**.
Feeding all of it to one 4:2:0 video codec is the classic mistake: chroma subsampling smears
anti-aliased text fringes and DCT quantization rings around high-contrast glyph edges, destroying
legibility — a **hard constraint** for a work desktop. The fix is a **hybrid codec** routing each
region to the right encoder, as RDP/RemoteFX and modern remoting stacks do.

**Classifier (per damage rect, cheap, runs on the GPU).** We already have damage rects from the
guest (doc 17); we classify each into one of three classes using features computable from the tile
histogram without a readback:

| Class | Discriminating features | Route |
|---|---|---|
| **Text / UI** | few distinct colors (small palette), high-contrast hard edges, sparse changes, high edge/area ratio | **Screen-content path**: near-lossless dirty-rect blit, **4:4:4**, optionally palette/RLE; composited client-side |
| **Photographic / video** | many colors, smooth gradients, high temporal change, natural-image statistics | **Video codec**: H.264/HEVC/AV1, **4:2:0**, perceptual RC (§5), foveated QP (§2) |
| **3D / CAD** | full-viewport churn, geometric edges + shading | **Video codec**, 4:2:0, but with x264-style psy-RDO / AV1 `--psy-rd` to preserve detail texture |

The screen-content route is not a hypothesis — it is what **HEVC Screen Content Coding** was built
for: palette mode + intra-block-copy give large gains on rendered text/graphics precisely because
those blocks are palettized and self-similar, not natural images
([Overview of HEVC-SCC, MERL TR2015-126](https://www.merl.com/publications/docs/TR2015-126.pdf)).
We don't need full HEVC-SCC; the SPICE/RDP lesson is simpler — **send UI as commands+palettized
image tiles, send only the dynamic rectangles through the video codec, and composite in the browser
client** (WebCodecs decodes the video regions; a 2D canvas layer blits the UI tiles). This is
directly the RDP "GFX pipeline" split: RemoteFX Progressive / ClearCodec for UI, H.264 AVC420/AVC444
for the video surface ([FreeRDP Codecs](https://freerdp-freerdp.mintlify.app/concepts/codecs)).

**Text edges get 4:4:4 via the AVC444 hybrid.** Where text *must* go through the video codec (e.g.
sub-pixel-animated UI), we borrow **RDP's AVC444**: pack a YUV444 frame into two YUV420 sub-frames
— a main (luma) view and an auxiliary (chroma) view — so the chroma planes survive at full
resolution for the text region while the rest stays 4:2:0
([MS-RDPEGFX YUV444 combination](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/8131c1bc-1af8-4907-a05a-f72f4581160f);
[Azure AVD graphics encoding](https://learn.microsoft.com/en-us/azure/virtual-desktop/graphics-encoding)).
AVC444 roughly doubles the pixel work of that region, so it is applied **only** to
classifier-flagged text tiles, not the whole frame — the whole point is spending 4:4:4 bits only
where legibility demands them.

---

## 4. HVS property → encoder lever map

The core deliverable: each HVS property, the concrete lever, and rough savings **for a VDI desktop**.
Savings are per the cited literature and apply to the fraction of the frame each lever governs
(that fraction is what makes the VDI ranking in §7 differ from a gaming ranking).

| HVS property | Vision fact | Encoder lever | Rough savings |
|---|---|---|---|
| **Foveal vs peripheral acuity** | acuity/cone density fall sharply beyond ~2° | attention QP map (§2), coarser toward periphery | **>50%** of the *video-coded* region ([1706.04804](https://ar5iv.labs.arxiv.org/html/1706.04804)) |
| **Contrast sensitivity function (CSF)** | eye can't resolve high spatial freq below a visibility threshold; peak sensitivity ~mid-freq | coarsely quantize high-freq DCT/transform coeffs (CSF/JND-weighted quant) | **~14% avg, up to ~40%** ([JND PVC, MDPI Entropy 21(11):1095](https://www.mdpi.com/1099-4300/21/11/1095)) |
| **Luma ≫ chroma acuity** | luma resolved ~50 cyc/deg, chroma ~25 | 4:2:0 default; 4:4:4 only for text (§3) | **~50%** of chroma samples ([chroma subsampling, Wikipedia](https://en.wikipedia.org/wiki/Chroma_subsampling)) |
| **Reduced blue-cone density** | S-cones 10–15%, blue-cone-free foveal center; color fusion ~25 Hz | YCoCg / decorrelated chroma, harder chroma quant, chroma at ≤ half framerate | ~0.5 dB over YCbCr; small but structural ([YCoCg, Wikipedia](https://en.wikipedia.org/wiki/YCoCg)) |
| **Luminance adaptation (Weber-Fechner)** | detection threshold rises with background luminance; dark & bright mask more, mid-gray most sensitive | luma-adaptive QP (raise QP in very dark/bright blocks) — this is encoder AQ | few %, folded into AQ ([visual masking AQ, ScienceDirect](https://www.sciencedirect.com/science/article/abs/pii/S0923596521001235)) |
| **Contrast/texture masking** | distortion hides in busy texture, shows in flat regions | variance-based AQ (raise QP in high-activity blocks) — x264/x265/SVT-AV1/VVenC QPA | baked into modern RC ([Codec Wiki psychovisual](https://wiki.x266.mov/docs/introduction/psychovisual)) |
| **Temporal masking** | sensitivity drops during/after fast motion & scene cuts | drop QP-quality during high-motion frames; ~20%-bit frames at scene change | ~1–5% steady, big at cuts ([temporal-masking coding, SAGE](https://journals.sagepub.com/doi/full/10.1177/0020294020944949)) |
| **Saccadic masking / omission** | intra-saccade smear is barely perceived | drop a frame's quality/skip during a large cursor jump or window switch (gaze-shift proxy) | opportunistic ([saccade masking, PMC5952294](https://pmc.ncbi.nlm.nih.gov/articles/PMC5952294/)) |
| **Flicker fusion** | steady above ~50–90 Hz (edges push it higher); color fusion ~25 Hz | framerate ceiling; chroma at lower temporal rate than luma | caps waste ([Peripheral flicker fusion, PMC10057432](https://pmc.ncbi.nlm.nih.gov/articles/PMC10057432/)) |
| **Change blindness** | un-attended changes go unnoticed | let periphery lag/refresh slower; only attention region gets fresh high-quality frames | opportunistic ([change-blindness coding, arXiv 2408.00052](https://arxiv.org/pdf/2408.00052)) |

---

## 5. Perceptual preprocessing & rate control

**Encode in a perceptually-uniform space.** Quantization error should be spread evenly in
*perceived* lightness, not linear light — so we keep the signal gamma/PQ-encoded (never quantize in
linear RGB) and decorrelate color with **YCoCg**, which beats YCbCr on energy compaction (~0.5 dB)
and is trivial/lossless to invert client-side ([YCoCg](https://en.wikipedia.org/wiki/YCoCg)). For
the flat-SDR desktop case, gamma-space YCoCg 4:2:0 is the default; a PQ path is only relevant for an
HDR guest.

**Turn on the encoder's psychovisual RC, don't reinvent it.** Modern encoders already implement the
masking levers (CSF, luminance/texture masking) as **adaptive quantization** + **psy-RD**:
- **AQ / QPA** redistributes QP by block spatial activity — x264, x265, SVT-AV1, VVenC all do this;
  it *is* the texture/luminance-masking lever in production form
  ([Codec Wiki](https://wiki.x266.mov/docs/introduction/psychovisual)).
- **psy-RDO / psy-RDOQ** (x264) and **`--psy-rd`** (SVT-AV1-PSY) bias the rate-distortion decision
  toward preserving *detail/energy* over mathematical fidelity — the eye prefers "distorted but
  detail-rich" to "clean but blurry," which is exactly right for a CAD viewport or video texture
  ([Codec Wiki](https://wiki.x266.mov/docs/introduction/psychovisual);
  [SVT-AV1-PSY Parameters](https://github.com/psy-ex/svt-av1-psy/blob/master/Docs/Parameters.md)).

Our contribution is the **spatial map** these RC modes don't have: the attention QP delta (§2)
layered on top of AQ so the encoder's own masking handles *within-region* detail while our map
handles *across-region* attention.

**Target a perceptual metric, never PSNR.** Rate/quality tuning and the doc-16 degradation ladder's
"quality" axis are measured with **VMAF** (Netflix, trained on subjective scores) and
**SSIMULACRA2 / XPSNR / butteraugli** for edge- and color-aware checks — PSNR would reward blurring
text ([Codec Wiki](https://wiki.x266.mov/docs/introduction/psychovisual)). Legibility gets a
dedicated **edge-preservation check** on text tiles (the classifier already found them): a text
region that drops below an edge-fidelity floor forces the 4:4:4 / near-lossless route (§3) — a hard
gate, not a soft metric.

---

## 6. Temporal — the biggest VDI lever

This is where a desktop crushes a game. A game renders a fresh, fully-changed frame ~every 16 ms; a
desktop is **static most of the time**, with bursts.

**Damage-driven frame-skip (the #1 win).** The guest reports damage rects (doc 17); if nothing
changed, **we send nothing** — no encode, no frame. The present clock is driven by damage, not a
fixed vsync. An idle desktop, a user reading a page, a paused document: ~0 bits/s. This alone
removes the vast majority of a desktop's frames — a saving no per-pixel HVS trick can approach,
and it directly cuts encoder GPU-time (feeding the doc-16 capacity budget).

**Adaptive framerate under a flicker ceiling.** When content *is* moving, cap the framerate at the
useful ceiling. Steady flicker fuses above ~50–90 Hz, and **critical color fusion is ~25 Hz** —
about half the luma flicker rate — so chroma can be refreshed at a lower temporal rate than luma
without visible artifacts ([Peripheral flicker fusion, PMC10057432](https://pmc.ncbi.nlm.nih.gov/articles/PMC10057432/)).
A desktop that's scrolling at 60 Hz doesn't benefit from 120; a mostly-static one drops to a few Hz.
The capacity manager (doc 16) sets the per-VM ceiling; contention lowers it, background monitors
lower it further.

**Temporal & saccadic masking during motion.** During fast motion the eye can't resolve detail, so
we **raise QP / drop quality on high-motion frames** and spend the saved bits on the static frames
that follow — perceptually lossless per temporal-masking studies, and scene-cut frames can be coded
with as little as ~20% of a normal frame's bits with no perceived loss
([temporal-masking coding, SAGE](https://journals.sagepub.com/doi/full/10.1177/0020294020944949)).
We *approximate a gaze shift* — the moment saccadic omission makes the eye least sensitive — from
our own signals: a **large cursor jump or a focused-window switch** (doc 17) triggers a one-frame
quality dip, hiding the cost of re-rendering the newly-attended region under the same masking the
eye applies to a real saccade ([saccade masking, PMC5952294](https://pmc.ncbi.nlm.nih.gov/articles/PMC5952294/)).

---

## 7. HVS tricks ranked by bit-savings for a VDI desktop

Ranked by *expected whole-stream* impact on Infinibay's office-heavy fleet (doc 16 personas),
because a lever that governs 100% of a mostly-static screen beats one that governs the 10% that is
video. Stack them — they compose.

| Rank | Lever | Governs | Why it ranks here (VDI-specific) |
|---|---|---|---|
| **1** | **Damage-driven frame-skip + adaptive framerate** (§6) | whole stream, temporal | A static desktop is ~0 bits. Removes most frames outright — the single largest saving, and it's *free* accuracy (guest tells us what changed). Uniquely huge for desktops vs. games. |
| **2** | **Content routing: UI→screen-content path, video→codec** (§3) | whole frame, spatial partition | Keeps the giant flat-text majority out of the video codec entirely (near-lossless dirty-rect + palette), so the video codec only ever sees the small dynamic area. Also the legibility guarantee. |
| **3** | **Attention/foveated QP map** (§1,§2) | the video-coded region | >50% on that region; concentrates the video budget on the ~5–8° the user is actually looking at. Multiplier on rank 2's residue. |
| **4** | **Luma≫chroma: 4:2:0 default, 4:4:4 only for text** (§3,§4) | chroma of all coded regions | Structural ~50% chroma cut everywhere it's safe; 4:4:4 spent surgically on flagged text. |
| **5** | **CSF/JND + AQ + psy-RDO perceptual RC** (§4,§5) | within every coded block | ~14–40% on coded regions; already in the encoder — we just enable and target VMAF, not PSNR. |
| **6** | **Temporal & saccadic masking on motion** (§6) | motion/scroll/scene-cut frames | Spends less exactly when the eye can't tell; scene-cut frames ~20% bits; gaze-shift dips are free under saccadic omission. |
| **7** | **Luminance-adaptive (Weber) QP** (§4) | dark/bright blocks | Folds into AQ; a few % by coarsening where masking is strongest. |
| **8** | **YCoCg / blue-chroma compression + chroma@½ rate** (§4,§5) | chroma channel | Small (~0.5 dB) but structural and nearly free; blue-cone sparsity + 25 Hz color fusion justify sub-rate chroma. |
| **9** | **Flicker-fusion framerate ceiling** (§6) | temporal cap | Prevents over-sending; largely subsumed by rank 1's adaptive framerate. |

**The headline:** ranks 1–2 are *desktop-structural* and do most of the work; ranks 3–8 are the
*perceptual HVS layer* squeezing the remaining dynamic pixels. The whole stack is driven by two
artifacts we can build today from signals already flowing (doc 16 capacity budget, doc 17 attention
telemetry): a **saliency field** and a **per-block QP map** — both accepted directly by NVENC and
Vulkan Video, on-GPU, no readback. One caveat carried forward: emphasis-map mode disables NVENC AQ
and risks VBV violations, so we drive **delta-QP maps** and keep AQ for within-block masking.

## Sources

- Foveated Video Streaming for Cloud Gaming (>50% bandwidth, Gaussian eccentricity QP, <100 ms budget): https://ar5iv.labs.arxiv.org/html/1706.04804
- FovOptix — Human-Vision-Compatible Video Encoding & Adaptive Streaming in VR Cloud Gaming (ACM MMSys'24): https://dl.acm.org/doi/10.1145/3625468.3647612
- Frame-Complexity-Aware Foveated Video Encoding for Real-time High-Quality Streaming (IEEE 2025): https://ieeexplore.ieee.org/document/11457547/
- EyeNexus — Adaptive Gaze-Driven Quality/Bitrate Streaming for VR Cloud Gaming (ACM 2025): https://dl.acm.org/doi/10.1145/3768989
- Foveated Compression for Immersive Telepresence Visualization (arXiv 2510.19848, 2025): https://arxiv.org/pdf/2510.19848
- NVENC Video Encoder API Programming Guide 13.0 (qpMapMode, qpDeltaMap, NV_ENC_QP_MAP_DELTA/EMPHASIS, AQ exclusion, VBV caveat): https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html
- NVIDIA Dev Forums — qpDeltaMap in NVENC SDK samples (per-macroblock signed byte, raster order): https://forums.developer.nvidia.com/t/qpdeltamap-in-nvenc-5-0-1-sdk-samples/39121
- Khronos — Vulkan Video Encode AV1 & Encode Quantization Map extensions announcement (delta vs emphasis maps, H.264/H.265/AV1, Nov 2024): https://www.khronos.org/blog/khronos-announces-vulkan-video-encode-av1-encode-quantization-map-extensions
- VK_KHR_video_encode_quantization_map spec (delta map all RC modes; emphasis map RC-only): https://registry.khronos.org/vulkan/specs/latest/man/html/VK_KHR_video_encode_quantization_map.html
- SVT-AV1 Parameters — ROI per-64×64-block QP-offset map, segmentation/alternate-quantizer feature: https://gitlab.com/AOMediaCodec/SVT-AV1/-/blob/v1.9.0-rc1/Docs/Parameters.md
- A Technical Overview of AV1 (up to 8 segments, per-segment QP offset, superblock/block QP offset; arXiv 2008.06091): https://arxiv.org/pdf/2008.06091
- Perceptual Video Coding using JND / CSF (13.87% avg, up to 39.52%; MDPI Entropy 21(11):1095): https://www.mdpi.com/1099-4300/21/11/1095
- Visually Lossless Coding in HEVC — JND-based perceptual quantisation, 4:4:4/high-bit-depth (arXiv 1708.06417): https://arxiv.org/pdf/1708.06417
- On visual masking estimation for adaptive quantization using steerable filters (luminance/contrast masking → AQ; ScienceDirect): https://www.sciencedirect.com/science/article/abs/pii/S0923596521001235
- Codec Wiki — Psychovisual (psy-RDO/psy-RDOQ, AQ/QPA in x264/x265/SVT-AV1/VVenC, SSIMULACRA2/XPSNR/butteraugli/VMAF): https://wiki.x266.mov/docs/introduction/psychovisual
- SVT-AV1-PSY Parameters (--psy-rd): https://github.com/psy-ex/svt-av1-psy/blob/master/Docs/Parameters.md
- MS-RDPEGFX — YUV420p stream combination for YUV444 mode (AVC444 main+auxiliary packing): https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/8131c1bc-1af8-4907-a05a-f72f4581160f
- Azure Virtual Desktop — Graphics encoding over RDP (AVC420/AVC444, 4:4:4 for text): https://learn.microsoft.com/en-us/azure/virtual-desktop/graphics-encoding
- FreeRDP — Codecs (RemoteFX Progressive, ClearCodec, H.264 AVC420/AVC444, ZGFX; GFX pipeline hybrid): https://freerdp-freerdp.mintlify.app/concepts/codecs
- Overview of the Emerging HEVC Screen Content Coding Extension (palette mode + intra-block-copy for text/graphics; MERL TR2015-126): https://www.merl.com/publications/docs/TR2015-126.pdf
- Chroma subsampling (luma ~50 vs chroma ~25 cyc/deg; 4:2:0 ≈ 50% sample reduction; Wikipedia): https://en.wikipedia.org/wiki/Chroma_subsampling
- YCoCg color space (energy compaction, ~0.5 dB over YCbCr, ~4.5 dB over RGB lossless; Wikipedia): https://en.wikipedia.org/wiki/YCoCg
- Masking of temporal activity for video quality control (temporal-masking bit savings, scene-cut coarse coding; SAGE 2020): https://journals.sagepub.com/doi/full/10.1177/0020294020944949
- Motion Masking by Stationary Objects: A Study of Simulated Saccades (saccadic omission; PMC5952294): https://pmc.ncbi.nlm.nih.gov/articles/PMC5952294/
- Peripheral Flicker Fusion at High Luminance / Ferry–Porter (CFF ~50–90 Hz, higher with edges, color fusion ~25 Hz; PMC10057432): https://pmc.ncbi.nlm.nih.gov/articles/PMC10057432/
- Exploiting Change Blindness for Video Coding (arXiv 2408.00052): https://arxiv.org/pdf/2408.00052
