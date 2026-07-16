# 19 — Remote desktop I/O + the honest Infinibay SPICE integration/migration

**Scope:** everything a remote *desktop* protocol needs **beyond pixels** — input, audio,
clipboard/USB/file/multi-monitor, session security — plus a concrete plan to migrate
Infinibay off its transparent SPICE relay without losing the many non-video things SPICE
quietly does. This builds on **doc 09** (the pixel datapath: dma-buf → CUDA import → NVENC,
plus the SPICE touch-point) and **doc 11** (the wire protocol: control ring, `EVENT`,
`CURSOR_*`). Doc 09 settled the frame; this doc settles the *loop around* the frame.

## Verdict up front

**HYBRID, not full replace — and use the browser as the primary client.** infinigpu owns
the **latency-critical loop** (video + input + audio) as a new encoded-console-stream service
that talks to the arbiter, which already holds the frame on-GPU. SPICE/`vdagent`/`usbredir`
is **retained** as (a) the whole-session fallback for no-KVM / no-NVENC / thin-client / legacy
and (b) the carrier for USB redirection and clipboard/file transfer, which are large,
low-value-to-reinvent, and not latency-critical. Reinventing `usbredir` and SPICE's mature
2D-desktop diffing buys us nothing on motion-to-photon; the GPU display loop is the only place
the latency actually lives (doc 09 §7). **Fallback ladder:** infinigpu-native (browser,
WebCodecs + NVENC) → infinigpu software x264 → SPICE `.vv` native viewer.

**Transport recommendation:** target **WebTransport (HTTP/3 + QUIC) + WebCodecs**, with
**WebRTC DataChannel + WebCodecs** as the proven interim. We own all three ends
(guest driver, host arbiter, browser client), so we do **not** need WebRTC's SFU/ICE/NAT
machinery; a QUIC pipe to a server we control is a cleaner fit and now ships in all major
browsers ([Media over QUIC status, 2026](https://www.nanocosmos.net/blog/media-over-quic-moq/),
[webrtcHacks: WebCodecs/WebTransport vs WebRTC](https://webrtchacks.com/webcodecs-webtransport-and-webrtc/)).
Parsec already proves the interim path works in a browser today (WebRTC DataChannel + MSE
low-delay + WASM-Opus) ([Parsec browser tech](https://parsec.app/blog/game-streaming-tech-in-the-browser-with-parsec-5b70d0f359bc)).

---

## 1. Input — capture, transport, injection

The input loop is **browser capture → reliable control channel → guest HID injection**, and
it must be a *separate* channel from video so a full video frame never head-of-line-blocks a
keystroke. Moonlight/Sunshine model this exactly: after RTSP negotiation, a dedicated
**ENet reliable-UDP control channel** carries `IDX_INPUT_DATA` (client→host) and
`IDX_RUMBLE_DATA` / `IDX_SET_ADAPTIVE_TRIGGERS` (host→client), separate from the video and
audio UDP flows ([Sunshine data plane](https://deepwiki.com/LizardByte/Sunshine/4.4-udp-streaming-and-data-plane)).

**Browser capture.** All four input classes are reachable from a modern browser:
- **Mouse (absolute)** — standard `mousemove`/`pointermove`, mapped to guest display coords.
- **Mouse (relative / pointer-lock)** — the **Pointer Lock API** delivers raw `movementX/Y`
  deltas with no cursor and no viewport clamp, which is what FPS/CAD apps and
  `SetCursorPos`-driven guests need ([MDN Pointer Lock](https://developer.mozilla.org/en-US/docs/Web/API/Pointer_Lock_API),
  [web.dev pointerlock](https://web.dev/articles/pointerlock-intro)).
- **Keyboard** — `keydown`/`keyup`; use the **Keyboard Lock API** to capture OS-reserved
  chords (Alt-Tab, Win, Esc) so they reach the guest, not the browser. Both Pointer Lock and
  Keyboard Lock are **permission-gated from Chrome 131** — the session UI must request them
  ([Chrome 131 permission change](https://developer.chrome.com/blog/keyboard-lock-pointer-lock-permission)).
- **Gamepad** — the **Gamepad API is polled, not event-driven**, so the client samples it
  each rAF tick and diffs ([Parsec browser tech](https://parsec.app/blog/game-streaming-tech-in-the-browser-with-parsec-5b70d0f359bc)).
- **Multi-touch** — Pointer Events (`pointerType==="touch"`, per-pointer IDs) → a guest
  multi-touch HID (Windows accepts up to 10 contacts via a digitizer HID).

**Transport.** Input frames are tiny binary structs (type, timestamp, deltas/scancode/button
mask). Send them on a **reliable, ordered** channel: a dropped `keyup` is a *stuck key*, a
worse failure than a dropped video frame. A WebRTC `RTCDataChannel` is UDP→SCTP→DTLS and can
be tuned unreliable/unordered, but for input we keep it **reliable+ordered** and simply
coalesce pointer deltas per tick to bound rate ([Parsec: DataChannel = UDP/SCTP/DTLS](https://parsec.app/blog/game-streaming-tech-in-the-browser-with-parsec-5b70d0f359bc)).
On WebTransport, use a **reliable bidirectional stream** for keyboard/buttons and an
**unreliable datagram** for coalesced pointer deltas (latest-wins; a lost delta is corrected
by the next one). This mirrors ENet's reliable control channel but on QUIC.

**Injection into the guest — the seam.** Three options, in order of ownership:
1. **QMP `input-send-event` (Phase-0 reuse).** infinization already wires an emulated
   `usb-tablet` (absolute pointer) and `usb-kbd` into every VM
   (`infinization/src/core/QemuCommandBuilder.ts:958`, `addUsbKeyboard`). QEMU exposes
   `input-send-event` over QMP, which routes into exactly those HID devices — so the arbiter
   can inject without any new guest driver. Good enough to prove the loop; adds QMP JSON
   round-trip jitter and can't do relative-mode cleanly. **NEEDS VERIFICATION:** per-event QMP
   throughput under fast mouse motion (hundreds of events/s).
2. **A dedicated `virtio-input`-style HID device (Phase-1, recommended).** Our device model
   (vfio-user, doc 07) presents a small **relative+absolute HID** the arbiter writes to
   directly, bypassing QEMU's input subsystem entirely — the lowest-latency, most-owned path,
   and it lets us toggle relative/absolute pointer mode in lockstep with the browser's
   pointer-lock state. This is net-new but small (evdev-shaped event structs), and consistent
   with "own all three ends." **NEEDS VERIFICATION:** relative pointer over the vfio-user seam.
3. **infiniservice agent — rejected for input.** The Rust agent's virtio-serial NDJSON channel
   is telemetry + `SafeCommand`/`UnsafeCommand` with **500 ms inbound polling** (CLAUDE.md);
   it is the wrong tool for 8 ms-cadence HID and would add ~250 ms mean latency. Keep it for
   clipboard/file (§3), never for input.

**Client-side cursor prediction.** The single biggest perceived-latency win. In naive
*server mouse mode* the cursor round-trips (client→host→re-render→client), so pointer lag =
full motion-to-photon ([USPTO 9,798,436 low-latency mouse mode](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/9798436)).
Instead use **client mouse mode**: render the cursor **locally** at the predicted position the
instant the user moves, hide the guest's cursor, and treat host-reported cursor position/shape
(from doc 09/11's dedicated **HW-cursor channel**, never re-encoded into video) as the
authoritative correction. Parsec does exactly this — "a snappy local feel so you are not
constantly reminded you are streaming" ([Parsec browser tech](https://parsec.app/blog/game-streaming-tech-in-the-browser-with-parsec-5b70d0f359bc)).
Cursor *shape* changes (I-beam, resize) still come from the host on the cursor channel.

**Input latency budget (LAN, browser client):**

| Stage | Cost | Note |
|---|---|---|
| Browser event → serialize | 0.5–2 ms | rAF quantization; poll for gamepad |
| Client → host (control channel) | 1–5 ms | one-way LAN; WAN adds RTT/2 |
| Inject (virtio-input HID) | <1 ms | QMP path adds ~1–3 ms + jitter |
| Guest processes + repaints | next frame | folded into doc 09's render term |
| **Input → guest sees it** | **~3–8 ms LAN** | cursor *feels* instant via local prediction |

The cursor prediction decouples *perceived* pointer latency (~0 ms, local) from *actual*
click-registration latency (~3–8 ms), which is the whole trick.

---

## 2. Audio — low-latency bidirectional Opus

**Codec: Opus**, the WebRTC-mandated real-time codec, decodable in-browser via WASM or (target)
WebCodecs `AudioDecoder`. Moonlight and Parsec both ship **20 ms Opus @ 48 kHz** with FEC
([Sunshine](https://deepwiki.com/LizardByte/Sunshine/4.4-udp-streaming-and-data-plane),
[Parsec](https://parsec.app/blog/game-streaming-tech-in-the-browser-with-parsec-5b70d0f359bc)).
The 20 ms default carries **22.5 ms algorithmic latency**; Opus supports frames down to
**2.5 ms**, so for interactive VDI use **10 ms frames** to shave ~10 ms with negligible
efficiency loss ([Opus, Hydrogenaudio](https://wiki.hydrogenaudio.org/index.php?title=Opus)).

- **Downstream (guest → user):** desktop/media audio, **128–256 kbps** stereo, CBR-ish, no DTX
  (we want continuous). Capture is host-side: the guest renders into a **virtual audio sink**
  (a QEMU `ich9-intel-hda`/`virtio-sound` sink, or our own device), the arbiter reads PCM and
  Opus-encodes on the same box as the video, then muxes onto its own channel — mirroring
  Moonlight's separate audio UDP flow with FEC ([Sunshine audio: Opus + `audio_fec_packet_t`](https://deepwiki.com/LizardByte/Sunshine/4.4-udp-streaming-and-data-plane)).
- **Upstream (user mic → guest), for Teams/Zoom:** browser `getUserMedia` → Opus **48 kHz mono
  64–96 kbps** with **in-band FEC + DTX** (recover up to ~25 % loss; suppress silence)
  ([Opus FEC/DTX](https://wiki.hydrogenaudio.org/index.php?title=Opus)) → a **virtual audio
  source** the guest sees as a microphone. Webcam for conferencing is handled by USB redirect
  or a virtual video source fed from `getUserMedia` (§3).

**A/V sync.** Stamp audio and video with a **single host presentation clock** (RTP-style
timestamps, or WebCodecs `timestamp` on both `EncodedVideoChunk`/`EncodedAudioChunk`).
The client keeps a small shared jitter buffer (doc 09 budgets 5–15 ms) and **slaves video to
the audio clock** — gaps in audio are far more perceptible than a dropped frame, so audio
paces. Bidirectional echo cancellation for the mic path is done in the guest app (Teams/Zoom
do their own AEC); we just deliver clean full-duplex Opus.

---

## 3. Desktop features SPICE gives — build natively vs keep on a side channel

SPICE is *not* a video protocol; it is a desktop-integration suite. The honest split:

| Feature | Decision | Rationale |
|---|---|---|
| **Multi-monitor + dynamic resolution** | **BUILD native** | Already in the datapath: multiple virtio-gpu **scanouts**, each its own blob + NVENC session (free on the qualified A5000, doc 09 §5). Dynamic res = a host→guest `EVENT{display_change}` (doc 11) + guest mode-set. This is core VDI; owning it is the point. |
| **Clipboard (bidi text/image)** | **KEEP on agent** | Low-rate, security-sensitive (must be RBAC-gated + size-capped), not latency-critical. Carry it as a new NDJSON message type on the **existing infiniservice** virtio-serial channel rather than resurrecting SPICE `vdagent`. |
| **File transfer** | **KEEP on agent** | Same channel; drag-drop maps to a chunked file message. SPICE does this via `vdagent`+`filetransfer` ([SPICE user manual](https://www.spice-space.org/spice-user-manual.html)); we already own an agent transport. |
| **USB redirection** | **KEEP `usbredir`** | `usbredir` is a **protocol independent of SPICE** — "created for SPICE but completely independent, could be used for other remote desktop protocols" ([usbredir](https://www.spice-space.org/usbredir.html)). Reinventing USB device emulation is enormous for little gain. Keep `qemu-xhci` + `usbredir`; a browser can source devices via **WebUSB** (emerging) or the native viewer sources them. Deferred milestone. |
| **Webcam / mic** | **BUILD (audio) / KEEP (webcam)** | Mic via §2; webcam either via `usbredir` or a virtual video source. NICE DCV proves both are table-stakes for VDI (webcam redirection, multi-channel audio) ([NICE DCV features](https://www.ni-sp.com/support-old/nice-dcv-tips-and-tricks/)). |
| **Smartcard / stylus / printer** | **KEEP `usbredir`/agent** | Niche; DCV routes them over USB too. No reason to build. |
| **Seamless/rootless windows** | **OUT OF SCOPE** | Neither SPICE nor we do this well; explicit non-goal. |

Net: infinigpu **builds** exactly what is in the GPU datapath (display, multi-monitor, dynamic
res, cursor) and **rides existing channels** (infiniservice agent, `usbredir`) for the rest.
That is the hybrid, concretely.

---

## 4. Security / session — tied to Infinibay auth, fail-closed

**Encryption.** Every transport option is authenticated-encrypted by construction:
WebTransport = **QUIC-TLS 1.3** (mandatory); WebRTC media/data = **DTLS-SRTP**, with optional
**SFrame end-to-end** via the WebRTC **Encoded Transform** API if we ever terminate media at an
intermediary ([webrtcHacks E2EE](https://webrtchacks.com/true-end-to-end-encryption-with-webrtc-insertable-streams/)).
For the native x264 / raw-UDP fallback, follow Moonlight: **AES-GCM** on the control channel
with a monotonic `seq` as the IV and a 16-byte tag, per-frame IV+tag on video
([Sunshine crypto](https://deepwiki.com/LizardByte/Sunshine/4.4-udp-streaming-and-data-plane)).

**Auth tied to Infinibay's JWT/session model.** Reuse `SpiceProxyService`'s proven scaffolding
(`backend/app/services/console/SpiceProxyService.ts`): session creation is **gated at the
resolver by the `vm:console` permission** (RBAC), and the upstream `(host, port)` is resolved
**server-side from the VM's node record — never from client input** (no SSRF pivot). The new
service mints a **short-TTL console ticket** bound to `(userId, vmId, expiry)` — the analogue
of SPICE's per-VM ticket — that the browser presents on the WebTransport/WebRTC handshake. The
frontend already injects the raw JWT on its Apollo link; the ticket is issued by an
authenticated mutation, so the media plane itself never sees the JWT.

**Fail-closed posture** (consistent with the rest of Infinibay — HMAC-signed agent commands,
per-VM firewalls): no ticket / expired / wrong-VM / capacity-exceeded → **reject**; idle and
hard-lifetime timeouts tear the session down (SpiceProxyService already does exactly this,
`idleMs`/`maxLifetimeMs`/`maxSessions`). The arbiter binds one media session to one VM's
dma-buf and will not cross VMs.

---

## 5. Integration & migration with Infinibay — the concrete plan

**Today** (read in-repo): `SpiceProxyService.ts` is a **transparent TCP relay** —
`client.pipe(upstream); upstream.pipe(client)` between a client port (6100–6199) and QEMU's own
SPICE/VNC server — and `frontend/src/utils/spiceConnect.js` hands the user a **`.vv` file** that
launches a **native** `remote-viewer`. Zero pixel handling in Infinibay; QEMU/SPICE does the
GPU→sysmem readback + CPU encode (doc 09 §4).

**New service: `EncodedConsoleStreamService`** — a *sibling* in
`backend/app/services/console/`, deliberately reusing SpiceProxyService's port-range,
idle/hard-lifetime timers, `maxSessions` cap, and resolver auth gate — but instead of piping
raw TCP to QEMU SPICE, it:
1. terminates a **WebTransport (QUIC) or WebRTC** session from the browser on the master's one
   reachable ingress (same "single ingress, upstream resolved server-side" model);
2. bridges input/audio/control to the **arbiter** (which owns the on-GPU frame + NVENC + the
   virtio-input HID), *not* to QEMU;
3. streams **WebCodecs-decodable** H.264/AV1 elementary video + Opus audio + the cursor channel
   back down.

**Browser client** replaces the `.vv` download: a WebCodecs `<video>`/`VideoDecoder` player
(or MSE low-delay as Parsec uses today) + the §1 input capture + §2 Web Audio playback. It
lives beside `spiceConnect.js`, selected by capability negotiation (§6).

**Recommendation: run the hybrid, do not fully replace SPICE.** The datapath the arbiter owns
(frame already on-GPU, license-free NVENC, virtio-input, Opus) is where a purpose-built
protocol crushes SPICE's readback+CPU-encode round-trip — so **own the display/input/audio
loop**. But `usbredir`, `vdagent`-class clipboard/file, and SPICE's genuinely-good static-desktop
diffing are a large surface with little latency upside — **keep them** (clipboard/file on
infiniservice, USB on `usbredir`, whole-session SPICE as the fallback rung). Full replacement
would mean re-implementing USB device emulation and a 2D-diff desktop codec to *lose* latency on
idle screens (doc 09 §6). The hybrid is strictly better.

---

## 6. Fallback ladder + capability negotiation

The console session opens with a capability handshake (KVM present? A5000 NVENC "qualified"?
browser supports WebTransport/WebCodecs? is the client the native viewer?) and picks the
highest viable rung:

1. **infinigpu-native (best).** Browser: WebTransport(QUIC)+WebCodecs (or WebRTC interim),
   **NVENC** H.264/AV1 (doc 09), virtio-input HID, Opus. Needs KVM + qualified NVENC + our
   device model. Motion-to-photon ~40–70 ms LAN (doc 09 §7).
2. **infinigpu software-encode.** *Same* datapath, client, input, and audio — but the arbiter
   encodes with **x264 `--tune zerolatency` / `ultrafast`** (~8–12 ms/frame + CPU, doc 09 §3)
   when NVENC is absent, the A5000 is saturated, or on a non-NVIDIA host. Still in-browser,
   still our low-latency input/cursor/audio. This is the "no NVENC" rung.
3. **SPICE native viewer (`.vv`) — legacy/thin-client/no-KVM.** The **current, unchanged** path:
   QEMU's SPICE/VNC server + `SpiceProxyService` TCP relay + `remote-viewer`. Serves
   control-plane-only hosts (no `/dev/kvm`, e.g. macOS — CLAUDE.md's "control-plane-only"),
   thin clients, browsers lacking WebCodecs/WebTransport, or any case where the encoded path
   fails. NICE DCV validates keeping a **QUIC-preferred / TCP-fallback** ladder for exactly
   this reason ([NICE DCV QUIC/UDP with WebSocket/TCP fallback](https://www.ni-sp.com/12-11-2020-nice-dcv-releases-version-2020-2-with-new-session-manager-and-performance-enhancements-for-high-fps-interactive-workloads/)).

Both services listen concurrently; the frontend negotiates down the ladder automatically, so a
client that cannot do rung 1 or 2 silently lands on the SPICE it uses today — **no regression**
for anyone, and the fast path for everyone with a modern browser on a KVM+NVENC host.

## Sources

- Sunshine/Moonlight UDP data plane (separate video/audio/control streams, ENet reliable control, `IDX_INPUT_DATA`/`IDX_RUMBLE_DATA`, Reed-Solomon FEC, AES-GCM control / AES-CBC audio, `frame_processing_latency`): https://deepwiki.com/LizardByte/Sunshine/4.4-udp-streaming-and-data-plane
- Moonlight docs — FAQ (gamepad, latency): https://github.com/moonlight-stream/moonlight-docs/wiki/Frequently-Asked-Questions
- Parsec — game streaming tech in the browser (WebRTC DataChannel = UDP/SCTP/DTLS, MSE low-delay video, 20 ms Opus @48 kHz via WASM + Web Audio, Gamepad API polling, Pointer Lock, local cursor): https://parsec.app/blog/game-streaming-tech-in-the-browser-with-parsec-5b70d0f359bc
- Parsec — testing game streaming input latency: https://parsec.app/blog/testing-game-streaming-input-latency-on-parsec-with-diy-instructions-49ae838f45a7
- USPTO 9,798,436 — remote computing low-latency mouse mode (client vs server mouse mode): https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/9798436
- MDN — Pointer Lock API (raw movement deltas, no clamp/cursor): https://developer.mozilla.org/en-US/docs/Web/API/Pointer_Lock_API
- web.dev — Pointer Lock & FPS controls: https://web.dev/articles/pointerlock-intro
- Chrome for Developers — Keyboard Lock & Pointer Lock require permission from Chrome 131: https://developer.chrome.com/blog/keyboard-lock-pointer-lock-permission
- Opus — Hydrogenaudio (frame sizes 2.5–60 ms, 20 ms default = 22.5 ms latency, in-band FEC ~25 % loss, DTX): https://wiki.hydrogenaudio.org/index.php?title=Opus
- NICE DCV — features (clipboard copy/paste, USB smartcard/stylus, webcam redirection, multi-channel audio): https://www.ni-sp.com/support-old/nice-dcv-tips-and-tricks/
- NICE DCV — 2020.2 QUIC/UDP transport with WebSocket/TCP fallback: https://www.ni-sp.com/12-11-2020-nice-dcv-releases-version-2020-2-with-new-session-manager-and-performance-enhancements-for-high-fps-interactive-workloads/
- SPICE — usbredir (protocol independent of SPICE, reusable by other remote-desktop protocols): https://www.spice-space.org/usbredir.html
- SPICE — user manual (vdagent clipboard, file transfer, dynamic resolution): https://www.spice-space.org/spice-user-manual.html
- SPICE — USB redirection channel docs: https://people.freedesktop.org/~teuf/spice-doc/html/ch02s06.html
- webrtcHacks — WebCodecs, WebTransport, and the future of WebRTC: https://webrtchacks.com/webcodecs-webtransport-and-webrtc/
- webrtcHacks — true end-to-end encryption with WebRTC Insertable Streams / SFrame: https://webrtchacks.com/true-end-to-end-encryption-with-webrtc-insertable-streams/
- W3C — webrtc-encoded-transform explainer (Encoded Transform / former Insertable Streams): https://github.com/w3c/webrtc-encoded-transform/blob/main/explainer.md
- Media over QUIC explained (WebTransport+WebCodecs browser support, 2026): https://www.nanocosmos.net/blog/media-over-quic-moq/
- A WebTransport-based system for real-time game streaming (ACM, 2025): https://dl.acm.org/doi/10.1145/3744725.3744726
- VideoSDK — WebRTC low latency (sub-500 ms): https://www.videosdk.live/developer-hub/webrtc/webrtc-low-latency
- Selkies — open-source low-latency WebRTC/WebSocket Linux remote desktop (transport laddering): https://github.com/selkies-project/selkies
- Infinibay (read in-repo): `backend/app/services/console/SpiceProxyService.ts` (relay/auth/idle/session scaffolding), `frontend/src/utils/spiceConnect.js` (`.vv` viewer), `infinization/src/core/QemuCommandBuilder.ts` (`usb-tablet`/`usb-kbd` input devices)
- Internal: doc 09 (presentation path, NVENC, x264 fallback, cursor channel), doc 11 (control ring, `EVENT`, `CURSOR_*`)
