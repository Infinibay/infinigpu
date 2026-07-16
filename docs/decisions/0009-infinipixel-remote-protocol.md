# ADR 0009 — infiniPixel: custom low-latency remote protocol (+ perceptual layer)

- **Status:** accepted
- **Date:** 2026-07-16
- **Feeds from:** research/18-remote-protocol-display-datapath.md, research/19-remote-protocol-io-and-integration.md, research/22-perceptual-hvs-compression.md, research/23-perceived-latency-and-adaptive-control.md, research/09-presentation-latency.md

## Context

Owner directive: we are **not** locked to SPICE; if a purpose-built protocol drastically cuts
latency, design it — using **any** trick, including **human-vision/optics** (what the eye is and
isn't sensitive to). We control all three ends (guest driver, host arbiter with the frame already
on-GPU, browser client), which SPICE cannot exploit: SPICE reads every frame **back to system RAM**,
**CPU-encodes** in spice-server, ships over **TCP**, to a **native `.vv` viewer**.

## Decision

**Build "infiniPixel" — a purpose-built, perceptually-driven, low-latency display datapath that
replaces SPICE's GPU display path** (SPICE retained as a fallback rung and, short-term, for peripheral
channels). Target **~14–22 ms LAN motion-to-photon** (vs SPICE's readback + CPU-encode + TCP).

### Codec & encode (zero-readback, on-GPU)
- **NVENC HEVC primary, H.264 universal fallback.** **GA102/Ampere cannot encode AV1** (Ada+ only) —
  so codec is **negotiated per session**; AV1 where the host GPU supports it (Ada NVENC, or RADV
  Vulkan-Video). Cross-vendor path = **Vulkan Video** (ADR 0008).
- **Ultra-low-latency config:** infinite GOP / IPPPP / no B-frames / no look-ahead / delay=0;
  **intra-refresh (GDR)** instead of periodic IDR (no keyframe bitrate spikes — the top VDI knob);
  single-frame-VBV CBR; **slice-based** (reportSliceOffsets) to pipeline packetization and bound loss
  to one slice. Feed `VkImage → cuImportExternalMemory → NVENC` (or Vulkan Video, same VkDevice) — the
  frame never leaves the GPU. ~1–3 ms encode on Ampere.

### Damage-aware hybrid (the VDI structural win)
Two client-composited lanes driven by the guest damage map (ADR 0007 / doc 17):
- **UI/text → screen-content path:** near-lossless dirty-rect tiles, **4:4:4** (AVC444 only on flagged
  text tiles), reliable, client-cached. Legibility is a hard constraint.
- **Video/3D → codec path:** cropped dynamic regions, 4:2:0 + foveated QP, unreliable datagrams.
- **Idle desktop ⇒ empty damage map ⇒ ~0 bits and ~0 encode** — the ~100–1000× common-case win that
  lets one A5000's **single NVENC block** fan out to dozens of desktops (GA102/A5000 = **1× NVENC +
  2× NVDEC**; dual-NVENC is Ada+ — the NVENC engine is a scarce, first-class admission resource, ADR 0007).

### Perceptual / HVS layer (ranked by VDI payoff)
1. **Temporal:** damage-driven frame-skip (present clock = damage, not vsync) + adaptive framerate
   under a flicker-fusion ceiling — the #1 lever, also cuts encoder GPU-time (ADR 0007 budget).
2. **Structural:** UI→screen-content vs video→codec routing (above).
3. **Foveated attention (no eye-tracker):** a GPU-resident **saliency field** fused from cursor
   (strongest desktop gaze predictor) + focused-window/active-monitor + damage, **damage-gated** so a
   changing periphery is never blurred → a **per-block QP delta map** via `NV_ENC_QP_MAP_DELTA` (keeps
   AQ; *not* emphasis mode — it disables AQ + risks VBV) / Vulkan Video `VK_KHR_video_encode_
   quantization_map`. On-GPU, no readback.
4. **Chroma** (4:2:0 default, 4:4:4 text), **CSF/JND + encoder psy-RC/AQ**, temporal/saccadic masking,
   Weber luminance AQ, YCoCg, flicker ceiling — stackable multipliers on the dynamic fraction.
- **Metric:** target **VMAF / SSIMULACRA2** (never PSNR — it rewards blurring text) with a hard
  **edge-fidelity gate on text tiles**.

### Perceived-latency tricks (beyond wire latency)
Client-side **local HW-cursor sprite** (pointer moves at input latency, never re-encodes a frame — the
biggest perceived-responsiveness win); client input echo/prediction; progressive foveal-first refine;
**consistent pacing over absolute-min latency** (jitter is perceived worse than steady latency);
intra-refresh (no keyframe spikes).

### Transport & client
- **WebTransport (HTTP/3/QUIC)** — ships in 2026 browsers; unreliable **datagrams** (video slices +
  RTT-adaptive Reed-Solomon FEC on Wi-Fi/WAN, deadline-bounded selective retransmit on LAN) + reliable
  **streams** (UI tiles / input / control, no cross-stream HoL). **Mandatory WebSocket/TCP fallback**
  (WebTransport is still a WG draft; also for HTTP/3-blocking proxies). Custom UDP+FEC (Moonlight) is
  the latency floor but not browser-reachable → design reference only.
- **Client:** browser **WebCodecs** (`optimizeForLatency`, HW decode, 1-in-1-out) → WebGL; composites
  the tile-cache layer + cropped video + local cursor sprite; low-latency **Opus** audio. **A/V master
  clock is persona-conditioned:** video is master for interactive/office/CAD personas (protect
  motion-to-photon; audio may slip ≤125 ms per ITU-R BT.1359), audio is master only for the passive
  full-screen-media persona. Replaces the `.vv` native viewer.

### Adaptive control loop
Jointly adapt QP maps / foveation strength / resolution / framerate from (a) network (QUIC loss/
congestion), (b) **host-capacity budget** (ADR 0007 — under contention, spend fewer perceptual bits),
(c) persona/use-case. Perceptual degradation **is** the currency of ADR-0007's graceful-degradation
ladder.

### Peripheral channels & integration
Input (virtio-input-style injection or the infiniservice channel), clipboard, USB/device redirection,
multi-monitor: **short-term keep SPICE/agent channels**; migrate into infiniPixel over time. A **new
`encoded-console-stream` service** sits beside `SpiceProxyService.ts`, reusing its port/auth/session
scaffolding; the **fallback ladder** is infiniPixel(HW) → software-x264 (no NVENC) → SPICE (legacy/
thin clients, and the desktop-diff-optimized path for idle 2D).

## Consequences

- **Positive:** a genuine differentiator SPICE/vGPU cannot match (owned, GPU-native, perceptual,
  browser-delivered); idle desktops cost ~0; frame stays on-GPU end to end.
- **Negative / accepted / NEEDS VERIFICATION:** large net-new surface (browser client + service +
  client compositor); **browser HEVC decode is HW/OS-gated** → per-session negotiation + H.264
  fallback; content classifier needs hysteresis tuning (mis-route = blurred text or thrashed tiles);
  reconciling per-VM fairness with per-session network adaptation without oscillation is an open
  control-loop problem; WebTransport not frozen → TCP fallback mandatory; aggregate NVENC session
  ceiling on GA102's **single NVENC block** under active dynamic regions needs measurement (Phase-1 gate;
  make the NVENC session a first-class resource in the ADR-0007 capacity ledger — hard count = 1 on GA102).
- **Phasing:** Phase-0 proves the loop by presenting the blob into QEMU's **existing** SPICE path
  (zero new client code); infiniPixel is **Phase-1** once the render loop is solid.
