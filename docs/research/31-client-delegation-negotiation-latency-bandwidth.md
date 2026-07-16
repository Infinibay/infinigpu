# 31 — Delegation execution: negotiation, latency budget & signaling bandwidth

**Scope.** ADR-0010 and doc 27 decided *what* the client GPU can do (four offloads: compose+post,
reduced-res+super-res, cached-tile UI lane, video-island interpolation) and *how it is probed*
(the `CLIENT_HELLO`/`HOST_OFFER`/`CAPS_UPDATE`/`HOST_RECONFIG` CAPS handshake). Capability
negotiation only decides what the client *can* do. This doc closes the owner-directed gap: the
**delegation execution protocol** — once negotiated, how the host binds delegated work to frames,
what it costs in latency and bytes, and how the split is re-balanced at runtime without ever
stalling a frame. It grounds the wire format in Apache Guacamole's length-prefixed instruction
stream, RDP's MS-RDPEGFX graphics pipeline (wire-to-surface / surface-to-cache / cache-import),
WebTransport/QUIC's reliable-stream vs unreliable-datagram split, WebCodecs frame timing, and
cloud-gaming per-frame metadata sidecars.

---

## 1. Negotiation flow & the delegation state machine

Delegation is a **living two-phase contract**, not a one-shot handshake. The failure it must
prevent is *the host reducing its own render/encode before the client has proven it can cover the
gap* — an over-claiming client must only ever harm its own view (doc 27 §4 rule 2). So the
handshake adds an **ACCEPT + proof** phase on top of doc 27's HELLO/OFFER.

### 1.1 Message set (reliable control stream — ADR-0004 control ring, `postcard`/TLV skip-unknown)

All control messages ride the reliable WebTransport stream (like Guacamole's ordered instruction
stream — a lost element cannot be tolerated), each a length-prefixed TLV so an unknown opcode is
skipped, exactly as Guacamole elements are self-describing `LENGTH.VALUE` tokens the parser skips
without scanning ([Guacamole protocol](https://guacamole.apache.org/doc/gug/guacamole-protocol.html)).

| Msg | Dir | Carries | ~Bytes | Frequency |
|---|---|---|---|---|
| `DELEG_HELLO` | C→H | `ClientCaps` (doc 27: decode[], webgpu tier/limits, webnn, display, battery, net) + `session_nonce` + `deleg_epoch=0` | 200–400 | once |
| `DELEG_OFFER` | H→C | `DelegationProfile{profile_id, epoch, offload_mask(M0..M4), per-offload params, sidecar_schema_ver, side_channels[]}` | 100–200 | once + per reneg |
| `DELEG_ACCEPT` / `DELEG_NACK` | C→H | ACCEPT once resources acquired (WebGPU device up, WGSL pipelines compiled, caches allocated); NACK{offload, reason, fallback_hint} declines a rung | 16–48 | per offer |
| `DELEG_READY` | C→H | proof passed: a test upscale/reproject round-tripped, decode ACKs flowing → **unlocks host work reduction** | 16 | per offer |
| `CAPS_UPDATE` | C→H | runtime delta: battery unplug, thermal throttle, tab hidden, `MediaCapabilities.smooth` flip, self-measured RTT | 20–40 | event-driven |
| `DELEG_RECONFIG` | H→C | new `DelegationProfile` at `epoch+1` with `apply_at_frame`; ack'd, applied at frame-group boundary | 100–160 | per reneg |
| `DELEG_PROBE` / `_ACK` | H↔C | periodic re-validation (WebGPU device still alive, probe pass) | 16–32 | ~0.2 Hz |
| `DELEG_HEARTBEAT` / `_ACK` | H↔C | liveness + RTT sample; carries last-presented `frame_seq` (RDP-style flow ack, Guacamole `sync` analog) | 16 | 1–2 Hz |

### 1.2 States & transitions

```
                DELEG_HELLO                DELEG_OFFER               ACCEPT+provision
   [INIT] ───────────────────► [NEGOTIATING] ─────────► [PROVISIONING] ──────► [PROBING]
      ▲  QUIC/WebTransport open      │ NACK(all)              │ device lost         │ proof pass
      │                             ▼                        ▼                     ▼ DELEG_READY
      │                          [THIN:M0] ◄──────────────────────────────────  [DELEGATED]
      │  fatal transport fault      ▲   ▲                                        │  ▲   │
      └──────────────────────────────   │ ladder bottom (host pixels)           │  │   │ trigger
                                        │                                       ▼  │   ▼
                                   [DEGRADING] ◄──────────────────────────  [RENEGOTIATING]
                                     step down a rung        new epoch applied at frame boundary
```

- **INIT → NEGOTIATING:** WebTransport session opens; client sends `DELEG_HELLO`. The **host keeps
  running the full ADR-0009 pixel path** (M0) the entire time — delegation is pure upside layered on
  a already-correct stream.
- **NEGOTIATING → PROVISIONING:** host replies `DELEG_OFFER` with the computed profile. The client
  acquires resources (WebGPU adapter/device, compiles the SR/reproject/compose WGSL pipelines, sizes
  the tile cache). It answers per-offload `ACCEPT`/`NACK` — a client can accept M3 (tile cache) but
  NACK M1 (no WebGPU device), and the host trims the profile accordingly.
- **PROVISIONING → PROBING → DELEGATED:** the client runs one **proof round-trip** (a throwaway
  upscale/reproject on a host-sent probe frame) and sends `DELEG_READY`. **Only now** does the host
  reduce its own work (encode at 0.67× res, drop to ½ fps on the island). This is the ADR-0010
  invariant: upgrade requires proof; a lying `CLIENT_HELLO` never reduces host output until ACKs
  actually flow.
- **DELEGATED → RENEGOTIATING:** any trigger (§4) fires. The host emits `DELEG_RECONFIG(epoch+1,
  apply_at_frame=N)`. Critically, **the old profile keeps producing frames** while the client
  pre-provisions the new one; the switch is atomic at frame `N` (a frame-group boundary, ≤50 ms
  out). Nothing tears — this is double-buffered profiles keyed by `deleg_epoch`, the same discipline
  RDP uses to change surface state between well-defined PDU boundaries.
- **any → DEGRADING → THIN:** a hard fault (WebGPU device lost, reproject divergence, MV channel
  stall, sustained missing ACKs) steps the session **one rung toward M0** immediately — degrade,
  never deny, never blank (doc 27 §4 rule 1). The bottom rung is the unchanged host-pixel path.

The state machine's whole job is that **the two frame lanes never block on the control lane**: the
video datagram lane and the reliable tile lane keep flowing under the *current* epoch while the
*next* epoch is negotiated out-of-band and swapped at a boundary.

### 1.3 Frame binding — how an instruction attaches to a frame

Every delegated instruction is stamped with `{frame_seq: u32, deleg_epoch: u16}` so the client
applies it under the right profile and drops it if stale — the ADR-0010 "frame-versioning contract"
made concrete. The per-frame **delegation header** (≈6–9 B `postcard`-varint) precedes each emitted
frame:

```
DelegFrameHeader { frame_seq:u32, deleg_epoch:u16, lane_mask:u8 /*UI|video|cursor|mv*/, present_deadline_us:u16 }
```

`lane_mask` tells the client which sidecars to expect; `present_deadline_us` sets the pacing target
the client compares against WebCodecs `requestVideoFrameCallback`'s `expectedDisplayTime` (≈16 ms
in the future at 60 Hz) so late instructions are discarded rather than presented out of cadence
([rVFC](https://web.dev/articles/requestvideoframecallback-rvfc)). Reliable-lane instructions
(tile-cache refs) are ordered with the frame; unreliable-lane instructions (MV/reproject hints)
carry `frame_seq` so a late datagram is simply dropped (QUIC datagrams "may be reordered or dropped"
— [RFC 9221](https://datatracker.ietf.org/doc/html/rfc9221)).

---

## 2. Per-task latency budget (numbers)

Baseline motion-to-photon is ADR-0009's **~14–22 ms LAN** (encode ~1–3 ms on Ampere; the rest is
cadence, jitter buffer, decode, present). Each delegated task shifts that number; the sign matters.

| Delegated task | Client GPU cost | Effect on end-to-end MTP vs host path | Direction |
|---|---|---|---|
| **Local cursor** (ADR-0009) | immediate, input-driven overlay | pointer MTP **14–22 ms → ~1–2 ms** (decoupled from encode/net/decode) | **REDUCES** |
| **Cached-tile ref** (M3) | cache lookup + blit, sub-ms | replaces host encode (~1–3 ms) + tile decode of that region; ~3–6 B on the wire vs KB → less serialize/transmit | **REDUCES / neutral** |
| **Vector tile raster** (roadmap) | rasterize draw-ops, sub-ms/tile | removes encode+decode of the region entirely | **REDUCES** (gated on guest draw-op capture) |
| **Compose + 4:2:0→4:4:4 + deblock/sharpen** | zero-copy `VideoFrame`→WebGPU, ~sub-ms–2 ms | runs *inside* the compositor that already presents → adds ~0.5–2 ms to a stage already on the path; retires host **AVC444 double-encode** (host saving, not client latency) | **slight ADD** |
| **Super-resolution** (M1, 720p→1080p) | **~3–4 ms WebGPU** (Anime4K/FSR-class) | adds ~3–4 ms client present; host encodes ~0.44× pixels → ~½ encode + ~½ bytes → less transmit/jitter partly offsets | **slight ADD** (~+2–3 ms net) |
| **Frame interpolation** (M2, video island only) | ~3–4 ms compute, **but holds one frame** | **adds ~1 frame ≈ 16 ms** at 60 Hz — invisible on passive video, fatal on interactive → island-only | **ADDS** (~+16 ms, island only) |
| **Reprojection / extrapolation** (loss/late-frame) | warp last-good frame, ~1–3 ms | fills a *dropped/late* frame *on time* → removes a stall of up to one frame-interval + jitter | **REDUCES on loss** |

**The split is intentional:** cursor, tile-cache and reprojection **cut** perceived latency;
super-res adds a small fixed cost bought back by density+bandwidth; interpolation is the only task
that *adds* real latency and is therefore hard-fenced to the passive video island (ADR-0010 routing
rule). Interpolation "inherently adds input latency … because it holds a frame" (~10 ms on DLSS 3);
reprojection "reduces perceived latency" by warping an already-rendered frame — same client GPU,
opposite sign ([reprojection vs interpolation](https://en.wikipedia.org/wiki/Asynchronous_reprojection)).
*(Super-res ~3–4 ms and interp ~3–4 ms are WebGPU figures on a modest client GPU from doc 26; NEEDS
VERIFICATION on the actual fleet, and reproject quality on 2D desktop content that lacks a depth
buffer.)*

**Negotiation round-trip cost.** Setup is **~2 RTT** off the frame critical path (HELLO→OFFER = 1
RTT; ACCEPT+READY proof = 1 RTT), during session bring-up: **~1 ms on LAN** (RTT ~0.5 ms), **~60 ms
one-time on a 30 ms WAN**. Runtime **renegotiation adds zero per-frame latency**: `DELEG_RECONFIG`
is a single reliable message applied at the next frame-group boundary (≤50 ms) while the old epoch
keeps presenting — the cost is one control message amortized over thousands of frames, never a
frame stall.

---

## 3. Signaling bandwidth accounting (60 fps)

The rule: per-frame **signaling overhead (headers + control + cache-ref framing) must be `<<` the
bytes it saves**. Sidecars are **damage-gated** — no damage, no sidecar — which is what makes the
idle case free. Sizing the elements:

- **`DelegFrameHeader`** ≈ 6–9 B/emitted-frame.
- **Cache-ref element** (reliable UI lane): warm hit `{cache_slot:u16, dst_x:u16, dst_y:u16}` ≈ **6
  B**; cold offer `{content_hash:u64, rect}` ≈ 14 B — RDP's `SURFACETOCACHE`/`CACHETOSURFACE` /
  `CACHEIMPORTOFFER` (cmdId 0x0010) model over QUIC
  ([MS-RDPEGFX cache import](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0)).
- **MV/reproject sidecar** (unreliable datagram, only when M2/reproject active): global-motion
  `{dx:i16, dy:i16, conf:u8}` ≈ **5–8 B** for scroll/pan; dense per-block flow downsampled to 64×64
  blocks on a 1080p island ≈ 30×17 ≈ 510 blocks × 3 B ≈ **~1.5 KB/host-frame** *(grid granularity
  NEEDS VERIFICATION — this is the one sidecar big enough to eat its own savings if too dense)*.
- **Steady control** (all scenarios): heartbeat 16 B @ 2 Hz + rare `CAPS_UPDATE` ≈ **< 1 kbit/s**.

| Scenario | Emitted frames/s | Signaling overhead | Bandwidth saved by delegation | Net |
|---|---|---|---|---|
| **Idle desktop** | ~0 (empty damage → ~0 bits, ADR-0009) | control only ≈ **< 0.1 KB/s** | n/a (already ~0) | overhead ≈ 0; **no regression** |
| **Text editing / scroll** | ~15 effective | header ~0.14 KB/s + control ~0.1 KB/s ≈ **~0.4 KB/s (~3 kbit/s)** | tile-cache turns encoded screen-content tiles (~0.3–1.0 Mbit per page redraw, doc 26) into few-byte refs → **~0.8–1.8 Mbit/s saved** on active text | overhead **~0.3% of savings** |
| **Video playing (1080p island)** | 30 host (client interps to 60) | header ~0.27 KB/s + MV sidecar ~1.5 KB × 30 ≈ **~45 KB/s (~0.37 Mbit/s)** | M1 720p (~0.44× px) saves ~4.5 Mbit/s + M2 30→60 fps saves ~1.5 Mbit/s ≈ **~6 Mbit/s saved** vs ~8 Mbit/s baseline | overhead **~6% of savings** |

Two takeaways. (1) In the tile-cache case the cache-refs *are* the reduced payload; the true
*overhead* (headers+control) is a rounding error, so M3 is nearly free signaling for a ~20× payload
cut — Guacamole's own claim that drawing/cache primitives "take up less bandwidth than sending
corresponding PNG images." (2) The **video-island MV sidecar is the only overhead worth watching**:
at ~0.37 Mbit/s against ~6 Mbit/s saved it is ~6%, comfortably `<<` savings, but a too-dense MV grid
could invert that — hence bounding the grid, preferring the 8-byte global-motion form for
scroll/pan, and making the dense map opt-in per island.

---

## 4. The adaptive re-delegation controller

Delegation depth is one more knob in the doc 23 / ADR-0009 adaptive loop (SENSE→…→APPLY at
~every frame-group, ≤50 ms), not a parallel system. The novelty is that it balances **two opposite
pressures**: **host pressure PUSHES work onto the client** (shed host GPU-time for density,
ADR-0007); **client/network pressure PULLS it back**.

### 4.1 Inputs (SENSE)

- **Client headroom `H`** — from `CAPS_UPDATE` (battery/AC, thermal, tab visibility, `deviceMemory`,
  `smooth` flip) **fused with observed telemetry**: WebCodecs `expectedDisplayTime` vs `now` (a
  v-sync-late present means the client is over-budget), decode/reproject ACK cadence. Client-asserted
  hints are validated by behavior, never trusted raw (doc 27 §6 rule 4).
- **Network `B/RTT/L`** — QUIC/GCC delay+loss estimator at 10–20 Hz (ADR-0023).
- **Host budget `C`** — the ADR-0007 broker's GPU-time / encoder / VRAM pressure.

### 4.2 Decision function (per offload, per lane)

Each candidate offload gets a score; activation is threshold-gated:

```
score(o) = benefit(o) − cost(o)
  benefit = w_density·hostGPU_saved(o)·C_pressure      # worth more when the host is loaded
          + w_bw·bytes_saved(o)·net_scarcity(B,L)
  cost    = w_head·client_headroom_used(o)/H           # worth less when the client is loaded
          + w_lat·latency_added(o)·persona_lat_weight   # interp costs a lot for interactive personas
          + w_risk·hallucination_risk(o, is_UI_lane)    # ∞ on the UI lane → never ML-upscale text

activate o  when  score(o) > UP(o)   and   H,B sustained over the dwell window
deactivate o when  score(o) < DOWN(o)          # DOWN < UP  → dead-band
```

Canonical outcomes fall out directly: **loaded host + capable AC client + tight link → activate
M1+M2** (host spends least GPU-time, density highest); **weak/battery client → M0** (host does all
the work). The **reconciliation case matters most**: when host *and* client *and* network are all
under pressure there is no free lunch — the controller does **not** push work (the client can't take
it) and instead falls to pure ADR-0023 quality degradation (fps→bitrate→res→foveation). Offload can
*lower* a tenant's host cost but never raise it above the ADR-0007 budget; reclaimed GPU-time returns
to the broker ledger.

### 4.3 Damping (hysteresis) — avoid oscillation

1. **Dual thresholds** (`UP` > `DOWN`) per offload → a dead-band; a signal wobbling around the line
   does not toggle the offload.
2. **Minimum dwell time** — an offload cannot re-toggle for ≥2 s / ≥N frame-groups after a change,
   and `deleg_epoch` bumps are rate-limited, so the wire never thrashes profiles.
3. **EWMA-smoothed inputs** — react to sustained `H`/`B` trends (~0.5–1 s), not a single RTT spike
   (the WebRTC GCC discipline of trend, not instantaneous, from doc 23).
4. **Asymmetric response (AIMD-flavored)** — **step DOWN fast** (one frame-group, safety: a lost
   WebGPU device or MV stall drops a rung immediately) but **step UP slow** (require sustained
   headroom *and* a fresh proof, §1.2). Fast-decrease / slow-increase is exactly what keeps the loss
   response stable in WebRTC congestion control.

### 4.4 The ladder down to thin-client

Under mounting pressure the controller sheds offloads worst-cost-first, mapping onto doc 27's
capability tiers A→E and doc 23's rungs 0→5:

```
M1+M2+M3(+M4)  →  drop M4/M2 (battery+latency+risk first)  →  M1-light + M3  →  M3 + local cursor
   →  M0 (host pixels, ADR-0009 baseline)  →  ADR-0023 quality degrade (fps→bitrate→res→foveation)
   →  software-x264 (no NVENC)  →  SPICE (legacy/thin)
```

Two invariants survive to the bottom: the **local cursor stays local** and the **cadence stays
steady** (doc 23 §5) — the two things users feel as responsiveness are never spent. Every rung above
M0 is pure upside; M0 and below is the already-correct host path. That is what makes aggressive
re-delegation safe: the worst case is simply the protocol we already shipped.

---

## Sources

- Apache Guacamole — protocol (length-prefixed `LENGTH.VALUE` instruction stream; drawing/cache primitives "less bandwidth than … PNG images"; skip-parse): https://guacamole.apache.org/doc/gug/guacamole-protocol.html
- Apache Guacamole — protocol reference (rect/copy/transfer/cursor/img/cache verbs): https://guacamole.apache.org/doc/gug/protocol-reference.html
- MS-RDPEGFX — RDP Graphics Pipeline Extension (wire-to-surface / surface-to-cache / cache-import, cache-import-offer cmdId 0x0010, progressive codec): https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0
- MS-RDPEGFX — RDPGFX_WIRE_TO_SURFACE_PDU_1 (cmdId 0x0001): https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/fb919fce-cc97-4d2b-8cf5-a737a00ef1a6
- FreeRDP — rdpgfx.h (RDPGFX_CMDID_* opcodes: WIRETOSURFACE_1/2, SURFACETOCACHE 0x0006, CACHETOSURFACE 0x0007, CACHEIMPORTOFFER 0x0010): https://github.com/FreeRDP/FreeRDP/blob/master/include/freerdp/channels/rdpgfx.h
- FreeRDP — Codecs (RemoteFX Progressive, ClearCodec, AVC420/AVC444, GFX hybrid): https://freerdp-freerdp.mintlify.app/concepts/codecs
- RFC 9221 — An Unreliable Datagram Extension to QUIC (datagram size bounded by max_udp_payload/MTU; cannot fragment; may reorder/drop): https://datatracker.ietf.org/doc/html/rfc9221
- RFC 9000 — QUIC (path must support ≥1200-byte MTU; reliable independent streams, no cross-stream HoL): https://datatracker.ietf.org/doc/html/rfc9000
- WebTransport 2026 — multiplexed reliable streams + unreliable datagrams over HTTP/3: https://www.programming-helper.com/tech/webtransport-2026-web-api-multiplexed-transport-revolution
- web.dev — requestVideoFrameCallback (mediaTime = PTS; expectedDisplayTime vs now for v-sync-late detection; processingDuration): https://web.dev/articles/requestvideoframecallback-rvfc
- MDN — HTMLVideoElement.requestVideoFrameCallback() (per-frame present callback, frame metadata): https://developer.mozilla.org/en-US/docs/Web/API/HTMLVideoElement/requestVideoFrameCallback
- Asynchronous reprojection — Wikipedia (reprojection/timewarp *hides* latency vs interpolation *adds* it by holding a frame): https://en.wikipedia.org/wiki/Asynchronous_reprojection
- Meta Horizon — Asynchronous Spacewarp (per-block motion vectors → frame extrapolation): https://developers.meta.com/horizon/blog/asynchronous-spacewarp/
- US 11,957,975 — dead-reckoning & latency improvement in 3D game streaming (client-side reprojection metadata): https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/11957975
- US 11,833,419 — cloud gaming (server-sent motion-vector / scene-change metadata sidecar): https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/11833419
- ANVIL — accelerator-native video interpolation via codec motion-vector priors (arXiv 2603.26835): https://arxiv.org/html/2603.26835v1
- GameSR — client-side super-resolution on encoded frames (35–49% bandwidth cut): https://openreview.net/forum?id=wnJkdo5Gu9
- WebRTC transport-cc / Google Congestion Control (delay+loss, min, 10–20 Hz; AIMD): https://bloggeek.me/webrtcglossary/transport-cc/
- infinigpu ADR-0004 — wire protocol & control ring (NEGOTIATE/GET_CAPSETS, postcard, TLV skip-unknown): ../decisions/0004-wire-protocol-and-shared-crate.md
- infinigpu ADR-0007 — VDI capacity manager (GPU-time budget, admission/fairness, degradation ladder): ../decisions/0007-vdi-capacity-manager-and-scheduler.md
- infinigpu ADR-0009 — infiniPixel remote protocol (~14–22 ms LAN MTP, slice-per-datagram, fallback ladder): ../decisions/0009-infinipixel-remote-protocol.md
- infinigpu ADR-0010 — client-side offload / split-rendering (four offloads, routing rule, frame-versioning contract): ../decisions/0010-client-side-offload-split-rendering.md
- infinigpu doc 26 — client offload (super-res ~3–4 ms, tile-cache ~5–15× text cut, interpolation +1 frame island-only): ./26-client-offload-split-rendering.md
- infinigpu doc 27 — client capability negotiation (CAPS handshake, offload modes M0–M4, tiers A–E, proof-before-reduce): ./27-client-capability-negotiation-and-resilience.md
- infinigpu doc 23 — perceived latency & adaptive control loop (local cursor 1–2 ms, SENSE→APPLY, steady-cadence invariant): ./23-perceived-latency-and-adaptive-control.md
