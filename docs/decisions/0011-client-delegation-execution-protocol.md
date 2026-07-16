# ADR 0011 — Client-delegation execution protocol

- **Status:** accepted
- **Date:** 2026-07-16
- **Feeds from:** research/30-client-delegation-instruction-set-and-wire.md, research/31-client-delegation-negotiation-latency-bandwidth.md, decisions/0010 (client offload), decisions/0009 (infiniPixel), research/27 (capability negotiation)

## Context

Capability negotiation (doc 27) decides what the client *can* do. This ADR defines **how the host
actually delegates execution frame-to-frame**: which instructions, in what wire format, bound to which
frame, at what latency, and how it is negotiated and sent. It sits strictly **above** the unchanged
ADR-0009 host-pixel path (the always-correct floor).

## Decision — a two-plane, epoch-fenced, degrade-never-deny protocol

### Instruction set (1-byte opcode, Guacamole/RDP-EGFX taxonomy, binary encoding)
- **DRAW `0x10–0x1F`** — SOLIDFILL / COPY(blit) / IMG_TILE / GLYPH / TRANSFER (client rasterizes UI:
  Guacamole `cfill/copy/img/glyph` + RDPEGFX SOLIDFILL/SURFACETOSURFACE/WIRETOSURFACE semantics).
- **TILE-CACHE `0x30–0x33`** — CACHE_STORE (21 B) / **CACHE_HIT (9 B = the few-byte reference)** /
  EVICT / IMPORT_OFFER.
- **GPU-POST `0x50–0x53`** — CHROMA_444 / DEBLOCK / SHARPEN / COLOR_XFORM (4–6 B).
- **SUPER-RES** — `0x60` descriptor on control (14 B) + `0x61` per-frame ref (4 B).
- **INTERP `0x62`** (7 B, **passive video island only**). **CURSOR** `0x40` define / `0x41` move.
- **CONCEAL/REPROJECT `0x70/0x71`** (7 B, `maxHold` drift bound).

### Wire format — two lanes
- **Reliable control plane** (QUIC stream): 8-byte RDPEGFX-shaped header (`msgType:u16, flags:u16,
  length:u32`) + postcard/TLV body. Messages: `DELEG_HELLO/OFFER/ACCEPT/NACK/READY`,
  `SESSION_DELEGATION_PROFILE`, `DELEGATION_RECONFIG`, `TILE_CACHE_CREATE/EVICT/RESET`, `CURSOR_DEFINE`,
  `CAP_REPROBE`, `DELEGATION_ACK/NAK`, `QOE_REPORT`, `HEARTBEAT`.
- **Fast plane** (unreliable datagram): a compact **per-frame binary sidecar** — 16-byte header
  (`magic_ver, flags, frameSeq:u32, streamEpoch:u16, cacheEpoch:u16, regionCount, directiveCt,
  sidecarLen`) + RegionMap (~12 B/region: id+RECT16+kind) + TLV DirectiveList + optional AuxRefTable.
  **Typical 40–120 B, fits one ≤1200 B QUIC datagram.** Binary (not Guacamole's text) because the
  per-datagram budget is tight, frame-binding is fixed-width metadata, and hostile bytes need
  bounded-time validation. **We keep Guacamole's opcode taxonomy, not its encoding.**
- **Aux blobs** (motion vectors / edge / depth for INTERP/REPROJECT/CONCEAL) do **not** inline (a dense
  1080p MV map is ~16 KB) — they ride their own `frameSeq`-tagged datagram, coarsely quantized (32×32
  blocks, packed i8), referenced by AuxRefTable; lost aux degrades to zero-motion frame-copy.

### Frame-binding / sync contract (closes the ADR-0010 open item)
Every directive is stamped **`{frameSeq:u32, regionId}`** with **`streamEpoch`** (bumped on RECONFIG)
and **`cacheEpoch`** (bumped on TILE_CACHE_RESET). The client applies a directive only if **both epochs
match** its current generation → **epoch fencing** lets the host change the split mid-stream with no
round-trip. Directives are **idempotent**; a stale sidecar for an already-presented frameSeq is dropped;
**a lost sidecar means the region defaults to plain host pixels** (absence = safe baseline, never
blank/corrupt).

### Negotiation — two-phase, non-stalling (8-state machine)
`INIT → NEGOTIATING → PROVISIONING → PROBING → DELEGATED` (+ `RENEGOTIATING/DEGRADING/THIN`). The host
keeps running the **full ADR-0009 pixel path and does NOT reduce its own render/encode until the client
sends `DELEG_READY`** after a proof round-trip. Runtime reconfig is **double-buffered by `deleg_epoch`**:
`DELEGATION_RECONFIG(epoch+1, apply_at_frame=N)` swaps atomically at a frame-group boundary (≤50 ms)
while the old epoch keeps producing frames — **zero per-frame latency for renegotiation**. Setup cost
~2 RTT off the frame path.

### Latency & bandwidth (verified budget)
- **Latency-cutters:** cursor 14–22 ms → ~1–2 ms; cached-tile ref sub-ms; reprojection ~1–3 ms (hides
  loss/late frames). **Latency-adders:** compose+chroma+post ~sub-ms–2 ms (retires host AVC444);
  super-res ~3–4 ms; **interpolation +1 frame ~16 ms (hard-fenced to the passive video island only).**
- **Signaling overhead = 0.3 %–6 % of the bandwidth it saves** (idle ~0; text ~0.4 KB/s vs ~1 Mbit/s
  saved; 1080p video island ~45 KB/s MV-sidecar vs ~6 Mbit/s saved). Overhead ≪ savings in all cases.

### Adaptive re-delegation controller
One knob in the ADR-0009/doc-23 loop (SENSE→APPLY ≤50 ms). Host pressure **pushes** work to the client
(density, ADR-0007); client/network pressure **pulls** it back. Per-offload score =
`benefit(host-GPU-saved·C_pressure + bytes-saved·scarcity) − cost(client-headroom + latency·persona-weight
+ hallucination-risk[∞ on UI lane])`. Damping: dual UP/DOWN thresholds (dead-band), ≥2 s min-dwell +
epoch rate-limit, EWMA-smoothed inputs, **asymmetric AIMD (down fast, up slow with proof).** Ladder:
M1+M2+M3(+M4) → drop M4/M2 → M1-light+M3 → M3+cursor → **M0 host pixels** → ADR-0009 quality degrade →
x264 → SPICE. **Local-cursor + steady-cadence survive to the bottom.**

## Consequences

- **Positive:** concrete, codeable delegation contract; renegotiation never stalls frames; loss/reorder
  always defaults to the correct host-pixel baseline; overhead is negligible vs savings; tile-cache is
  nearly-free and shippable first.
- **Negative / accepted / NEEDS VERIFICATION:** the MV/reproject sidecar is the only overhead that can
  eat its savings → **bound the grid, prefer 8-byte global-motion for scroll/pan, dense map opt-in per
  island**; super-res/interp client-GPU costs are doc-26 estimates → bench on the real fleet;
  2D-desktop reprojection lacks a depth buffer → mask with the damage map or it smears static text; the
  hysteresis constants are unset → tune on telemetry (same open problem as ADR-0009); the **full
  vector-command UI lane needs a guest-side draw-op interception layer** (deferred) — only the
  content-addressed tile-cache half is free today.
- **Build order:** control plane + **tile-cache refs first** (nearly-free, ~20× text-bandwidth cut,
  cuts latency) → GPU-POST/chroma → super-res → **INTERP/REPROJECT gated on the MV-aux benchmark.**

## Corrections (review 2026-07-16)

- **Reconnect/resume flow (was missing).** A full QUIC session drop (laptop sleep / Wi-Fi handoff /
  proxy reset — distinct from QUIC connection migration) must NOT cold-reset the warm tile cache. Add:
  a **resumption token** + console-ticket grace window; on reconnect the client sends
  **`CACHE_IMPORT_OFFER`** (op 0x33) with its retained `tileHash` set so the host **revalidates**
  (does not reset `cacheEpoch`) and skips re-sending cached tiles; define idle/hard-lifetime timers
  across a transient drop.
- **One canonical per-frame header + epoch name.** Use **`streamEpoch`** everywhere (drop the
  `deleg_epoch` alias) and **add `present_deadline_us`** to the sidecar header (client discards
  directives that miss `requestVideoFrameCallback` `expectedDisplayTime`). Gate the client epoch advance
  on `apply_at_frame`/`profile_boundary` so the swap is deterministic across the two lanes.
- **Epoch fencing scope fix:** gate `SUPERRES_REF` and all GPU-POST/INTERP/REPROJECT sidecar ops on
  **`streamEpoch` only**; reserve `cacheEpoch` for CACHE_HIT/CACHE_STORE/tile-lane ops (gating super-res
  on `cacheEpoch` was wrong).
- **Message-name collision fix:** negotiation = `OFFER/ACCEPT/DECLINE/READY`; per-frame feedback =
  `FRAME_ACK/FRAME_NAK`; profile msg = `SESSION_DELEGATION_PROFILE` (one name across docs 30/31/ADR).
- **Always emit the sidecar as its own `frameSeq`-tagged datagram** (decouples it from slice-0 loss and
  from the 1200 B path MTU; absence = safe host-pixel baseline). Region struct = **12 B** (11 used + 1 pad).
- **Super-res legibility inside the video island:** the UI/island split is by churn, so a scrolling
  code editor / screen-share lands on the island. **Text-detect island sub-tiles** (reuse the doc-22
  edge/palette classifier) and cap those to spatial upscale or host pixels; run the edge-fidelity gate on
  island output, not only the UI lane.
- **Latency labels:** the ~14–22 ms figure is the **display-datapath** budget within a **~40–70 ms full
  motion-to-photon** — the local-cursor win is 40–70 ms → ~1–2 ms.
- **TCP fallback:** on WebSocket/TCP the unreliable video lane loses QUIC's HoL-avoidance — use parallel
  per-lane WS (or a lane-drop policy), cap res/fps, and hand hostile networks to the SPICE rung.
- **Client-cost figures (super-res/interp ~3–4 ms) are unverified estimates** — key M1/M2 activation on
  **measured** `FRAME_ACK.lagFrames`/`QOE_REPORT`, not the static estimate.

Full review log: [`../ERRATA.md`](../ERRATA.md). Failure-mode walkthroughs: [`../SCENARIOS.md`](../SCENARIOS.md).
