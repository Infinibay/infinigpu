# 27 — Client capability negotiation, client-GPU offload & loss resilience

**Scope.** ADR-0009 (infiniPixel) treats the browser as a decode-and-present endpoint: host renders
on a headless Vulkan context, NVENC/Vulkan-Video encodes, WebTransport/QUIC ships, WebCodecs decodes,
WebGL composites tiles + cropped video + a local cursor. The owner insight reframes that endpoint:
**the client is not a thin terminal — it is a PC with its own GPU** (hardware video codecs, 2D/3D
accel, and in-browser WebGPU/WebCodecs/WebNN). This doc designs the layer that *delegates* work to
that client GPU to (a) cut **host** GPU-time → more VMs per host GPU (density, ADR-0007), (b) cut
bandwidth, and (c) cut/hide latency — **negotiated by client capability, with a thin-client
fallback, and resilient to QUIC datagram loss.** The governing constraint: **offload is a bonus,
never a requirement, and the host remains the source of truth.**

## Verdict

**Feasible and worth building — as an opt-in, capability-gated overlay on the ADR-0009 datapath, not
a new datapath.** Every primitive exists in 2026 browsers: `VideoDecoder.isConfigSupported()` is the
honest hardware-decode probe, `MediaCapabilities.decodingInfo()` returns `{supported, smooth,
powerEfficient}` at a target resolution/fps, `navigator.gpu.requestAdapter()` exposes limits/features
for WebGPU compute, and the client already composites in WebGL (ADR-0009 §5). Client-side upscaling
(FSR/GameSR-class) and frame reprojection (VR async-spacewarp-class) are proven and portable to
WebGPU. The risk is entirely in the **policy** — deciding when a split *saves* host cost without
degrading the experience or the fail-closed posture — not in browser feasibility.

---

## 1. What the client GPU can actually do (2026 browser reality)

Three delegatable jobs, in increasing ambition, all downstream of decode:

1. **Spatial upscale.** Host encodes at reduced resolution; client reconstructs to native. Cloud
   gaming already does exactly this: **GameSR** runs a lightweight SR model on *encoded* game frames
   and reports **35–49 % bandwidth reduction** at target quality, up to 240 fps
   ([GameSR](https://openreview.net/forum?id=wnJkdo5Gu9)); AMD **FSR** is the reference spatial/ML
   upscaler ([FSR](https://gpuopen.com/amd-fsr-upscaling/)). In-browser this is a WebGPU compute pass
   (Lanczos/edge-directed for the cheap tier; a small SR net via WebGPU or WebNN for the rich tier).
2. **Temporal interpolation / reprojection.** Host sends at reduced fps; client synthesises the
   in-between frames. This is VR **Asynchronous Spacewarp** — the compositor "post-processes and
   converts motion vectors for frame extrapolation … predicting where pixels will be in the next
   frame," halving the render workload
   ([ASW, Meta](https://developers.meta.com/horizon/blog/asynchronous-spacewarp/)); FSR 3 does the
   same with optical-flow frame generation
   ([FSR3](https://gpuopen.com/manuals/fidelityfx_sdk/techniques/super-resolution-interpolation/)).
3. **Vector / drawing-command rendering.** The client already composites the reliable UI-tile lane
   (ADR-0009 §Damage-aware hybrid). RDP's Graphics Pipeline and Guacamole both push **drawing orders**
   (canvas-primitive ops) to the client instead of pixels — Guacamole's protocol "provides basic
   graphics operations similar to … the HTML5 canvas … [which] take up less bandwidth than sending
   corresponding PNG images" ([Guacamole protocol](https://guacamole.apache.org/doc/gug/guacamole-protocol.html);
   [MS-RDPEGFX](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0)).

The common thread: each **moves host GPU-time and/or bytes onto the client**, and each is *only*
available if the client can prove it can do the job. Hence negotiation first.

## 2. Capability negotiation — the CAPS handshake (extends ADR-0004 control ring)

### 2.1 Detection primitives (browser-API-concrete)

| Capability | API (2026) | Yields | Gotcha / verification |
|---|---|---|---|
| **HW video decode** | `VideoDecoder.isConfigSupported({codec, hardwareAcceleration:'prefer-hardware'})` | per-codec-string support + whether HW | The *only honest* check; HEVC needs the HEVC Video Extension on Windows, AV1 HW decode only on recent Intel/AMD/Apple ([MDN](https://developer.mozilla.org/en-US/docs/Web/API/VideoDecoder/isConfigSupported_static); [detect HW codecs](https://sigwait.org/~alex/blog/2025/02/17/1mJJHm.html)) |
| **Decode viability @ res/fps** | `MediaCapabilities.decodingInfo({video:{...}})` | `{supported, smooth, powerEfficient}` | `smooth=false` ⇒ don't push that res/fps; `powerEfficient` proxies HW/battery cost ([MDN](https://developer.mozilla.org/en-US/docs/Web/API/MediaCapabilities/decodingInfo)) |
| **Codec profiles** | codec string, e.g. `hev1.1.6.L153.B0` (HEVC), `av01.0.05M.10` (AV1 10-bit), `avc1.*` | 4:2:0/4:4:4, 8/10-bit, level | 4:4:4 / 10-bit HW decode is fleet-dependent → probe, don't assume |
| **WebGPU** | `navigator.gpu.requestAdapter()` → `adapter.limits`, `adapter.features`, `adapter.info` | `maxTextureDimension2D`, compute limits, vendor/arch | Limits are **tier-bucketed** to curb fingerprinting ([GPUSupportedLimits](https://developer.mozilla.org/en-US/docs/Web/API/GPUSupportedLimits)); adapter can be null |
| **WebNN** | `'ml' in navigator` (secure ctx) | NN accelerator entry point | CR only Jan 2026, **Chromium-only/experimental** → optional accelerator, never required ([W3C WebNN](https://www.w3.org/TR/webnn/); [compat](https://webnn.io/en/api-reference/browser-compatibility/api)) |
| **Display res / DPR** | `screen.width/height`, `devicePixelRatio` | target native resolution | authoritative for upscale target |
| **Refresh rate** | *no direct API* → estimate via `requestAnimationFrame` cadence | ~fps ceiling | approximate; drives pacing cap (ADR-0023) — **NEEDS VERIFICATION** at 120/144 Hz |
| **HDR / range** | `matchMedia('(dynamic-range: high)')`, `screen.colorDepth` | HDR-capable panel | Chrome/Edge/Safari/FF ship `dynamic-range` ([HDR MQ](https://chromestatus.com/feature/5680926106320896)) |
| **CPU / RAM** | `navigator.hardwareConcurrency`, `navigator.deviceMemory` | logical cores, RAM bucket | `deviceMemory` rounded to a power of 2 for privacy ([MDN NetInfo](https://developer.mozilla.org/en-US/docs/Web/API/NetworkInformation)) |
| **Battery** | `navigator.getBattery()` → `charging`, `level` | AC vs battery, charge | **restricted in Chrome 108+** → best-effort hint only |
| **Network (coarse)** | `navigator.connection.effectiveType/downlink/rtt` | 2g–4g class, Mbit/s, RTT | coarse WAN hint; the *real* signal is QUIC GCC in the loop (ADR-0023 §6) |

**Trust rule:** all of the above are **client-asserted hints**. The host validates them by observed
behaviour — decode ACKs, frame-arrival telemetry (`requestVideoFrameCallback`), a WebGPU device
actually acquired — and never lets a claim reduce host work until the client *proves* the capability
(§4, §6).

### 2.2 The message set (postcard control messages on the reliable stream)

The negotiation extends ADR-0004's per-device **control ring** (`NEGOTIATE`/`GET_CAPSETS`, TLV
skip-unknown, `postcard` variable-shape control) and rides ADR-0009's reliable WebTransport control
stream. Four messages:

- **`CLIENT_HELLO { ClientCaps, session_nonce }`** — sent once after the QUIC connection opens.
  `ClientCaps` = `{ decode:[{codec, hw, maxRes, smooth, powerEff}], webgpu:{present, tier, maxTex,
  features[]}, webnn:bool, display:{w,h,dpr,refreshHz,hdr}, host:{cores, memGB, battery, onAC},
  net:{effType, downMbps, rtt} }`.
- **`HOST_OFFER { SessionProfile }`** — host replies with the chosen `codec`, base `resolution`/`fps`,
  and an **`offload_mode`** (§3) plus the side-channels it will open (motion-vector datagrams,
  tile lane). This is the negotiated contract for the session.
- **`CAPS_UPDATE { delta }`** — client pushes runtime deltas that change the calculus: battery
  unplugged→on-battery, tab visibility/thermal throttle, `MediaCapabilities.smooth` flipping,
  display change. Cheap, frequent-tolerant.
- **`HOST_RECONFIG { SessionProfile }`** — host re-issues a profile in response to `CAPS_UPDATE` *or*
  its own budget/network loop. Ack'd; applied at the next frame-group boundary so nothing tears.

Capability negotiation is thus a *living* contract, not a one-shot handshake — which is exactly what
the adaptive loop (§3.3) needs.

## 3. Adaptive offload policy — the host/client work-split decision function

### 3.1 The offload modes (each is a host↔client split)

| Mode | Host does | Client does | Saves | Requires |
|---|---|---|---|---|
| **M0 — Thin** | full render + full-res encode | decode + WebGL composite + local cursor | nothing (baseline) | HW *or* SW decode only |
| **M1 — Client upscale** | render+encode at 0.5–0.67× res | WebGPU spatial upscale → native | host GPU-time **and** bandwidth | WebGPU + HW decode |
| **M2 — Client interpolate/reproject** | encode at ½ fps + emit motion vectors | synthesize in-between/late frames (ASW-style) | host encode GPU-time + hides fps cut | WebGPU + MV side-channel |
| **M3 — Client vector composite** | ship UI as drawing-orders/tiles, video cropped | render UI primitives + composite | host encode of the static lane ≈ 0 | WebGL (already ADR-0009) |
| **M4 — Client NN upscale/restore** | encode low-res/low-bitrate | WebNN/WebGPU SR net (GameSR-class) | max bandwidth + host cost | WebNN or capable WebGPU |

M1/M2/M4 are the density plays (host cost → client); M3 is the ADR-0009 tile lane, *already* a client
offload, here made explicit and extensible. Modes compose (e.g. M1+M2+M3 for a rich client on a
loaded host).

### 3.2 The decision function

```
choose_offload(clientCaps, net{B,RTT,L}, hostBudget C_from_ADR0007) -> mode set:
  base = M0                                            # always safe
  if not clientCaps.webgpu:      return {M0, M3}       # composite only
  # host pressure is the trigger to PUSH work to the client:
  if C.gpu_time_pressure high AND clientCaps.onAC AND net.B tight:
        add M1 (upscale)                               # low-res encode → client reconstructs
  if C.encoder_pressure high AND clientCaps.refreshHz>=host_fps*2 AND mv_channel_ok:
        add M2 (reproject)                             # ½-fps encode → client fills
  if clientCaps.webnn or clientCaps.webgpu.tier>=high AND net.B very tight:
        prefer M4 over M1                              # NN SR for max bandwidth cut
  # client pressure is the trigger to PULL work back:
  if not clientCaps.onAC (battery) or clientCaps.host.cores low or thermal:
        drop M2,M4; keep at most M1-light or fall to M0
  always keep M3 for the text lane (crisp glyphs, ~0 host encode)
```

Canonical outcomes from the brief: **capable client + loaded host → M1+M2** (send low-res, client
upscales and interpolates — host spends the least GPU-time while density is highest). **Weak client
or on battery → M0** (full host-side render+encode; the client only decodes). The **inversion** is
the point: unlike a pure degradation ladder, offload lets a *loaded host* stay high-quality by
spending the *client's* GPU instead of its own.

### 3.3 Plugging into the ADR-0023 adaptive control loop

The adaptive perceptual loop (research doc 23 → ADR-0009 *Adaptive control loop*) already runs
per-stream at a fixed cadence with inputs {network `B/RTT/L`, host budget `C`, persona} → knobs
{bitrate, res, fps, base-QP, foveation ΔQP, intra-refresh}. **`offload_mode` becomes one more knob**
in that same loop:

- Its **SENSE** step already has everything: `C` from the ADR-0007 broker, `B/RTT/L` from QUIC GCC,
  and now `clientCaps`/`CAPS_UPDATE` deltas.
- **ALLOCATE** gains a branch: instead of only *cutting* quality under pressure, it may *shift* the
  cut onto the client — e.g. under host contention pick M1 (encode at 0.67× res) and let the client
  restore, so the *perceived* resolution holds while host GPU-time drops. The GPU-time reclaimed is
  returned to the ADR-0007 broker's ledger under the same fairness accounting (offload lowers a
  tenant's cost; it can never raise a tenant *above* its budget).
- The §5-invariant of doc 23 — **steady cadence, local cursor never rubber-bands** — is *reinforced*
  by M2: client reprojection keeps a metronomic present clock even when a host frame is late.

Offload is therefore not a parallel system; it is a new column in the existing knob matrix, chosen by
the same controller, bounded by the same budget.

## 4. The fallback ladder — offload is a bonus, never a requirement

Detected at `CLIENT_HELLO`, re-checked on `CAPS_UPDATE`. Richest → poorest; **every rung renders a
correct desktop**, and downgrading is always safe because the host can always produce full pixels.

| Tier | Client profile | Session | Offload |
|---|---|---|---|
| **A** | WebGPU (high tier) + HW HEVC/AV1 10-bit + AC power + fast link | native res, foveal-high | M1+M2+M3 (+M4 if WebNN) |
| **B** | WebGPU + HW **H.264 only** | native res, no HDR/AV1 | M1 (upscale) + M3 |
| **C** | HW H.264 decode, **no WebGPU** | native res | **M0 + M3** (WebGL composite — ADR-0009 baseline) |
| **D** | **software** WebCodecs decode (no HW) | host caps to 720p/30 (`decodingInfo.smooth=false` ⇒ downshift) | M0 only |
| **E** | no WebCodecs *or* WebTransport blocked (proxy/old browser) | WebSocket/TCP + intra-only tiles; **SPICE legacy** rung | none |

Two hard rules keep offload a bonus:

1. **Downgrade is unconditional and immediate.** Any offload failure (WebGPU device lost, reproject
   diverges, MV channel stalls) drops the session *one rung toward M0* — to full host pixels, never to
   a blank or frozen view. Offload failure ⇒ *degrade*, never *deny* (fail-closed on quality, not on
   availability).
2. **Upgrade requires proof.** The host does not reduce its own render/encode until the client has
   *demonstrated* the capability: WebGPU adapter acquired, a probe upscale round-tripped, decode ACKs
   flowing. A client that over-claims in `CLIENT_HELLO` only ever harms *its own* session (host sees
   missing ACKs and steps down the ladder).

This mirrors ADR-0009's existing `infiniPixel(HW) → software-x264 → SPICE` fallback and doc 23's
worst-perceptual-loss-last ladder — offload sits *above* rung 0 as pure upside.

## 5. Loss resilience — the client GPU as jitter/loss shock-absorber

QUIC datagrams (ADR-0009 transport) can drop or arrive late. ADR-0009 already bounds a lost datagram
to **one NVENC slice** (`reportSliceOffsets`, slice-per-datagram). The client GPU turns that bounded
loss into *concealed* loss instead of a visible glitch. Two failure modes, two client-GPU responses:

**(a) Lost/late slice within a frame → error concealment.** Port classic decoder EC to a WebGPU pass.
The two established algorithms are **Frame-Copy EC** (reuse the co-located region from the previous
decoded frame) and **Motion-Compensated EC** (predict the lost macroblocks' motion vectors from
received neighbours and fetch the motion-shifted region)
([H.264 EC survey](https://repository.gatech.edu/server/api/core/bitstreams/c8a3bc23-d221-4ce6-800e-773c8300ffd7/content)).
Because our loss is a *contiguous slice band* (not scattered MBs), MCEC over the band's neighbours is
well-conditioned. The client conceals immediately, then — if the region is still wrong after the
concealment window — asks the host for an **on-demand intra-refresh wave, never an IDR** (ADR-0009/23:
a keyframe reintroduces the bitrate spike we designed out).

**(b) Dropped/late *whole* frame → reprojection.** Rather than stalling the present clock (the jitter
users feel worst, doc 23 §5), the client **synthesizes** a frame from the last good frame + motion,
exactly as VR async-spacewarp extrapolates during a render miss
([ASW](https://developers.meta.com/horizon/blog/asynchronous-spacewarp/)). This is precisely the
"async space warp for **remotely rendered** VR" pattern
([US 11,455,705](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/11455705)) applied
to a remote desktop: the client GPU *is* the jitter buffer's shock absorber — it manufactures a
plausible frame on time instead of showing a stale or blank one.

**Motion-vector sourcing (the enabler for both).** Good reprojection needs real motion, not guesses.
NVENC already computes per-block motion vectors during encode; export a compact per-block MV map on a
dedicated unreliable side-channel (small vs the video payload), so the client reprojects/conceals with
**true** motion — the ASW "motion-vector pass" ported to our datapath. *(NEEDS VERIFICATION: exact
NVENC MV-export path and its cost under the ULL config; and reprojection quality on 2D desktop content,
which lacks the depth buffer VR ASW relies on — desktop reproject may need damage-map masking to avoid
smearing static text. Bench before shipping M2.)*

**Bounding drift (correctness).** Conceal/reproject for **at most N frames** (≈2–3), then freeze-and-
request-refresh. Synthesized content must never drift far from truth: the host is the source of truth,
and the **reliable UI-tile lane** (ADR-0009) plus the periodic intra-refresh continuously re-anchor the
client to real pixels.

## 6. Security & correctness — offload must not break fail-closed

The new attack surface is "the client renders host-issued commands (composite/upscale/reproject/vector
orders)." Five rules keep it safe and keep the host authoritative:

1. **Client rendering is presentation-only; it never feeds back into host/guest state.** Input still
   travels host→guest over the authoritative channel; a wrong upscale or a mis-reprojected frame can
   corrupt *only the local view*, not the VM. This is the RDP/Guacamole model — the client draws;
   authority stays server-side.
2. **The host stays source of truth by construction.** A steady stream of ground-truth pixels
   (intra-refresh waves + the reliable tile lane) re-anchors the client every few frames, so any
   client-side synthesis is self-correcting and time-bounded (§5). Offload cannot make the displayed
   state *persistently* diverge from what the host rendered.
3. **Security-relevant UI is never left to speculative client pixels.** Text/credential prompts ride
   the **reliable, near-lossless tile lane** at host-decided fidelity; client SR/interpolation applies
   to the *video/dynamic* regions, and where it touches UI it is bounded by the ground-truth tile as
   the authority — a client cannot blur, drop, or fabricate a security prompt the host didn't send.
4. **Capability claims are untrusted input.** `CLIENT_HELLO` fields are validated against observed
   behaviour and clamped **server-side** (never size a host-driven buffer to a client-reported
   `maxTextureDimension2D` without a server cap). An over-claiming or malicious client only degrades
   *its own* session — the host detects missing decode/reproject ACKs and steps down the ladder to M0.
5. **Offload failure is fail-closed on *quality*, open on *availability*.** Any offload-channel fault
   falls back to full host pixels (§4 rule 1) — degrade, never deny, never blank. Crucially, offload
   lives **downstream of the display datapath**, entirely separate from the command/authorization
   channel: the backend's HMAC-signed host→guest command boundary (`AgentMessageSigner` /
   infiniservice `auth.rs`) and the ADR-0007 admission/fairness accounting are untouched. Reclaimed
   host GPU-time is returned to the broker under the same per-tenant budget, so offload can *lower* a
   tenant's cost but never let it exceed quota, and the browser sandbox keeps each session's
   client-side rendering isolated from other tenants.

**Net:** capability negotiation makes the split *safe by consent*, the fallback ladder makes it *safe
by degradation*, and the presentation-only + re-anchoring rules make it *safe by authority* — the
host renders the truth, the client GPU only ever makes that truth cheaper, faster, or smoother.

## Sources

- MDN — `VideoDecoder.isConfigSupported()` (honest HW-decode probe, codec strings): https://developer.mozilla.org/en-US/docs/Web/API/VideoDecoder/isConfigSupported_static
- Detect Hardware Video Codecs in Chrome (isConfigSupported + prefer-hardware; HEVC/AV1 platform gating): https://sigwait.org/~alex/blog/2025/02/17/1mJJHm.html
- MDN — `MediaCapabilities.decodingInfo()` ({supported, smooth, powerEfficient}): https://developer.mozilla.org/en-US/docs/Web/API/MediaCapabilities/decodingInfo
- MDN — WebCodecs API (HW decode H.264/HEVC/AV1, SW fallback): https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API
- MDN — `GPU.requestAdapter()` / GPUAdapter (limits, features, info): https://developer.mozilla.org/en-US/docs/Web/API/GPU/requestAdapter
- MDN — GPUSupportedLimits (maxTextureDimension2D; tier-bucketing vs fingerprinting): https://developer.mozilla.org/en-US/docs/Web/API/GPUSupportedLimits
- W3C — Web Neural Network API (WebNN; CR Jan 2026): https://www.w3.org/TR/webnn/
- WebNN browser compatibility (Chromium-only, experimental): https://webnn.io/en/api-reference/browser-compatibility/api
- ChromeStatus — HDR CSS Media Queries (`dynamic-range: high`): https://chromestatus.com/feature/5680926106320896
- MDN — Network Information API (`effectiveType`/`downlink`/`rtt`; `deviceMemory` rounding): https://developer.mozilla.org/en-US/docs/Web/API/NetworkInformation
- GameSR — real-time client-side SR on encoded frames (35–49% bandwidth cut, up to 240 fps): https://openreview.net/forum?id=wnJkdo5Gu9
- AMD FidelityFX Super Resolution (FSR) — ML spatial upscaling: https://gpuopen.com/amd-fsr-upscaling/
- AMD FSR 3 — frame generation via interpolation + optical flow (GPUOpen): https://gpuopen.com/manuals/fidelityfx_sdk/techniques/super-resolution-interpolation/
- Meta — Asynchronous Spacewarp (motion-vector frame extrapolation, halves render workload): https://developers.meta.com/horizon/blog/asynchronous-spacewarp/
- UploadVR — Timewarp/Spacewarp/Reprojection explained: https://www.uploadvr.com/reprojection-explained/
- US 11,455,705 — Asynchronous space warp for **remotely rendered** VR (remote reprojection): https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/11455705
- H.264 error-concealment survey (Frame-Copy vs Motion-Compensated EC; temporal concealment): https://repository.gatech.edu/server/api/core/bitstreams/c8a3bc23-d221-4ce6-800e-773c8300ffd7/content
- MS-RDPEGFX — RDP Graphics Pipeline Extension (drawing/graphics data encode→client decode): https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0
- Azure Virtual Desktop — RDP graphics encoding (content classifier: image processors + codec): https://learn.microsoft.com/en-us/azure/virtual-desktop/graphics-encoding
- Apache Guacamole — protocol (client-side canvas-primitive drawing orders, less bandwidth than PNG): https://guacamole.apache.org/doc/gug/guacamole-protocol.html
- infinigpu ADR-0004 — wire protocol & shared crate (control ring, NEGOTIATE/GET_CAPSETS, postcard, TLV): ../decisions/0004-wire-protocol-and-shared-crate.md
- infinigpu ADR-0007 — VDI capacity manager (GPU-time budget, admission/fairness, degradation ladder): ../decisions/0007-vdi-capacity-manager-and-scheduler.md
- infinigpu ADR-0009 — infiniPixel remote protocol (damage-aware hybrid, WebCodecs/WebGL client, fallback ladder): ../decisions/0009-infinipixel-remote-protocol.md
- infinigpu doc 18 — display datapath (NVENC/QUIC/WebCodecs, slice-per-datagram, ~14–22 ms): ./18-remote-protocol-display-datapath.md
- infinigpu doc 23 — perceived latency & adaptive control loop (SENSE→knobs, cursor/pacing invariant): ./23-perceived-latency-and-adaptive-control.md
