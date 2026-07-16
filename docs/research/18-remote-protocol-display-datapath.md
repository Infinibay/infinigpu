# 18 — Display datapath: a purpose-built low-latency remote protocol (beats SPICE)

**Scope:** the frame the arbiter produced already lives on the host A5000 as a blob dma-buf
(docs 06/09). This doc designs the pixels-to-browser path — codec, a damage-aware hybrid
encoder, transport, frame pacing, and the browser client — as a **purpose-built protocol**
that replaces SPICE's readback+CPU-encode+TCP pipeline. It builds on doc 09 (NVENC on the
A5000, Vulkan→CUDA interop), doc 11 (wire envelope, cursor/scanout messages), the guest
damage-tracking already done in the guest driver (doc 17), and the host-capacity
degradation budget (doc 16). Owner directive: we own guest + arbiter + client, so exploit
end-to-end ownership; the north star is motion-to-photon, then bandwidth for many desktops.

## Verdict up front

Build **infiniPixel**: NVENC **HEVC** (H.264 fallback) in an infinite-GOP, intra-refresh,
slice-pipelined ULL configuration, wrapped in a **damage-aware hybrid** that sends static UI
as reliable near-lossless tiles and dynamic regions as a video stream, over **WebTransport
(HTTP/3/QUIC)** — unreliable datagrams for video slices, reliable streams for tiles/input/
control — decoded in the browser with **WebCodecs** and composited on a WebGL canvas. This
lands a **~14–22 ms LAN motion-to-photon** display-datapath budget versus SPICE's
readback+CPU-encode+TCP path, and drops an **idle desktop from megabits to near-zero**.
AV1 is *not* on the table for encode on our hardware — GA102 Ampere NVENC does **H.264 and
HEVC only** ([TechPowerUp Ampere codec matrix](https://www.techpowerup.com/273420/nvidia-updates-video-encode-and-decode-matrix-with-reference-to-ampere-gpus),
[NVENC/NVDEC matrix, NVIDIA forum](https://forums.developer.nvidia.com/t/video-encode-and-decode-gpu-support-matrix/64780)).

## 1. Codec strategy — HEVC-first ULL, and the AV1 correction

**The AV1 trap.** The brief lists "AV1 tradeoffs on Ampere GA102 NVENC." There are none:
Ampere's NVENC is the *Turing-generation* block and **cannot encode AV1** — it does H.264 and
HEVC; AV1 *decode* (NVDEC) is present but irrelevant to a host encoder. AV1 *encode* first
shipped on Ada Lovelace (RTX 40 / A-series successor)
([VideoCardz](https://videocardz.com/newz/nvidia-updates-nvdec-video-decoding-and-nvenc-encoding-matrixes-for-ampere-gpus),
[NVENC — Wikipedia](https://en.wikipedia.org/wiki/NVENC)). So on the A5000 the real choice is
**HEVC vs H.264**, with AV1 reserved as a "when the host GPU is Ada+" upgrade — the protocol
must negotiate codec per session, not hard-wire one.

- **HEVC (primary):** ~25–40% lower bitrate than H.264 at equal quality; supports 4:4:4 and
  10-bit; the natural default for a bandwidth-constrained many-desktop host. Cost: browser
  HEVC decode is HW-gated and less universal (Safari/Edge/Chrome with a HW decoder; Firefox
  spotty) — **NEEDS VERIFICATION per target fleet**.
- **H.264 (fallback):** universally decodable in every WebCodecs browser; use when the client
  can't advertise HEVC. Slightly more bits for the same quality.

**Ultra-low-latency NVENC config** (Video Codec SDK 13, preset P1–P4 + `NV_ENC_TUNING_INFO_
ULTRA_LOW_LATENCY`; the tuning enum auto-sets most knobs)
([NVENC API Programming Guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html),
[SDK 10 presets blog](https://developer.nvidia.com/blog/introducing-video-codec-sdk-10-presets/)):

- **Infinite GOP, IPPPP…, no B-frames, no look-ahead** — B-frames and look-ahead trade
  latency for compression and are disqualified; `delay=0`.
- **Intra-refresh (GDR) instead of periodic IDR.** A periodic keyframe is a bitrate spike that
  blows the pacing budget and, on loss, a full-frame stall. Intra-refresh sprays a *wave* of
  intra macroblocks across N frames (e.g. a moving band refreshing the whole picture every
  ~30–60 frames), giving **constant bitrate and continuous error recovery** without ever
  sending a monolithic IDR — the single most important ULL knob for VDI
  ([NVENC API guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html)).
  On packet loss the client requests an *on-demand* intra-refresh wave rather than a keyframe.
- **Rate control:** CBR with a **single-frame VBV** (`vbvBufferSize = bitrate/fps`) so the
  encoder can never bank bits and burst — bounds per-frame size to bound network jitter. CQP
  is the alternative for a fixed-quality LAN where bytes are cheap.
- **Slice-based encoding + `reportSliceOffsets`.** Split each frame into K horizontal slices;
  NVENC reports each slice as it completes, so the packetizer ships slice 0 while slice K−1 is
  still encoding — **pipelining that hides most of the encode term** and bounds a lost packet's
  blast radius to one slice ([NVENC API guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html)).
- **Per-region QP** via NVENC's ROI / QP-map (`qpMapMode`): spend bits where the guest damage
  map says content changed and where OCR-class heuristics flag text edges; starve static
  background. This is the encoder-side lever the hybrid (§2) drives.
- **On-GPU feed, zero readback:** `VkImage → vkGetMemoryFdKHR → cuImportExternalMemory →
  CUdeviceptr → NvEncRegisterResource(CUDADEVICEPTR)` — the interop chain proven in doc 09.
  ARGB goes straight in; the RGB→NV12 CSC is fixed-function inside NVENC. No pixel ever
  touches system RAM. **This is the line SPICE cannot cross.**

Encode cost in this config is **~1–3 ms/frame on Ampere** (doc 09), a small slice of budget.

## 2. Damage-aware hybrid encoding — the VDI specialization

A VDI desktop is 95% static text/2D with occasional dynamic islands (a video player, a 3D
viewport, a scrolling list). Encoding the *whole* screen as H.264/HEVC every frame is what
naive game-streaming does and it is wasteful and ugly for text: chroma-subsampled 4:2:0 blurs
sub-pixel-hinted fonts, and an idle screen still emits a CBR stream. Every serious VDI
protocol splits the screen by content class. **RDP's Graphics Pipeline runs an image
classifier — text vs image — and encodes each with a different codec**; its **AVC444** mode
even carries two H.264 sub-streams (a luma view + an auxiliary chroma view) reassembled into
crisp 4:4:4 so text stays sharp
([Azure Virtual Desktop graphics encoding](https://learn.microsoft.com/en-us/azure/virtual-desktop/graphics-encoding),
[MS-RDPEGFX YUV444v2](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/781406c3-5e24-4f2b-b6ff-42b76bf64f6d)).
NICE/Amazon DCV does the same tiered dirty-region approach for its VDI streaming
([DCV](https://aws.amazon.com/blogs/aws/nice-desktop-cloud-visualization-dcv-is-now-amazon-dcv/)).
We copy the pattern and wire it to **our own guest damage tracking (doc 17)** — we already get
per-frame dirty rects, idle detection, and foreground awareness for free from the guest driver,
so classification is cheap.

**infiniPixel's two lanes, composited client-side:**

1. **Static UI lane (reliable):** dirty rects the guest reports as low-churn are encoded as
   **near-lossless tiles** — a fast intra-only pass (deflate/RLE for flat UI, or an NVENC
   intra-4:4:4 tile for photographic UI) — and sent on a **reliable** WebTransport stream.
   They arrive exactly once, persist in the client's tile cache, and are drawn under the video
   layer. Unchanged region → **zero bytes, zero encode**.
2. **Dynamic-region lane (unreliable):** rects with high inter-frame churn (video/3D, detected
   by damage-rate over a threshold or an explicit "this is a swapchain scanout" hint) are fed
   to the **NVENC video stream** (§1), cropped to a bounding box, on **unreliable** datagrams.
   A dropped slice is old news in <16 ms; don't retransmit, let intra-refresh heal it.

A tiny **classifier** in the arbiter promotes/demotes rects with hysteresis (a region that
churns for >M frames becomes a video region; one that goes quiet for >N frames flushes a final
lossless tile and leaves the video lane) — exactly RDP's moving-region promotion. The client
composites: tile cache (WebGL textures) + decoded video frame (cropped blit) + HW cursor
sprite (§5) into one canvas.

**Quantified win — idle 1080p60 desktop.** Full-frame HEVC CBR still runs the pipe at its
floor: even a "static" screen emits P-frames at cadence, realistically **~3–8 Mbit/s** and a
full encode+decode every 16.6 ms. With the hybrid, an idle desktop sends **nothing** — the
damage map is empty, so 0 tiles, 0 video slices, ~**0.0x Mbit/s** (just keep-alive/cursor,
low-kbit/s). A blinking cursor or clock is a few tiny tiles — **single-digit KB/s**. That is a
**~100–1000x** bandwidth reduction on the common case and it removes the encode/decode work
entirely when idle, which is what lets one A5000's two NVENC blocks fan out to dozens of
desktops. Full-frame NVENC only wins inside the *dynamic island*, which is where we use it.

## 3. Transport — WebTransport (QUIC), not custom UDP, not WebRTC

Three candidates, judged for a **self-hosted, LAN-first, browser-reachable** VDI:

- **Custom UDP + FEC + partial reliability (Moonlight/Sunshine/ENet).** The latency floor:
  RTP-over-UDP with Reed-Solomon FEC (Sunshine defaults ~20%, raised to 30–40% on lossy WAN)
  and an ENet-style reliable control channel
  ([Sunshine UDP media streaming](https://deepwiki.com/qiin2333/foundation-sunshine/7.3-udp-media-streaming),
  [Sunshine architecture](https://deepwiki.com/LizardByte/Sunshine)). **Disqualifier: not
  reachable from a browser.** It needs a native client — exactly the `.vv`/remote-viewer
  situation we're trying to kill. Keep it as the *design reference* for our datagram+FEC layer,
  not as the transport.
- **WebRTC (DataChannel + media).** Mature congestion control (GCC), NAT traversal
  (ICE/STUN/TURN), and browser-native — the default for internet-facing streaming. But for a
  **LAN** VDI it's the wrong tool: SDP/ICE handshake latency, a signalling server, an opinionated
  jitter buffer and pacer you fight rather than drive, and DataChannel-over-SCTP-over-DTLS
  overhead. Insertable Streams let you inject custom-encoded frames but you're swimming upstream.
  Its wins (NAT, GCC) are WAN concerns we mostly don't have inside a department LAN.
- **WebTransport over HTTP/3/QUIC (recommended).** As of 2026 it is broadly shipping —
  **Chrome 97+, Edge 98+, Firefox 114+, Safari 26.4+, Opera, Samsung Internet**
  ([WebTransport browser support](https://www.testmuai.com/learning-hub/webtransport-browser-support/),
  [MDN WebTransport](https://developer.mozilla.org/en-US/docs/Web/API/WebTransport_API)) — so a
  browser client is finally realistic without WebRTC's baggage. QUIC gives us exactly the two
  primitives the hybrid needs in one connection: **unreliable datagrams** (UDP-like, for video
  slices — drop and move on) and **reliable, independent streams** (for tiles, input, control —
  no head-of-line blocking between streams). Plus 0-/1-RTT setup, mandatory TLS 1.3, and
  built-in congestion control. This is precisely the direction the VDI industry took: **Amazon
  DCV made QUIC/UDP its default transport in 2024**, streaming 4K60, and recommends UDP once
  latency/loss rise ([DCV enable QUIC](https://docs.aws.amazon.com/dcv/latest/adminguide/enable-quic.html),
  [DCV 4K60 over QUIC](https://aws.amazon.com/blogs/gametech/stream-remote-environment-nice-dcv-quic-udp-4k-monitor-60-fps/)).

**Recommendation:** WebTransport primary, with a **WebSocket-over-TCP fallback** for the odd
proxy/browser that blocks HTTP/3 — mirroring DCV's QUIC-with-TCP-fallback posture. On a clean
LAN (<1 ms RTT, ~0 loss) TCP is actually competitive (DCV only recommends QUIC above ~50–70 ms
latency ([NI-SP DCV performance guide](https://www.ni-sp.com/knowledge-base/dcv-general/performance-guide/))),
so the fallback is not a cliff; QUIC's per-stream independence and datagrams still pay off under
Wi-Fi loss and host contention.

**Loss handling — FEC vs retransmit, decided by RTT.** For interactive video the rule is: never
spend an RTT you can't afford. On **LAN** (RTT <1 ms) a *deadline-bounded selective retransmit*
of a lost slice beats FEC's constant overhead — ask again, it arrives before present. On
**Wi-Fi/WAN** (RTT tens of ms) a retransmit misses the frame, so apply **Moonlight-style ~20%
Reed-Solomon FEC** to the video datagrams and skip retransmit. Make it **adaptive**: raise FEC
as measured loss rises, and fall back to an on-demand intra-refresh wave (not an IDR) when a
region is unrecoverable ([adaptive FEC tuning, arXiv](https://arxiv.org/pdf/2602.09880)).
Tiles and input always ride reliable streams — correctness there is non-negotiable.

**Datagram framing (binary, zerocopy-friendly per doc 11):** `{vm_id, frame_seq, slice_idx,
slice_count, fec_group_id, flags(refresh|region_id), pts_us}` + HEVC/H.264 slice NAL. FEC groups
are K data + M parity datagrams. Tiles on a reliable stream: `{region_id, rect(x,y,w,h),
codec(raw|deflate|intra), seq, pts_us}` + payload. Input on its own reliable ordered stream;
control (codec/res/fps negotiation, adaptation, refresh requests) on another.

## 4. Frame pacing & motion-to-photon budget (<30 ms LAN)

Pacing rules that keep the pipe shallow:

- **Queue depth 1:** NVENC `async_depth=1`, one in-flight frame; never let surfaces pool.
- **Pace to the client, not the guest vblank.** The client reports its `requestAnimationFrame`
  cadence and target fps; the arbiter encodes at that rate and **skips unchanged frames** using
  the damage map — no dirty rects, no frame. Do not lock guest→host vblank (doc 09 §5).
- **Adaptive on two axes.** (a) *Network:* QUIC congestion/loss signals drive bitrate → then
  resolution → then fps, in that order (drop bits before pixels before smoothness). (b) *Host
  capacity:* when the two NVENC blocks or the GPU are saturated across many VMs, the **doc 16
  degradation budget** caps per-VM fps/res so no tenant starves another — background/idle VMs
  degrade first (guest foreground-awareness from doc 17 informs priority).

**Display-datapath latency budget (1080p60, LAN, protocol-only — excludes the guest app's own
frame-draw, which is not ours to pay):**

| Stage | Cost (LAN) | Notes |
|---|---|---|
| Frame already on host GPU | 0 ms | blob dma-buf; no capture, no readback |
| Export → CUDA import → NVENC | 2–4 ms | handle import + fixed-function CSC + encode; slice-pipelined |
| Packetize / FEC / send first slice | 0.3–0.8 ms | slice 0 ships while later slices encode |
| Host → client network | 0.3–1 ms | one-way LAN QUIC datagram |
| Client jitter buffer | 2–8 ms | tunable; 1-in-1-out with `optimizeForLatency` minimizes it |
| WebCodecs HW decode | 2–4 ms | on-GPU decode in the browser |
| Composite + present | 8–16 ms | ≤ one client refresh; the irreducible display term |
| **Motion-to-photon (datapath)** | **~14–22 ms** | plus input→host (1–5 ms) and the app's own frame boundary |

The display datapath itself fits **well under 30 ms on LAN**; the remaining budget is the
client's refresh interval (physics, not protocol) and the guest app's own render cadence.

**Where SPICE loses, concretely.** The current path
(`backend/app/services/console/SpiceProxyService.ts`, a transparent `client.pipe(upstream)` TCP
relay to QEMU's own SPICE server) forces: **(1)** a GPU→system-RAM **readback** of every frame
(2–5 ms + PCIe bus, and it *pulls the frame off the GPU we just rendered on*); **(2)** a
**CPU/VA-API encode** in QEMU/spice-server (higher latency than NVENC and it burns host CPU we
want for arbiters); **(3)** **TCP** transport — head-of-line blocking, Nagle, and RTT-multiplying
retransmit stalls under any loss; and **(4)** a **native remote-viewer** (`.vv` download), not a
browser. infiniPixel deletes (1) entirely (frame stays on-GPU through NVENC), replaces (2) with
fixed-function NVENC, replaces (3) with QUIC datagrams + FEC/partial-reliability, and replaces (4)
with an in-browser WebCodecs client. The hybrid (§2) additionally makes the idle-desktop common
case nearly free, which SPICE's per-frame pipeline never achieves.

## 5. Client — WebCodecs decode → WebGL composite, replacing the .vv viewer

Feasible today. WebCodecs exposes low-level HW `VideoDecoder`/`VideoEncoder` for H.264, HEVC,
AV1, VP8/9, decoding on the GPU media stack with automatic SW fallback
([MDN WebCodecs](https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API)). The path:

1. **Decode:** reassemble slices per `frame_seq` (apply FEC if a group is short), build an
   `EncodedVideoChunk`, feed `VideoDecoder.decode()` configured with
   `optimizeForLatency: true` and HW preference — pushing toward 1-in-1-out so a frame in yields
   a frame out with no reorder buffer ([WebCodecs 1-in-1-out, w3c/webcodecs#732](https://github.com/w3c/webcodecs/issues/732)).
2. **Composite (the hybrid reassembly):** upload the decoded `VideoFrame` as a WebGL texture and
   blit it into its region rect; draw the **tile cache** (static UI textures from the reliable
   stream) behind/around it; draw the **HW cursor** as a separate sprite driven by the cursor
   channel — cursor motion updates at input latency and **never re-encodes the frame** (doc 09
   §5, RDP/DCV all keep cursor off the video plane). One `requestAnimationFrame` paints the
   layered canvas.
3. **A/V sync:** audio is a separate low-latency lane — Opus via WebCodecs `AudioDecoder` (or a
   WebAudio worklet) on its own reliable-ish stream. Both media carry `pts_us` on a shared
   session clock; for a desktop, video motion-to-photon is king, so we sync audio *to* the video
   PTS with a small (~30–50 ms) audio buffer and let audio absorb the slack rather than delay
   video. Lip-sync tolerance for VDI is generous; interactive pointer latency is not.

This replaces `frontend/src/utils/spiceConnect.js`'s `.vv` download and native remote-viewer
with a browser canvas — the same UX win DCV got moving to its web client, and it slots beside
the Phase-1 "encoded console stream" sibling service doc 09 already scoped in
`backend/app/services/console/`.

## Recommendation (the protocol, in one line)

**infiniPixel = NVENC HEVC (H.264 fallback, AV1 when host is Ada+) · infinite-GOP · intra-refresh
· single-frame-VBV CBR · slice-pipelined · ROI-QP · zero-readback VkImage→CUDA→NVENC**, wrapped
in a **damage-aware hybrid** (reliable near-lossless UI tiles + unreliable cropped video regions,
composited client-side, tied to guest damage tracking doc 17), carried over **WebTransport/QUIC**
(datagrams+FEC for video, reliable streams for tiles/input/control; WebSocket/TCP fallback;
adaptive FEC-vs-retransmit by RTT), **paced to the client with async_depth=1 and frame-skip**,
degrading under the doc 16 host-capacity budget, and **decoded in-browser with WebCodecs onto a
WebGL canvas**. Net vs SPICE: no GPU readback, HW encode instead of CPU, QUIC instead of TCP,
browser instead of `.vv`, and a near-free idle desktop — a **~14–22 ms LAN display datapath**
against SPICE's readback+CPU-encode+TCP path.

## Sources

- NVIDIA Ampere video encode/decode matrix (no AV1 encode, AV1 decode only): https://www.techpowerup.com/273420/nvidia-updates-video-encode-and-decode-matrix-with-reference-to-ampere-gpus
- NVIDIA NVENC/NVDEC support matrix (forum thread): https://forums.developer.nvidia.com/t/video-encode-and-decode-gpu-support-matrix/64780
- VideoCardz — Ampere adds AV1 decode, not encode: https://videocardz.com/newz/nvidia-updates-nvdec-video-decoding-and-nvenc-encoding-matrixes-for-ampere-gpus
- NVENC — Wikipedia (generation/codec support): https://en.wikipedia.org/wiki/NVENC
- NVENC Video Encoder API Programming Guide (ULL tuning, intra-refresh, slices, VBV): https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html
- NVIDIA Video Codec SDK 10 presets (P1–P7 + tuning info): https://developer.nvidia.com/blog/introducing-video-codec-sdk-10-presets/
- OBS advanced NVENC options (practical ULL knobs): https://obsproject.com/kb/advanced-nvenc-options
- Sunshine architecture (GameStream, RTSP, UDP media): https://deepwiki.com/LizardByte/Sunshine
- Sunshine UDP media streaming (RTP + Reed-Solomon FEC ~20%): https://deepwiki.com/qiin2333/foundation-sunshine/7.3-udp-media-streaming
- Adaptive FEC parameter tuning for video streaming (TAROT, arXiv): https://arxiv.org/pdf/2602.09880
- WebTransport API (MDN, datagrams + streams): https://developer.mozilla.org/en-US/docs/Web/API/WebTransport_API
- WebTransport browser support 2026 (Chrome/Edge/Firefox/Safari 26.4+): https://www.testmuai.com/learning-hub/webtransport-browser-support/
- HTTP/3 browser support (caniuse): https://caniuse.com/http3
- WebCodecs API (MDN, HW decode H.264/HEVC/AV1): https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API
- WebCodecs 1-in-1-out / optimizeForLatency (w3c/webcodecs#732): https://github.com/w3c/webcodecs/issues/732
- Amazon DCV — QUIC/UDP default transport since 2024.0: https://docs.aws.amazon.com/dcv/latest/adminguide/enable-quic.html
- Amazon DCV — 4K60 streaming over QUIC/UDP (AWS gametech): https://aws.amazon.com/blogs/gametech/stream-remote-environment-nice-dcv-quic-udp-4k-monitor-60-fps/
- NI-SP DCV performance guide (QUIC recommended above ~50–70 ms latency): https://www.ni-sp.com/knowledge-base/dcv-general/performance-guide/
- NICE DCV is now Amazon DCV (2024.0): https://aws.amazon.com/blogs/aws/nice-desktop-cloud-visualization-dcv-is-now-amazon-dcv/
- Azure Virtual Desktop — RDP graphics encoding (text/image classifier, AVC444): https://learn.microsoft.com/en-us/azure/virtual-desktop/graphics-encoding
- MS-RDPEGFX — YUV444 stream combination (dual luma/chroma H.264): https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/781406c3-5e24-4f2b-b6ff-42b76bf64f6d
- Azure Virtual Desktop — increase chroma to 4:4:4: https://learn.microsoft.com/en-us/azure/virtual-desktop/graphics-chroma-value-increase-4-4-4
- Infinibay SPICE relay (read in-repo): backend/app/services/console/SpiceProxyService.ts, frontend/src/utils/spiceConnect.js
- Prior corpus: docs 06 (data plane / host GPU), 09 (presentation latency / NVENC on A5000), 11 (wire envelope), 16 (host-capacity degradation), 17 (guest damage tracking)
