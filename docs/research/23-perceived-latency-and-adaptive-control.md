# 23 — Perceived Motion-to-Photon Latency & the Adaptive Perceptual Control Loop

**Scope.** Doc 09 pinned the *wire* budget at ~40–70 ms motion-to-photon on LAN and showed
encode is only 2–5 ms of it — cadence, jitter buffer, and client display dominate. This doc
attacks the term doc 09 left open: **perceived** latency, which is not the same number. We
can make the desktop *feel* near-instant while the pixels still take 40 ms, by decoupling the
most-watched objects from the frame pipeline, speculating locally, and prioritising where the
eye actually is. Then it designs the **closed control loop** that spends a joint
perceptual/bit/GPU budget across codec QP maps, foveation, resolution, and framerate — reacting
to network, to the host capacity manager (doc 16), and to persona/use-case. We have no
eye-tracking hardware, so "attention" is approximated from the cursor, the foreground window,
and the guest's damage rectangles (doc 17). Every encoder knob named here is one NVENC/AV1
actually exposes.

## Verdict

**CONFIRMED and high-value.** The single biggest perceived-latency win — **client-side local
cursor** — is real, cheap, and already half-built: our wire protocol carries a dedicated
hardware-cursor sub-channel (doc 11 §3, doc 09 §5) we can forward to a client overlay instead of
compositing into the encoded frame. Input echo/prediction and foveal-first transmission are
established techniques with published bit-savings (50–63 % at just-noticeable-distortion for
foveation). The adaptive loop is a well-trodden control problem (WebRTC GCC + cloud-gaming QoE
controllers); our novelty is **jointly** driving it from *three* pressure sources — network, GPU
contention, persona — with **perceptual degradation as the single currency of degradation.**

---

## 1. The perceived input→photon budget — where each trick buys ms

The honest wire budget (doc 09 §7) is ~40–70 ms LAN. Below 20 ms motion-to-photon is
imperceptible; sub-100 ms is the "playable" threshold cloud gaming targets
([JitBright/MTP thresholds](https://dl.acm.org/doi/10.1145/3651863.3651881)). We cannot beat
physics on the wire, so we cut the *perceived* number by removing the pipeline from the objects
the user is actually judging latency by. **Perceived** savings below are engineering estimates
tied to the doc-09 stage costs (NEEDS VERIFICATION against our own instrumentation, §7):

| Trick | What it decouples | Perceived ms saved | Applies to |
|---|---|---|---|
| **Local cursor** | pointer motion from the entire encode/net/decode pipeline | ~**40–70 → ~1–2 ms** for the pointer | all sessions, always |
| **Input echo/prediction** | scroll/type/drag feedback from one RTT + frame cadence | ~**30–50 ms** for the predicted class | office/knowledge; off for CAD viewport |
| **Progressive / foveal-first** | time-to-legible from full-frame refine | ~**one frame (8–16 ms)** to first meaningful pixel | scene changes, window switches |
| **Intra-refresh** | keyframe bitrate spike from steady cadence | removes ~**tens-of-ms jitter** at bitrate cap | all encoded streams |
| **Consistent pacing** | *variance*, not mean | trades ~3–8 ms mean for large jitter cut | all; jitter feels worse than latency |

The rest of this doc is these five rows plus the loop that arbitrates them.

## 2. Client-side local cursor — the biggest single win

**The pointer is the object humans judge latency by.** A remote cursor that lags the video
"rubber-bands" and is instantly noticeable, because the visuomotor system is exquisitely tuned
to hand-eye delay when aiming at a UI target — the exact request that drove Moonlight to render
a client-side cursor "like RDP does"
([moonlight-qt #465](https://github.com/moonlight-stream/moonlight-qt/issues/465)). RDP,
Parsec, and Moonlight all draw a *local* cursor decoupled from the frame stream; this alone
takes pointer latency from the full 40–70 ms budget down to client input→display (~1–2 ms).

**How it feeds off our existing plane.** We already keep the cursor off the encoded video
plane: virtio-gpu's dedicated cursor virtqueue (`CURSOR_UPDATE`/`CURSOR_MOVE`, doc 11 §3) is a
hardware-cursor plane, so shape/hotspot updates arrive out-of-band and never trigger a re-encode
(doc 09 §5). The change is purely on the transport/client edge:

1. Host arbiter forwards cursor-plane events (bitmap + hotspot + visibility) over the control
   channel to the browser client instead of (or in addition to) blitting them into the scanout.
2. The client renders the cursor as a **DOM/canvas overlay** positioned by its *own* local
   mouse coordinates, updated every input event — zero server round-trip for motion.
3. The server-reported cursor *position* is used only to reconcile warps/teleports, never for
   normal motion.

**Caveats that force the local cursor off** (fall back to server-composited cursor): relative-mouse
/ pointer-lock mode (captured FPS input, absolute-mapped drawing tablets), apps that **hide or
warp** the cursor (games, some CAD tools), and custom animated cursors the client can't render
faithfully. The guest-intelligence layer (doc 17) already reports foreground-app class and can
flag "cursor captured/hidden" so the client swaps modes without guessing. This is the cheapest ms
in the program and belongs in the Phase-1 browser player from day one.

## 3. Client-side input echo & prediction with reconciliation

For everything that isn't the pointer, borrow the game-networking pattern: **predict locally,
reconcile with the authoritative frame.** The thin client "predicts what to draw at the endpoint
and renders that prediction without waiting" for the server, giving immediate local echo, then
"reconciles its prediction with the information provided" and corrects if wrong
([speculative rendering, US 11,489,845](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/11489845);
[Gambetta, client-side prediction & reconciliation](https://www.gabrielgambetta.com/client-side-prediction-server-reconciliation.html)).
Three desktop-VDI cases pay off, in decreasing safety:

- **Scroll** — the safest. On a wheel/trackpad scroll the client can **shift the last decoded
  frame** by the scroll delta immediately and reveal a low-detail placeholder at the incoming
  edge, then snap to the real frame when it arrives. Scroll is spatially predictable; a small
  mispredict is a one-frame correction.
- **Typing / local echo** — the client shows the keystroke in a recognised text field right away
  "without waiting for the surrogate browser," then reconciles
  ([speculative rendering](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/11489845)).
  Riskier (caret position, IME, autocomplete) — gate to plain single-line inputs the client can
  detect, or ship as opt-in.
- **Drag** — reuse the local-cursor overlay to drag a *ghost* of the grabbed object; the server
  frame confirms the drop. Cheap because the ghost is client-drawn.

**Reconciliation rule:** every predicted action carries a sequence number; when the
authoritative frame for that seq arrives, the client cross-fades prediction→truth. Persist a
short ring of predicted deltas; on mispredict, roll forward from the last confirmed frame.
**Persona gate:** enable prediction for office/knowledge personas; **disable it for the CAD/3D
viewport**, where a wrong speculative transform is worse than a truthful 40 ms — the persona
already comes from doc 16's tiering and doc 17's foreground-app class.

## 4. Progressive / priority transmission — send where the eye is, first

**Two ideas, both driving the same delta-QP map.** (a) *Foveal-first, temporally*: on a scene
change or window switch, send a **coarse full frame** in the first encode slot so *something*
legible paints within one frame time, then refine — cutting time-to-meaningful-pixel below
time-to-perfect-frame. (b) *Foveal, spatially*: spend bits where acuity is highest and starve
the periphery. Human acuity drops sharply outside the ~2° fovea, and foveated video coding
exploits this for large savings: an H.264 foveated scheme hit **63 % bitrate savings at the
just-noticeable-distortion point**, and the cloud-gaming study reports **>50 %** with a 2-D
Gaussian QP-offset map `QO(i,j) = QO_max·(1 − exp(−((i−x)²+(j−y)²)/(2W²)))`, `W≈FW/8`
([Foveated Video Streaming for Cloud Gaming, Illahi et al.](https://ar5iv.labs.arxiv.org/html/1706.04804);
[EyeNexus](https://arxiv.org/html/2509.11807v1)).

**We have no eye tracker — approximate attention.** Software-only gaze prediction from saliency +
motion cues is a live research area (GazeProphet: 3.83° median error, beating saliency baselines
by 24 % ([GazeProphet](https://arxiv.org/html/2508.13546))). Our attention prior is *stronger*
than blind saliency because we own the guest: the **cursor position**, the **foreground-window
rectangle**, and the **damage rectangles** the guest driver already reports (doc 17) form a
high-confidence desktop attention map — the user looks at the caret, the active window, and what
just changed. So `attention(x,y)` = weighted union of {cursor neighbourhood, foreground-window
bbox, recent damage} → drives `QO_max`/`W` of the foveation map. Because our "fovea" is a soft
region around cursor/active-window rather than a tracked gaze point, mispredict cost is low (the
periphery is *desktop chrome*, not the text being read).

**How it drives the encoder (concrete).** The foveation map is realised as a per-macroblock QP
map, not a resolution change:

- **NVENC**: `NV_ENC_RC_PARAMS::qpMapMode = NV_ENC_QP_MAP_EMPHASIS` with
  `NV_ENC_PIC_PARAMS::qpDeltaMap` (signed byte per macroblock, `NV_ENC_EMPHASIS_MAP_LEVEL`);
  gate on `NV_ENC_CAPS_SUPPORT_EMPHASIS_LEVEL_MAP`. Emphasis raises quality in-region; the
  delta-QP variant (`NV_ENC_QP_MAP_DELTA`) lets us also *penalise* the periphery for harder
  savings ([NVENC prog. guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html)).
- **AV1 (SVT-AV1 / cross-vendor path, doc 18)**: up to 8 segments via the alternate-quantizer
  segment feature, or a per-64×64-block QP-offset map; when ROI and variance-AQ are both on, the
  ROI map wins ([SVT-AV1 AQ/ROI](https://github.com/spawlows/SVT-AV1/blob/master/Docs/Appendix-Variance-Based-Adaptive-Quantization.md)).

**Intra-refresh replaces the keyframe.** A periodic I-frame is 5–10× a P-frame; at a CBR cap it
queues behind the VBV and injects a multi-frame latency *spike* — exactly the jitter §5 says is
perceived worst. **Gradual Decoder Refresh** spreads intra macroblocks across many frames
instead, avoiding the bitrate spike for "smoother, more consistent bitrate … reduced end-to-end
delay" ([AMP intra-refresh](https://www.ampltd.com/blogs-the-advantages-of-using-intrarefresh-in-video-encoding/)).
NVENC: `enableIntraRefresh=1`, `intraRefreshPeriod`, `intraRefreshCnt`
([NVENC guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html)).
Combined with reference-frame invalidation (§6), we almost never send a full IDR.

## 5. Consistent pacing beats absolute-minimum latency

**Jitter is perceived worse than steady latency.** The dominant cause of inflated client-side
motion-to-photon is *receive-to-composition* latency from jitter-buffer management, not the
network mean; adaptive schemes cut it hard — JitBright reaches ~30 ms R2C (vs 100 ms+ for naive
buffers) and PASync cut average buffering latency 43.5 % / tail 33.9 % with **no perceptible
quality loss** in a 12,000-user A/B test
([JitBright](https://dl.acm.org/doi/10.1145/3651863.3651881);
[PASync perception-aware scheduling](https://dl.acm.org/doi/10.1145/3798065.3798067)). Design
consequences:

- **Adaptive jitter buffer, not fixed.** Size the buffer to a running estimate of network jitter
  (a few frames), shrink aggressively when the link is steady, grow only under measured variance.
- **Pace the encoder to the client's refresh** and skip unchanged frames on damage (doc 09 §5) —
  a steady 60 fps cadence with dropped-duplicate frames beats a bursty "fast when it can" stream.
- **Prefer a slightly higher, *stable* mean** over a lower, jittery one. A metronomic 45 ms feels
  more responsive than a 30–70 ms sawtooth. This directly shapes the controller objective (§6):
  latency *variance* is a first-class penalty term.

## 6. The adaptive perceptual control loop

One closed loop per stream, running on the host encoder path, arbitrated against the broker.
It mirrors the doc-16 control-loop idiom (fixed cadence, explicit inputs → knobs → objective).

**Inputs.**
- **Network** (WebRTC-style): available-bandwidth estimate `B`, RTT, loss `L` from Google
  Congestion Control — a delay-based estimator (trendline filter over transport-cc arrival
  times) and a loss-based estimator, taking the **min** of the two, updated 10–20× /s
  ([GCC/TWCC](https://bloggeek.me/webrtcglossary/transport-cc/);
  [BW estimation in WebRTC](https://www.forasoft.com/learn/video-streaming/articles-streaming/webrtc-bandwidth-estimation)).
- **GPU capacity** (doc 16): the broker's per-VM **encoder budget** and contention signal
  `C = f(Σ gpu_busy, vram_pressure, encoder_util)`. Under contention the broker tells this loop
  to *spend fewer perceptual bits* — the coupling is explicit, not implicit.
- **Persona / use-case**: tier (office/knowledge/CAD, doc 16 §1) + foreground-app class + damage
  statistics (doc 17). Determines the *shape* of the utility, not just the budget.

**Knobs.** target bitrate `R`; resolution; framerate `f`; base QP; the **foveation ΔQP map**
(`QO_max`, `W`, §4); intra-refresh cadence. All are continuous or few-valued and map onto real
NVENC/AV1 fields (§4).

**Objective.** Maximise perceived quality per bit *and* per ms:

```
maximise  U = w_p(persona)·PerceptualQuality(knobs, attention)
              − λ_lat·E[latency] − λ_jit·Var[latency]     # §5: variance is penalised
subject to R ≤ min(B, encoderBudget(broker)),  f ≤ f_client,  vram/gpu-time within broker caps
```

`PerceptualQuality` is persona-weighted: for **office/text** it weights **edge/text fidelity**
(sharp glyphs, no ringing) and tolerates low framerate; for the **CAD viewport** it weights
spatial detail *in the foreground window* and smooth interaction (no speculation); for **video
(Teams/YouTube)** it weights framerate + temporal smoothness and tolerates softer stills.

**Reaction to congestion (WebRTC discipline).** On `B` dropping (delay gradient rising or loss),
apply an AIMD-style cut and **shed knobs in persona-ranked order**, cheapest-perceptual-loss
first: (1) tighten peripheral ΔQP (foveate harder — free from the reader's view); (2) drop
**framerate** before resolution for text personas (a static page at 20 fps reads fine), drop
**resolution** before framerate for video; (3) only then raise base QP. **Never answer loss with
an IDR** — invalidate the damaged reference and re-reference a good long-term frame (§6 below),
because a keyframe reintroduces the spike we removed in §4.

**Reaction to the capacity manager (doc 16).** The broker's degradation ladder (doc 16 §7) is
expressed here as perceptual knobs, and this loop is the *actuator* for its non-foreground rungs:
`C` rising → cap background-desktop `f`; `C` high → raise peripheral ΔQP / lower `R` on
non-foreground; severe → drop resolution of non-foreground streams last. Foreground office
desktops are touched last and restored first — identical priority order to the broker, so the two
loops never fight.

```
every frame-group (~every 4–8 frames, ≤50 ms):
  1. SENSE   : B,RTT,L ← GCC;  budget,C ← broker;  persona,fgWindow,damage ← doc 17
  2. ATTEND  : attention_map ← union(cursor, fgWindow, recent_damage)  # §4
  3. BUDGET  : R_target ← min(B, encoderBudget);  clamp by broker ladder rung
  4. ALLOCATE: base_QP ← rateCtl(R_target,f);  ΔQP_map ← foveate(attention, QO_max(C,persona))
  5. PACE    : f ← min(f_client, personaCap);  jitterBuf ← estimate(RTT_var)   # §5
  6. RESILIENCE: on loss → invalidateRefs + LTR (never IDR);  intra-refresh cadence from L
  7. APPLY   : push qpDeltaMap + rcParams + f to NVENC/AV1;  emit chosen knobs as telemetry
```

## 7. Online quality metric, A/V sync, and empirical validation

**Close the loop with a live perceptual metric — but edge-aware for text.** VMAF is the
reference perceptual metric (>0.92 correlation with subjective scores vs ~0.70 PSNR) and its
2026 models run faster and add banding awareness
([VMAF](https://www.zegocloud.com/blog/vmaf-video-multimethod-assessment-fusion)). But VMAF is
tuned for natural video; a VDI desktop is **screen content** where **text legibility** is the
quality axis users care about, and generic VMAF under-weights glyph edges. So: run a lightweight
VMAF (or its edge/detail sub-features) on the foveal region as the loop's quality estimate, and
add an **edge-preservation / text-sharpness** term for text personas (gradient-domain fidelity in
the foreground window). This is the online signal that tells the controller whether the current
`base_QP`/foveation is actually legible before pushing harder. (NEEDS VERIFICATION: which
screen-content-aware metric is cheap enough to run per-stream at scale — candidate is a small
edge-aware sub-metric, not full VMAF, on the attention region only.)

**A/V sync.** For the video persona, keep audio within ITU-R BT.1359 bounds: detectable at
**+45 ms (audio early) / −125 ms (audio late)**, unacceptable beyond **+90 / −185 ms**, and the
brain tolerates late audio far more than early (light-before-sound is natural)
([ITU-R BT.1359 lip-sync](https://www.tvtechnology.com/opinions/av-synchronization-how-bad-is-bad)).
Practical rule: timestamp audio and video to a common clock and, when we must slip, **let audio
lag rather than lead**, staying inside −125 ms.

**Empirical input→photon validation.** Don't trust the model; measure. Standard MTP methodology
uses a **photodiode on the client display** plus an instrumented input event, a **high-speed
camera** co-registering an input LED with the on-screen response, or frame-counting
([MTP measurement survey](https://link.springer.com/article/10.3758/s13428-022-01983-5)). Our
plan: (1) a **synthetic-input harness** injecting a timestamped click/scroll that flips a known
screen region, with a photodiode/high-speed camera capturing the photon time → true
click-to-photon distribution (mean *and* variance); (2) in production, in-band telemetry
correlating input-event seq → decoded-frame seq via the client's `requestVideoFrameCallback` and
WebRTC `getStats`. We report the **tail** (p95/p99), not just the mean, because §5's whole thesis
is that variance is what users feel.

## 8. Integration & the fallback ladder

**How this ties the program together.** The control loop is the *brain* on top of the datapath:
it consumes doc 18's codec datapath (NVENC now, Vulkan Video cross-vendor later) and doc 22's
perceptual-compression primitives (the ΔQP/foveation maps are doc 22's currency, this loop
*schedules* them), it is *steered* by doc 17's guest intelligence (foreground app, active
monitor, damage → the attention map and persona gates), and it is *bounded* by doc 16's capacity
manager (encoder budget + contention → how many perceptual bits it may spend). **Perceptual
degradation is the single currency of graceful degradation:** under GPU contention *or* network
congestion, the same knob ladder pays the bill, worst-perceptual-loss last, foreground-office
protected — one policy, two pressure sources.

**Fallback ladder (perceptual, worst-first, foreground last):**

| Rung | Trigger (network `B↓`/loss OR broker `C↑`) | Action |
|---|---|---|
| 0 | healthy | native res, client refresh, foveal quality high, local cursor + prediction on |
| 1 | mild | foveate periphery harder (raise ΔQP outside attention); intra-refresh cadence up |
| 2 | moderate | drop **framerate** (text) / **resolution** (video) on non-foreground; keep foreground crisp |
| 3 | high | raise base QP on non-foreground; disable input prediction (keep local cursor) |
| 4 | severe | lower foreground resolution; **still** local cursor + steady pacing (never rubber-band) |
| 5 | link collapse / no NVENC | software x264 zerolatency fallback (doc 09 §8), 2D-desktop SPICE path for idle |

The invariant across every rung: **the cursor stays local and the cadence stays steady.** We
spend spatial detail, framerate, and speculation in that order — but never the two things the
user feels as "responsiveness." That is the whole design: physics keeps the photons ~40 ms away,
and perception never notices.

## Sources

- moonlight-qt #465 — render cursor locally to reduce apparent input latency (RDP-style): https://github.com/moonlight-stream/moonlight-qt/issues/465
- Speculative rendering / local echo & reconciliation for thin clients (US 11,489,845): https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/11489845
- Gambetta — client-side prediction & server reconciliation: https://www.gabrielgambetta.com/client-side-prediction-server-reconciliation.html
- Illahi et al. — Foveated Video Streaming for Cloud Gaming (Gaussian QP offset, >50% savings): https://ar5iv.labs.arxiv.org/html/1706.04804
- EyeNexus — adaptive gaze-driven quality/bitrate for VR cloud gaming: https://arxiv.org/html/2509.11807v1
- GazeProphet — software-only gaze prediction (3.83° median, no eye tracker): https://arxiv.org/html/2508.13546
- NVENC Video Encoder API Programming Guide (emphasis/delta QP map, intra-refresh, ULL RC, LTR): https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html
- SVT-AV1 — variance-based adaptive quantization & ROI segments (8 segments, alt-Q): https://github.com/spawlows/SVT-AV1/blob/master/Docs/Appendix-Variance-Based-Adaptive-Quantization.md
- AMP — advantages of intra-refresh (GDR) in video encoding (no bitrate spike, lower latency): https://www.ampltd.com/blogs-the-advantages-of-using-intrarefresh-in-video-encoding/
- WebRTC transport-cc / Google Congestion Control (delay+loss, min, 10–20 Hz): https://bloggeek.me/webrtcglossary/transport-cc/
- Bandwidth estimation & congestion control in WebRTC (GCC trendline filter): https://www.forasoft.com/learn/video-streaming/articles-streaming/webrtc-bandwidth-estimation
- JitBright — low-latency mobile cloud rendering via jitter-buffer optimization (~30 ms R2C): https://dl.acm.org/doi/10.1145/3651863.3651881
- Perception-aware frame display scheduling for low-latency cloud gaming (PASync, −43.5% buffering): https://dl.acm.org/doi/10.1145/3798065.3798067
- VMAF overview (perceptual metric, >0.92 correlation, 2026 models): https://www.zegocloud.com/blog/vmaf-video-multimethod-assessment-fusion
- ITU-R BT.1359 lip-sync thresholds (+45/−125 detect, +90/−185 accept): https://www.tvtechnology.com/opinions/av-synchronization-how-bad-is-bad
- Measuring motion-to-photon latency (photodiode / high-speed camera methodology): https://link.springer.com/article/10.3758/s13428-022-01983-5
- Saccadic suppression window (30–50 ms pre to 50–100 ms post): https://en.wikipedia.org/wiki/Saccadic_masking
- Reference picture invalidation for loss recovery without keyframe (US 9,813,193, error resilience): https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/9813193
- infinigpu doc 09 — Presentation Path & Latency Budget (40–70 ms LAN, cursor plane, pacing): ./09-presentation-latency.md
- infinigpu doc 16 — Host-Side Brain: capacity manager & degradation ladder: ./16-vdi-workload-and-host-scheduler.md
- infinigpu doc 11 — OS-neutral wire protocol (cursor sub-channel, control ring): ./11-wire-protocol-design.md
