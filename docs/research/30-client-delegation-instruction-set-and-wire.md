# 30 — Client-delegation instruction set, wire format, frame-binding & fallback

**Scope.** Doc 27 / ADR-0010 negotiate *what the client CAN do* (the CAPS handshake → an
`offload_mode` set). This doc closes the owner-flagged gap: the **delegation *execution* protocol** —
once negotiated, *how* the host delegates work frame-to-frame. It specifies (1) a typed **instruction
set**, (2) the **two-lane wire format** (reliable control-channel messages + a per-frame binary
sidecar), (3) the **frame-versioning / sync contract** binding a directive to the exact frame+region,
and (4) the **fallback rule** reverting a region to the ADR-0009 host-pixel path without a glitch.

It reuses the **opcode taxonomy** of two mature server-drives-the-client protocols — **Apache
Guacamole** (text 2D drawing orders) and **RDP MS-RDPEGFX** (binary surface/cache pipeline) — while
diverging on encoding for the per-frame hot path (§4.3). Transport is ADR-0009's WebTransport/QUIC:
a reliable stream for control + UI, unreliable datagrams for video + per-frame directives.

---

## 1. Design frame: two planes, one opcode space

Delegation splits cleanly along the ADR-0009 lane boundary:

- **Slow plane (reliable QUIC stream, the "control ring" of ADR-0004/0009).** Session-scoped, must
  not be lost, changes at UI/control rate: the delegation profile, tile-cache slot management,
  cursor-sprite definitions, capability re-probes, and the **2D UI draw-ops** the client rasterizes
  (Guacamole/RDPEGFX static-UI vocabulary). Ordered, retransmitted, no head-of-line coupling to video
  because it is a *separate* stream.
- **Fast plane (unreliable QUIC datagram, per encoded frame).** A compact **binary sidecar** riding
  with (or just ahead of) each frame's slice datagrams, carrying the per-region directives that
  post-process / super-resolve / interpolate / conceal *that specific decoded frame*. Droppable by
  design: a lost sidecar means the region presents as plain decoded pixels.

Every operation gets a **1-byte opcode** in a single space, partitioned so the opcode alone tells the
client which plane it belongs to and which client subsystem executes it:

| Range | Class | Plane / lane |
|---|---|---|
| `0x00–0x0F` | Session / control | reliable stream |
| `0x10–0x2F` | 2D UI **DRAW** ops | reliable stream (UI-tile lane) |
| `0x30–0x3F` | **TILE-CACHE** directives | reliable stream |
| `0x40–0x4F` | **CURSOR** | reliable (define) / local (move) |
| `0x50–0x5F` | **GPU-POST** on a video region | per-frame sidecar |
| `0x60–0x6F` | **SUPER-RES / INTERP** | control (descriptor) + sidecar (per-frame ref) |
| `0x70–0x7F` | **LOSS-CONCEAL / REPROJECT** | per-frame sidecar |

Shared field types (all little-endian): `u8/u16/u32`, `i8`, `q0.8` (unsigned fixed 0–1),
`RECT16 = {x:u16, y:u16, w:u16, h:u16}` = **8 bytes** (RDPEGFX uses left/top/right/bottom; we carry
x/y/w/h to save a subtraction on the client), `regionId/surfaceId/layerId:u16`, `frameSeq:u32`,
`streamEpoch:u16`, `cacheEpoch:u16`, `tileHash:u64` (truncated xxh3/BLAKE3 content address).

---

## 2. The delegation instruction set (opcode table)

Sizes are the **fixed header** of each directive; ops that carry a pixel/blob payload append it after
the header (on the reliable stream) or reference a separate datagram (sidecar). Field widths for the
RDP-derived cache/fill ops are modelled on MS-RDPEGFX's `SOLIDFILL` / `SURFACETOCACHE` /
`CACHETOSURFACE` PDUs (*exact RDP struct widths NEEDS VERIFICATION against the spec; ours are chosen
for our region model*).

### (a) 2D UI DRAW ops — client rasterizes static UI as commands, not pixels

| Op | Name | Parameters (widths) | Hdr bytes | Lane |
|---|---|---|---|---|
| `0x10` | `SOLIDFILL` | layer:u16, rect:RECT16, rgba:u32 | 15 | reliable |
| `0x11` | `COPY` (surface→surface blit) | srcLayer:u16, srcRect:RECT16, dstLayer:u16, dstX:u16, dstY:u16 | 17 | reliable |
| `0x12` | `IMG_TILE` | layer:u16, rect:RECT16, codecId:u8, streamId:u16 *(+ blob)* | 14 + payload | reliable |
| `0x13` | `GLYPH` (text from glyph cache) | glyphSlot:u16, dstX:u16, dstY:u16, fgRgba:u32 | 11 | reliable |
| `0x14` | `TRANSFER` (ROP blit) | srcLayer:u16, srcRect:RECT16, rop:u8, dstLayer:u16, dstX:u16, dstY:u16 | 18 | reliable |

`SOLIDFILL`/`COPY`/`TRANSFER`/`IMG_TILE`/`GLYPH` are the binary encodings of Guacamole's
`cfill`/`copy`/`transfer`/`img`/glyph-cache verbs and RDPEGFX's `SOLIDFILL` / `SURFACETOSURFACE` /
`WIRETOSURFACE_1`. `IMG_TILE.codecId` reuses the RDPEGFX codec enum shape (`0x00` raw / `0x02` PNG /
`0x03` WebP / `0x09` progressive) so a photographic tile still rides the codec path. These make static
UI **draw commands, not encoded pixels** — the ADR-0010 §3 "command lane" (its full form needs the
deferred guest draw-op interception layer; the cache half below is free today).

### (b) TILE-CACHE directives — content-addressed dedup (the free-now half of the command lane)

| Op | Name | Parameters (widths) | Hdr bytes | Lane |
|---|---|---|---|---|
| `0x30` | `CACHE_STORE` (surface→cache) | cacheSlot:u16, tileHash:u64, srcLayer:u16, srcRect:RECT16 | 21 | reliable |
| `0x31` | `CACHE_HIT` (cache→surface) | cacheSlot:u16, dstLayer:u16, dstX:u16, dstY:u16 | 9 | reliable |
| `0x32` | `CACHE_EVICT` | cacheSlot:u16 | 3 | reliable |
| `0x33` | `CACHE_IMPORT_OFFER` | count:u16, {tileHash:u64}[] | 3 + 8·n | reliable |

`CACHE_HIT` is the **~9-byte reference** that replaces re-sending an already-seen 32×32/64×64 tile —
RDPEGFX `CACHETOSURFACE` / `CACHEIMPORTOFFER` over ADR-0009's reliable lane, the ~5–15× text-bandwidth
cut with **zero host encode** (ADR-0010 §3).

### (c) GPU-POST ops on a decoded video region (per-frame sidecar)

| Op | Name | Parameters (widths) | Hdr bytes | Lane |
|---|---|---|---|---|
| `0x50` | `CHROMA_444` | regionId:u16, method:u8, edgeAuxOff:u16 | 6 | sidecar |
| `0x51` | `DEBLOCK` | regionId:u16, strength:u8 | 4 | sidecar |
| `0x52` | `SHARPEN` | regionId:u16, amount:q0.8, edgeMask:u8 | 5 | sidecar |
| `0x53` | `COLOR_XFORM` (gamma/HDR) | regionId:u16, transferFn:u8, primaries:u8, rangeFlag:u8 | 6 | ctrl+sidecar |

`CHROMA_444.method` selects luma-guided 4:2:0→4:4:4 reconstruction (retiring ADR-0009's AVC444
double-encode) using an optional host edge-hint map referenced by `edgeAuxOff`. `COLOR_XFORM` is
usually session-static (carried once on the control profile) with a 6-byte per-frame re-assert only
when the transfer function changes (SDR↔HDR/PQ/HLG).

### (d) SUPER-RES directive

| Op | Name | Parameters (widths) | Hdr bytes | Lane |
|---|---|---|---|---|
| `0x60` | `SUPERRES_DESC` (session/region) | regionId:u16, srcW:u16, srcH:u16, dstW:u16, dstH:u16, scale:q0.8, modelId:u8, quality:u8 | 14 | reliable (profile) |
| `0x61` | `SUPERRES_REF` (per-frame) | regionId:u16, modelId:u8 | 4 | sidecar |

The heavy descriptor (`srcRes→dstRes`, scale, `modelId` = Lanczos/FSR/ESRGAN/Anime4K/WebNN-net,
`quality`) is negotiated **once** on the reliable profile; each frame carries only the 4-byte
`SUPERRES_REF` confirming the island is still SR-active this frame.

### (e) FRAME-INTERPOLATION directive (video island only)

| Op | Name | Parameters (widths) | Hdr bytes | Lane |
|---|---|---|---|---|
| `0x62` | `INTERP` | islandId:u16, targetFps:u8, method:u8, mvAuxOff:u16 | 7 | sidecar |

`method` = warp / RIFE-class / optical-flow; `mvAuxOff` references the per-block motion-vector aux
block (§4.2). Applied **only** to the passive island (ADR-0010 hard rule); the interactive path stays
1-in-1-out.

### (f) LOCAL CURSOR sprite

| Op | Name | Parameters (widths) | Hdr bytes | Lane |
|---|---|---|---|---|
| `0x40` | `CURSOR_DEFINE` | cursorId:u16, hotspotX:u16, hotspotY:u16, w:u16, h:u16, codecId:u8, blendMode:u8 *(+ blob)* | 12 + payload | reliable |
| `0x41` | `CURSOR_MOVE` (authoritative correct) | cursorId:u16, x:u16, y:u16 | 7 | sidecar / tiny reliable |

The sprite is defined once (reliable); the client then moves it **locally at input latency** (ADR-0009
§Perceived-latency) and re-encodes no frame. `CURSOR_MOVE` is only an occasional host *correction* of
the authoritative position, not the per-motion path.

### (g) LOSS-CONCEALMENT / REPROJECTION directive

| Op | Name | Parameters (widths) | Hdr bytes | Lane |
|---|---|---|---|---|
| `0x70` | `CONCEAL` (intra-frame slice loss) | regionId:u16, method:u8, mvAuxOff:u16, maxHold:u8 | 7 | sidecar |
| `0x71` | `REPROJECT` (whole-frame miss) | regionId:u16, method:u8, mvAuxOff:u16, maxHold:u8 | 7 | sidecar |

`CONCEAL.method` = frame-copy vs motion-compensated EC (doc 27 §5); `REPROJECT` = ASW-style warp of
the last good frame. `maxHold` caps synthesized frames (≈2–3) before freeze-and-request-refresh —
the drift bound.

---

## 3. Wire format — the reliable control-channel message set

Control messages ride ADR-0009's reliable WebTransport stream and extend ADR-0004's control ring.
Each is framed with a fixed **8-byte header deliberately shaped like RDPEGFX's `RDPGFX_HEADER`**
(`cmdId:u16 + flags:u16 + pduLength:u32`), followed by a `postcard`-encoded body (ADR-0004's
variable-shape control serializer):

```
CtrlHeader { msgType:u16, flags:u16, length:u32 }   // 8 bytes, then postcard body
```

| `msgType` | Message | Dir | Body (key fields) |
|---|---|---|---|
| `0x0100` | `SESSION_DELEGATION_PROFILE` | H→C | streamEpoch:u16, offloadModes:bitset, perRegion:[`SUPERRES_DESC` + chroma/interp/color descriptors], sideChannels:{mvChan, tileLane} |
| `0x0101` | `DELEGATION_RECONFIG` | H→C | streamEpoch:u16 (bumped), delta of the above; applied at next frame-group boundary |
| `0x0110` | `TILE_CACHE_CREATE` | H→C | cacheEpoch:u16, slotCount:u16, tileBytes:u16 |
| `0x0111` | `TILE_CACHE_EVICT` | H→C | slot:u16 |
| `0x0112` | `TILE_CACHE_RESET` | H→C | cacheEpoch:u16 (bumped) — invalidates all `CACHE_HIT` refs |
| `0x0120` | `CURSOR_DEFINE` | H→C | (op `0x40` body + blob) |
| `0x0130` | `CAP_REPROBE` | H→C | reason:u8 (re-run WebGPU/decode probe; e.g. suspected device-lost) |
| `0x0200` | `DELEGATION_ACK` | C→H | ackFrameSeq:u32, appliedMask:bitset(opclasses), lagFrames:u8 |
| `0x0201` | `DELEGATION_NAK` | C→H | frameSeq:u32, regionId:u16, opcode:u8, cause:u8 (unsupported/device-lost/behind) |
| `0x0210` | `QOE_REPORT` | C→H | window stats: arrivalJitter, concealedFrames, driftFrames, decodeHwFlag |

`SESSION_DELEGATION_PROFILE`/`RECONFIG` are the negotiated contract (extending doc 27's
`HOST_OFFER`/`HOST_RECONFIG`); `NAK`/`QOE_REPORT` are the fallback triggers (§5). Everything on this
plane is loss-free and rate-limited to UI/control cadence, so text-vs-binary is moot — but we keep it
binary-framed for one code path with the sidecar.

---

## 4. Wire format — the per-frame binary sidecar

### 4.1 Byte layout

One sidecar is emitted per encoded frame and either **prepended to the frame's first slice datagram**
or sent as its own small datagram tagged by `frameSeq`. Because QUIC datagrams carry **no stream ID
and no sequence number** and are **never fragmented** (RFC 9221), the sidecar supplies its own binding
fields and stays inside one ~1200-byte datagram (RFC 9000 min path MTU).

```
Sidecar := SidecarHeader , RegionMap , DirectiveList [, AuxRefTable]

SidecarHeader (16 bytes)
  magic_ver   u8    // 0x1n : 0x1 = infiniPixel sidecar, n = proto minor
  flags       u8    // bit0 keyframe(intra-refresh anchor), bit1 has_aux,
                    //   bit2 last_slice, bit3 profile_boundary
  frameSeq    u32   // monotonic frame generation — THE binding key (§4.4)
  streamEpoch u16   // profile generation; must equal client's current profile
  cacheEpoch  u16   // tile-cache generation; must equal client's cache gen
  regionCount u8
  directiveCt u8
  sidecarLen  u16   // total bytes incl. directives & aux table
  reserved    u16

RegionMap : regionCount × Region (10 bytes each)
  regionId  u16
  rect      RECT16  // 8 bytes: x,y,w,h in surface coords
  kind      u8      // 0=UI-tile 1=video-island 2=cursor  (packed w/ reserved → 12B aligned)

DirectiveList : directiveCt × Directive (TLV)
  opcode    u8      // from §2 (sidecar-plane opcodes only: 0x41,0x50-0x53,0x61,0x62,0x70,0x71)
  len       u8      // payload length
  payload   len×u8  // the op's parameter block from §2 (regionId first)

AuxRefTable (present iff flags.has_aux) : references to separate datagram(s)
  auxKind   u8      // 0=MV-map 1=edge-hint 2=depth
  auxDgramId u16    // id of the companion datagram carrying the aux blob for THIS frameSeq
  auxLen    u32
```

### 4.2 Motion-vector / aux blobs do not inline

A per-block MV map for a 1080p island at 16×16 blocks is ~8k blocks × 2 B ≈ 16 KB — far past one
datagram. So MV/edge/depth aux rides its **own unreliable datagram(s)**, coarsely quantized
(e.g. 32×32 blocks, packed `i8 dx,dy`), tagged with the same `frameSeq`; the sidecar only *references*
it via `AuxRefTable` + each op's `mvAuxOff`. If the aux datagram is lost, `INTERP`/`REPROJECT`/`CONCEAL`
degrade to zero-motion (frame-copy) rather than failing. This mirrors the cloud-gaming pattern of
shipping motion-vector / depth metadata as an optimized *regional* side channel rather than per-pixel.

### 4.3 Typical size, and why binary not text

**Typical hybrid frame** (1 video island + 2 UI dirty bands, no inline aux): 16 (header) + 3×12
(regions) = 52 + directives {`CHROMA_444` 6, `DEBLOCK` 4, `SUPERRES_REF` 4, `REPROJECT` 7} = 21 + TLV
overhead ≈ **~80–100 bytes**; a pure-video-island frame with just chroma+deblock is **~40 bytes**;
a rich island (chroma+deblock+sharpen+superres+interp+reproject) tops out **~120 bytes** — all
comfortably inside one datagram beside the video slice.

**Why we diverge from Guacamole's text protocol** for the fast plane while reusing its opcode
vocabulary — three hard reasons:

1. **Per-datagram budget.** Directives must fit *with* video slices in a non-fragmentable ≤1200-byte
   QUIC datagram at 60–120 fps. Guacamole's `LENGTH.VALUE` decimal-ASCII + UTF-8 framing inflates every
   integer 2–4× and forces per-element parse/allocation; a fixed binary struct is `memcpy`/zerocopy
   (ADR-0004) with a known size computed before send.
2. **Frame binding is structured metadata, not a drawing script.** A per-frame directive's essence is
   `{frameSeq, streamEpoch, cacheEpoch, regionId} → op`. That is fixed-width binding data — exactly the
   binary `RDPGFX_HEADER` shape (`u16 cmdId + u32 len`), not a text order stream.
3. **Hostile-byte safety under loss/reorder.** Datagrams drop and reorder with no seqno (RFC 9221), so
   the parser must validate untrusted, possibly-partial bytes in bounded time — `zerocopy` fixed layout
   does; scanning a text stream for the next `;` does not, and a truncated text instruction is
   ambiguous where a truncated binary directive is a length check.

Guacamole's **taxonomy** (rect/fill/copy/img/glyph/cursor/cache) is the right *vocabulary*; its
*text encoding* is right for a reliable TCP control stream (and we may keep the reliable UI/control
lane Guacamole-ish), but wrong for the per-frame unreliable hot path — hence RDPEGFX-style binary
there.

---

## 5. Frame-versioning / synchronization contract (the ADR-0010 open item)

A directive must apply to **exactly** the frame+region it was computed for; otherwise a chroma/SR/warp
pass runs against the wrong pixels. The contract is three monotonic counters + idempotency:

- **`frameSeq` (u32, monotonic per session).** Every encoded frame and its sidecar carry the same
  `frameSeq`. The client keys decoded frames and their directives by it. A directive references its
  frame implicitly (same datagram/`frameSeq`) and its region by `regionId` against the sidecar's
  RegionMap. **Binding tuple = `(frameSeq, regionId)`.**
- **`streamEpoch` (u16).** Bumped on every `DELEGATION_RECONFIG`. The client **applies a sidecar
  directive only if `streamEpoch` equals its current profile epoch.** A `RECONFIG` therefore *fences*:
  in-flight sidecars stamped with the old epoch are ignored, so a stale directive can never be applied
  under a new profile (e.g. an old `SUPERRES_REF` after SR was turned off).
- **`cacheEpoch` (u16).** Bumped on `TILE_CACHE_RESET`. A `CACHE_HIT`/`SUPERRES_REF` whose epoch no
  longer matches is dropped and the host re-sends real pixels — an evicted slot can never draw stale
  content.

**Behaviour under reorder/loss (datagrams do both, RFC 9221):**

- **Idempotent directives.** Every sidecar op is a pure function of its frame+region; it accumulates no
  state. Applying a duplicate sidecar (datagram dup) yields the identical result — safe to re-apply.
- **Stale-drop.** A sidecar arriving for a `frameSeq` the client has already presented is discarded
  (monotonic check) — reorder cannot resurrect an old transform.
- **Absence = safe default.** A lost sidecar (or lost aux) means the region has *no* directive this
  frame → the client presents **plain decoded pixels** (the ADR-0009 host path) for that region. Missing
  delegation never blanks or corrupts; it degrades to the correct baseline.
- **Epoch fencing over the join.** Because profile/cache changes bump an epoch and directives self-check
  it, the host can change the split mid-stream with no coordination round-trip: old-epoch traffic simply
  stops being honored at the exact frame the new epoch takes effect.

`flags.profile_boundary` marks the first `frameSeq` under a new `streamEpoch`, so the client swaps its
pipeline at a clean frame edge (no tearing between two profiles).

---

## 6. Failure / fallback rule

**Detection** (host-side, per region) — three triggers, richest→cheapest signal:

1. **Explicit `DELEGATION_NAK`.** The client couldn't run an op (WebGPU device lost, model missing,
   decode fell back to software) → names `{frameSeq, regionId, opcode, cause}`. Immediate.
2. **Ack lag / timeout.** `DELEGATION_ACK.lagFrames` (or missing acks for K consecutive frames)
   shows the client fell behind the delegated workload → host infers overload.
3. **Drift report.** `QOE_REPORT.concealedFrames/driftFrames` exceeding the `maxHold` bound means the
   client is holding synthesized frames too long — the reproject/conceal path is diverging from truth.

**Reversion (revert a region to the ADR-0009 host-pixel path within N frames, no visible glitch):**

- On any trigger for a region, the host **(a)** stops emitting that region's sidecar directives,
  **(b)** resumes encoding that region as full host pixels on the unchanged ADR-0009 path, and
  **(c)** issues a `DELEGATION_RECONFIG` that **bumps `streamEpoch`**, fencing off all in-flight
  delegated state for the region (§5).
- **No glitch, by overlap.** The pixel path is *always correct and always available* (the ADR-0009
  invariant), so reversion is additive: the host sends the first full-pixel frame for the region **at or
  before** the frame where it drops directives. Worst case the region shows slightly softer full-res
  pixels for one frame instead of a client-super-resolved one — never a blank, freeze, or torn frame.
  To re-anchor cleanly the host forces an **intra-refresh wave** on the reverting region (never an IDR —
  ADR-0009's no-keyframe-spike rule), so the full-pixel path locks to ground truth within the wave.
- **Bound `N ≈ 2–3 frames`,** matching the conceal/reproject drift bound (doc 27 §5): synthesized
  content is time-boxed, and the reliable UI-tile lane + periodic intra-refresh continuously re-anchor
  the client to real pixels regardless.
- **Direction of failure.** Fallback is **degrade, never deny** (doc 27): an offload fault drops the
  region *one rung toward M0* (full host pixels), and **upgrade requires proof** — the host does not
  re-delegate until fresh acks/probe (`CAP_REPROBE`) show the client recovered. An over-claiming client
  only ever harms its own view; the host, guest, and command/authorization boundary
  (HMAC-signed host→guest channel) are untouched, because all of this lives strictly **downstream of the
  display datapath**.

---

## 7. Summary of the contract

- **Instruction set (§2):** one 1-byte opcode space — DRAW `0x10–0x1F`, TILE-CACHE `0x30–0x3F`,
  CURSOR `0x40–0x41`, GPU-POST `0x50–0x53`, SUPER-RES/INTERP `0x60–0x62`, CONCEAL/REPROJECT
  `0x70–0x71` — each a typed fixed-header directive with an explicit lane.
- **Wire (§3–4):** a reliable **control-message set** (8-byte RDPEGFX-shaped header + postcard body)
  for profile/cache/cursor/probe/acks + the 2D UI draw-ops; a **16-byte-header binary per-frame
  sidecar** (region map + TLV directives + epoch/seqno, typically **40–120 bytes**) on the unreliable
  datagram — binary because the per-frame path is budget-, binding-, and loss-constrained where
  Guacamole's text is not.
- **Binding (§5):** `(frameSeq, regionId)` + `streamEpoch` + `cacheEpoch`; idempotent directives,
  stale-drop on reorder, absence⇒host pixels, epoch fencing across profile/cache changes.
- **Fallback (§6):** NAK / ack-lag / drift → revert region to full host pixels within ~2–3 frames via
  overlap + intra-refresh wave; degrade-never-deny, upgrade-only-on-proof.

**Open / NEEDS VERIFICATION:** exact RDPEGFX cache/fill struct widths we modelled ours on; NVENC
per-block MV export path + its cost under the ULL config, and MV-aux quantization that still yields
usable reprojection on 2D content (no depth buffer); measured glitch-freeness of the overlap+intra-
refresh reversion at 60 fps; and whether an ~80-byte sidecar prepended to the first slice datagram
ever pushes a frame's lead datagram past path MTU on a 1200-byte link (fall back to a standalone sidecar
datagram if so).

## Sources

- Apache Guacamole — protocol (server→client drawing instructions; "less bandwidth than … PNG"; instruction = `LENGTH.VALUE` comma-list, semicolon-terminated): https://guacamole.apache.org/doc/gug/guacamole-protocol.html
- Apache Guacamole — protocol reference (opcode arg lists: `rect`/`cfill`/`copy`/`transfer`/`cursor`/`img`/`blob`/`end`/`dispose`/`size`/`clip`): https://guacamole.apache.org/doc/gug/protocol-reference.html
- MS-RDPEGFX — RDPGFX_HEADER (`cmdId:u16`, `flags:u16`, `pduLength:u32`): https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/ed075b10-168d-4f56-8348-4029940d7959
- MS-RDPEGFX — RDPGFX_WIRE_TO_SURFACE_PDU_1 (surfaceId/codecId/pixelFormat/destRect RECT16/bitmapDataLength/bitmapData field widths): https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/fb919fce-cc97-4d2b-8cf5-a737a00ef1a6
- FreeRDP — rdpgfx.h (RDPGFX_CMDID_* command IDs: WIRETOSURFACE_1 0x0001 … SOLIDFILL 0x0004, SURFACETOSURFACE 0x0005, SURFACETOCACHE 0x0006, CACHETOSURFACE 0x0007, EVICTCACHEENTRY 0x0008, CACHEIMPORTOFFER 0x0010; RDPGFX_CODECID_* incl. AV1/AVC420/AVC444/PROGRESSIVE): https://github.com/FreeRDP/FreeRDP/blob/master/include/freerdp/channels/rdpgfx.h
- Microsoft — RemoteFX Adaptive Graphics (content classifier, glyph/bitmap cache, cache-import reuse): https://techcommunity.microsoft.com/blog/microsoft-security-blog/remotefx-adaptive-graphics-in-windows-server-2012-and-windows-8/247454
- RFC 9221 — An Unreliable Datagram Extension to QUIC (no retransmit; may be dropped/reordered; no flow control; not fragmented; no QUIC-layer stream ID or seqno → app prepends its own varint; size bounded by max_datagram_frame_size / max_udp_payload_size / path MTU): https://www.rfc-editor.org/rfc/rfc9221.html
- RFC 9000 — QUIC transport (min supported path MTU 1200 bytes; DPLPMTUD to grow): https://www.rfc-editor.org/rfc/rfc9000.html
- IETF — WebTransport over HTTP/3 (reliable streams + unreliable datagrams over one QUIC connection): https://datatracker.ietf.org/doc/html/draft-ietf-webtrans-http3
- MDN — WebCodecs API (EncodedVideo​Chunk/VideoFrame timestamps as the frame-binding handle; HW decode + SW fallback): https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API
- ResearchGate — Enhancing Video Encoding for Cloud Gaming Using Rendering Information (per-frame motion-vector/rendering metadata shipped alongside encoded frames): https://www.researchgate.net/publication/282556614_Enhancing_Video_Encoding_for_Cloud_Gaming_Using_Rendering_Information
- MDPI Applied Sciences — Cloud Gaming Video Coding via Camera-Motion-Guided Reference Frame Enhancement (motion/depth metadata as a display-side reprojection side channel): https://www.mdpi.com/2076-3417/12/17/8504
- Meta Horizon — Asynchronous Spacewarp (motion-vector frame extrapolation; the reproject model for `0x71`): https://developers.meta.com/horizon/blog/asynchronous-spacewarp/
- ANVIL — Accelerator-Native Video Interpolation via Codec Motion-Vector Priors (arXiv 2603.26835) — MV-prior interpolation, the `INTERP`/`mvAux` model: https://arxiv.org/html/2603.26835v1
- infinigpu ADR-0004 — wire protocol & shared crate (control ring, TLV skip-unknown, zerocopy fixed framing + postcard control): ../decisions/0004-wire-protocol-and-shared-crate.md
- infinigpu ADR-0009 — infiniPixel remote protocol (damage-aware hybrid, reliable UI lane vs unreliable video datagrams, slice-per-datagram, intra-refresh, local cursor): ../decisions/0009-infinipixel-remote-protocol.md
- infinigpu ADR-0010 — client-side offload / split-rendering (four offloads; frame-versioning contract flagged as the open item this doc closes): ../decisions/0010-client-side-offload-split-rendering.md
- infinigpu doc 27 — client capability negotiation & resilience (CAPS handshake, offload modes M0–M4, fallback ladder, error-concealment/reprojection): ./27-client-capability-negotiation-and-resilience.md
- infinigpu doc 26 — client-side GPU offload & split rendering (Guacamole/RDPEGFX command lane, content-addressed tile cache, super-res, interpolation): ./26-client-offload-split-rendering.md
