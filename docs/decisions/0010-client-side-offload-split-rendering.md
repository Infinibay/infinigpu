# ADR 0010 — Client-side GPU offload / split-rendering

- **Status:** accepted
- **Date:** 2026-07-16
- **Feeds from:** research/26-client-offload-split-rendering.md, research/27-client-capability-negotiation-and-resilience.md, decisions/0009 (infiniPixel), decisions/0007 (capacity)

## Context

Owner insight: the client is **not a thin terminal** — it opens the VM from a PC that has its own
GPU (minimal but real: hardware video codecs, 2D/3D, and in-browser WebGL/WebGPU/WebCodecs/WebNN).
**Delegating work to the client GPU** (a) cuts **host** GPU load → more VMs per host GPU (the ADR-0007
density lever), (b) cuts bandwidth, (c) cuts/hides latency. The offload must be **negotiated by client
capability** with a mandatory thin-client fallback.

## Decision

**Add a capability-negotiated client-offload layer to infiniPixel (ADR-0009)** — four offloads, each
degrading to the unchanged ADR-0009 host-pixel path when the client can't do it:

1. **Client composition + post-processing (do first, low risk).** WebCodecs `VideoFrame` →
   `importExternalTexture` is a documented **zero-copy** path (Chrome 116+). The client GPU runs
   luma-guided **4:2:0→4:4:4 chroma reconstruction** (can **retire ADR-0009's AVC444 double-encode** if
   it clears the text-legibility gate), deblocking, text-sharpening, dithering — all moved off the host.
2. **Reduced-res video + client super-resolution (high density payoff).** Host encodes the video
   *island* at reduced res (720p ≈ 0.44× the pixels of 1080p); the client reconstructs to native
   (Lanczos/WebGL baseline; FSR/ESRGAN/Anime4K on WebGPU ~3–4 ms/frame; WebNN-NPU when advertised).
   **~Halves NVENC GPU-time AND bitrate** on that region and makes ADR-0007's "drop resolution"
   degradation rung far less visible.
3. **Content-addressed cached-tile UI lane (biggest bandwidth win, half of it free now).** Hash dirty
   UI tiles; on a cache hit send a **few-byte reference** → ~**5–15× less** bandwidth on active text and
   **zero host encode** on static/cache-hit UI. **No guest changes needed.** The *full* RDP-GFX/Guacamole
   vector-command lane (send draw-ops, not pixels) needs a net-new **guest-side 2D draw-op interception**
   layer (infiniPixel sits at the rendered-framebuffer seam) → **roadmap item**, not now.
4. **Frame interpolation — video island ONLY.** Interpolation *adds* a frame of latency (holds a frame),
   so it is used **only on the passive video island** (host sends ~½ fps, client interpolates to 60 →
   ~half encode+bandwidth on the island, +1 frame latency invisible on passive video). The **interactive
   path stays strictly 1-in-1-out**; the local cursor sprite (ADR-0009) already covers responsiveness.

**Hard routing rule:** ML super-res and interpolation apply **only to the video/3D island**, **never to
the UI lane** (which stays crisp via the tile/command lane) — protecting ADR-0009's text-legibility gate.

**Capability negotiation** at session setup probes WebCodecs (codecs + HW-vs-SW decode, 4:2:0/4:4:4,
bit-depth), WebGL2 (mandatory floor), WebGPU (~85% coverage in 2026, preferred), WebNN/NPU, max texture
size, display res/refresh/HDR, and CPU/battery hints → a decision function shifts work by
`client-capability × network × host-budget (ADR-0007)`, re-evaluated at runtime. **Loss resilience:** the
client GPU doubles as a jitter/loss shock-absorber (error concealment on lost slices; reprojection covers
a late/dropped frame over QUIC datagrams).

## Consequences

- **Positive:** offload is a **density multiplier** (frees host NVENC + SMs), a bandwidth cut, and a
  latency-hider — and turns ADR-0007's degradation ladder (drop res/fps) nearly invisible; all a **bonus,
  never a requirement** (thin clients keep working on the ADR-0009 pixel path).
- **Negative / accepted / NEEDS VERIFICATION:** the full vector-command lane needs a guest-side draw-op
  interception layer (deferred); ML super-res/interp can hallucinate on text → hard-routed off the UI lane
  and must clear the legibility gate (measure VMAF/SSIMULACRA2 of client 720p→1080p vs native 1080p);
  **client GPU budget becomes a new scheduling variable** in the ADR-0009 adaptive loop; WebCodecs exposes
  no decoder motion vectors → cheap interpolation needs host-sent MV/flow hints or client optical flow; the
  host↔client split needs a frame-versioning contract carried by the ADR-0004 wire protocol.
