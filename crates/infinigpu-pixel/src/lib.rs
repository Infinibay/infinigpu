//! # infinigpu-pixel — infiniPixel v0 (ADR-0009)
//!
//! The **owned** low-latency remote-display datapath that replaces SPICE's GPU
//! display path: a host-rendered framebuffer is encoded on the GPU's dedicated NVENC
//! block, wrapped in an **owned frame protocol**, streamed over a transport, and
//! decoded in the browser with WebCodecs. We control all three ends, which SPICE
//! (readback → CPU-encode → TCP → native viewer) cannot exploit.
//!
//! ## What v0 delivers (and what it defers)
//!
//! v0 proves the end-to-end path — **encode → own-protocol framing → transport →
//! browser decode → display** — on the smallest honest slice:
//! - **Codec:** H.264 (the ADR's *universal fallback*; broadest WebCodecs support),
//!   encoded on **NVENC** (`h264_nvenc`) — the A5000's dedicated encode engine,
//!   separate from the 3D SMs (ADR-0007 density story). Software x264 is the fallback
//!   rung when no NVENC. Low-latency config: no B-frames, `-tune ull`, CBR.
//! - **Transport:** WebSocket (the ADR's *mandatory browser-reachable fallback*).
//!   WebTransport/QUIC + datagrams/FEC is the v1 target.
//! - **Framing:** [`FrameHeader`] — an owned 32-byte binary header per encoded access
//!   unit, mirrored byte-for-byte by the JS client.
//!
//! Deferred to v1 (all documented in the ADR): damage-aware hybrid (idle ⇒ ~0 bits),
//! intra-refresh/GDR (v0 uses periodic IDR for simple client start-up), the perceptual
//! /foveation layer, HEVC/AV1 negotiation, adaptive control, and the local cursor
//! sprite. None of those change the datapath proven here.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// The infiniPixel wire protocol constants (kept byte-identical with the JS client).
pub mod proto {
    /// Header magic, read little-endian on both ends (`"XIPI"` bytes).
    pub const MAGIC: u32 = 0x4950_4958;
    /// Header size in bytes; the encoded access unit follows immediately.
    pub const HEADER_LEN: usize = 32;
    pub const VERSION: u8 = 1;

    pub mod codec {
        pub const H264: u8 = 1;
        pub const HEVC: u8 = 2;
    }
    pub mod flags {
        /// This access unit is a keyframe (contains SPS/PPS + IDR) — a client may
        /// start decoding here.
        pub const KEYFRAME: u8 = 1 << 0;
    }
}

/// Which codec a stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    pub fn wire(self) -> u8 {
        match self {
            Codec::H264 => proto::codec::H264,
            Codec::Hevc => proto::codec::HEVC,
        }
    }
}

/// One encoded access unit (a complete coded picture in Annex-B).
#[derive(Debug, Clone)]
pub struct EncodedAu {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub seq: u64,
}

/// The owned per-frame header. Little-endian; 32 bytes; mirrored in the JS client.
///
/// ```text
///  off  size  field
///   0    4    magic (LE u32)
///   4    1    version
///   5    1    flags (bit0 = keyframe)
///   6    1    codec (1=H264, 2=HEVC)
///   7    1    reserved
///   8    4    frame_seq (LE u32)
///  12    2    width  (LE u16)
///  14    2    height (LE u16)
///  16    8    pts_us (LE u64)
///  24    4    payload_len (LE u32)
///  28    4    reserved
/// ```
#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub flags: u8,
    pub codec: u8,
    pub frame_seq: u32,
    pub width: u16,
    pub height: u16,
    pub pts_us: u64,
    pub payload_len: u32,
}

impl FrameHeader {
    /// Serialize the header into a fresh 32-byte buffer.
    pub fn to_bytes(&self) -> [u8; proto::HEADER_LEN] {
        let mut b = [0u8; proto::HEADER_LEN];
        b[0..4].copy_from_slice(&proto::MAGIC.to_le_bytes());
        b[4] = proto::VERSION;
        b[5] = self.flags;
        b[6] = self.codec;
        b[8..12].copy_from_slice(&self.frame_seq.to_le_bytes());
        b[12..14].copy_from_slice(&self.width.to_le_bytes());
        b[14..16].copy_from_slice(&self.height.to_le_bytes());
        b[16..24].copy_from_slice(&self.pts_us.to_le_bytes());
        b[24..28].copy_from_slice(&self.payload_len.to_le_bytes());
        b
    }

    /// Build a full wire message: header followed by the access-unit bytes.
    pub fn message(&self, au: &[u8]) -> Vec<u8> {
        let mut m = Vec::with_capacity(proto::HEADER_LEN + au.len());
        m.extend_from_slice(&self.to_bytes());
        m.extend_from_slice(au);
        m
    }
}

// ------------------------------- Annex-B AU splitting -------------------------------

/// Find the next access-unit delimiter (start code + AUD NAL, type 9) at or after
/// `from`, returning the index of its `00 00 01`.
fn find_aud(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 && (buf[i + 3] & 0x1F) == 9 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// True if the access unit contains an IDR (NAL type 5) or SPS (7) — i.e. a keyframe.
fn au_is_keyframe(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let t = au[i + 3] & 0x1F;
            if t == 5 || t == 7 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Splits a raw Annex-B byte stream (with an AUD before every access unit, courtesy of
/// the `h264_metadata=aud=insert` bitstream filter) into complete access units.
#[derive(Default)]
struct AuSplitter {
    buf: Vec<u8>,
}

impl AuSplitter {
    fn push(&mut self, incoming: &[u8], mut emit: impl FnMut(Vec<u8>)) {
        self.buf.extend_from_slice(incoming);
        // Drop any leading bytes before the first AUD.
        match find_aud(&self.buf, 0) {
            Some(0) => {}
            Some(first) => {
                self.buf.drain(0..first);
            }
            None => return,
        }
        // buf[0] is now an AUD. Emit [AUD_n .. AUD_{n+1}) for every complete AU.
        while let Some(next) = find_aud(&self.buf, 4) {
            let au: Vec<u8> = self.buf.drain(0..next).collect();
            emit(au);
        }
    }

    /// Emit the final buffered access unit. `push` always holds the most-recent AU back
    /// because it needs the *next* frame's AUD to delimit it; on end-of-stream (ffmpeg
    /// exit or a resolution-change teardown) that held AU would otherwise be lost. Call
    /// this once the input is done so the last encoded frame still reaches clients.
    fn flush(&mut self, mut emit: impl FnMut(Vec<u8>)) {
        // After `push`, buf is either empty or a single complete AU: `[AUD_last .. end]`
        // (all leading junk drained, no trailing AUD left). Emit it only if it is a real
        // AU — an AUD plus at least one following NAL — not a bare/partial delimiter.
        if self.buf.len() > 6 && find_aud(&self.buf, 0) == Some(0) {
            emit(std::mem::take(&mut self.buf));
        } else {
            self.buf.clear();
        }
    }
}

// ----------------------------------- Encoder ----------------------------------------

/// An NVENC (or software-x264 fallback) H.264 encoder driven through `ffmpeg`.
///
/// Frames are pushed as tightly-packed BGRA on ffmpeg's stdin; a reader thread splits
/// the Annex-B output into access units and forwards them over a channel. Low-latency
/// config: no B-frames, `-tune ull`, CBR, AUD-per-frame for clean framing.
///
/// Using `ffmpeg h264_nvenc` keeps the encode on the GPU's dedicated engine while the
/// project's own NVENC/Vulkan-Video FFI backend is still to come (a codec *backend*,
/// per the ADR-0008 vendor HAL — not the protocol).
pub struct Encoder {
    /// `Option` so a caller can `take_child` and own the reap/kill itself (the
    /// [`PixelStreamer`] session does this so it can `kill()` a wedged ffmpeg — see the
    /// note on [`Encoder::take_child`]). When still present, `Drop` kills + reaps.
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    rx: Option<Receiver<EncodedAu>>,
    codec: Codec,
    hardware: bool,
}

/// Encoder settings.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Prefer `h264_nvenc`; fall back to `libx264` if the hardware encoder fails.
    pub prefer_hardware: bool,
}

impl Encoder {
    pub fn spawn(cfg: &EncoderConfig) -> io::Result<Self> {
        // Try hardware NVENC first; if spawning/among the first bytes it fails, the
        // caller can retry with prefer_hardware=false.
        let hardware = cfg.prefer_hardware;
        let mut child = Command::new("ffmpeg")
            .args(Self::ffmpeg_args(cfg, hardware))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");

        let (tx, rx) = channel();
        thread::spawn(move || Self::reader_loop(stdout, tx));

        Ok(Encoder {
            child: Some(child),
            stdin,
            rx: Some(rx),
            codec: Codec::H264,
            hardware,
        })
    }

    fn ffmpeg_args(cfg: &EncoderConfig, hardware: bool) -> Vec<String> {
        let gop = (cfg.fps * 2).max(2); // periodic IDR every ~2s (v0; intra-refresh is v1)
        let mut a: Vec<String> = vec![
            "-hide_banner", "-loglevel", "error",
            "-f", "rawvideo", "-pix_fmt", "bgra",
            "-s", &format!("{}x{}", cfg.width, cfg.height),
            "-r", &cfg.fps.to_string(),
            "-i", "-",
            "-an",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        if hardware {
            a.extend(
                [
                    "-c:v", "h264_nvenc", "-preset", "p1", "-tune", "ull",
                    "-rc", "cbr", "-b:v", &format!("{}k", cfg.bitrate_kbps),
                    "-bf", "0", "-g", &gop.to_string(), "-forced-idr", "1", "-delay", "0",
                ]
                .into_iter()
                .map(String::from),
            );
        } else {
            a.extend(
                [
                    "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
                    "-x264-params", "bframes=0:scenecut=0",
                    "-b:v", &format!("{}k", cfg.bitrate_kbps), "-g", &gop.to_string(),
                ]
                .into_iter()
                .map(String::from),
            );
        }
        // AUD before each AU so the reader can split cleanly; raw Annex-B on stdout.
        a.extend(
            ["-bsf:v", "h264_metadata=aud=insert", "-f", "h264", "-"]
                .into_iter()
                .map(String::from),
        );
        a
    }

    fn reader_loop(mut stdout: std::process::ChildStdout, tx: Sender<EncodedAu>) {
        let mut splitter = AuSplitter::default();
        let mut seq: u64 = 0;
        let mut chunk = [0u8; 64 * 1024];
        // `Fn` (captures nothing) so both the streaming loop and the final flush can
        // reuse it while each takes its own mutable borrow of `seq`.
        let send_au = |au: Vec<u8>, seq: &mut u64, tx: &Sender<EncodedAu>| {
            let keyframe = au_is_keyframe(&au);
            let s = *seq;
            *seq += 1;
            let _ = tx.send(EncodedAu {
                data: au,
                keyframe,
                seq: s,
            });
        };
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break, // ffmpeg exited
                Ok(n) => splitter.push(&chunk[..n], |au| send_au(au, &mut seq, &tx)),
                Err(_) => break,
            }
        }
        // Emit the last AU the splitter was holding for a delimiter, so the final frame
        // before EOF (process exit or resolution-change teardown) isn't dropped.
        splitter.flush(|au| send_au(au, &mut seq, &tx));
    }

    /// Push one tightly-packed BGRA frame (`width*height*4` bytes) to the encoder.
    pub fn submit_bgra(&mut self, bgra: &[u8]) -> io::Result<()> {
        if let Some(stdin) = self.stdin.as_mut() {
            stdin.write_all(bgra)
        } else {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "encoder stdin closed"))
        }
    }

    /// Move the frame sink out so a producer thread can feed frames while another
    /// thread drains encoded AUs via [`Encoder::recv`]. Closing the returned sink (drop)
    /// flushes and exits ffmpeg.
    pub fn take_sink(&mut self) -> Option<FrameSink> {
        self.stdin.take().map(|stdin| FrameSink { stdin })
    }

    /// Move the AU receiver out so a drain thread can own it directly — leaving the
    /// [`Child`] on the caller's side, so the caller (not a parked drain thread) can
    /// `kill()` a wedged ffmpeg. Pairs with [`Encoder::take_child`].
    pub fn take_rx(&mut self) -> Option<Receiver<EncodedAu>> {
        self.rx.take()
    }

    /// Move the child process handle out so the caller owns kill/reap. Once taken, this
    /// `Encoder`'s `Drop` no longer touches the process.
    pub fn take_child(&mut self) -> Option<Child> {
        self.child.take()
    }

    /// Block for the next encoded access unit (None when the encoder exits or the
    /// receiver was moved out via [`Encoder::take_rx`]).
    pub fn recv(&self) -> Option<EncodedAu> {
        self.rx.as_ref().and_then(|rx| rx.recv().ok())
    }

    pub fn try_recv(&self) -> Option<EncodedAu> {
        self.rx.as_ref().and_then(|rx| rx.try_recv().ok())
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Close stdin so ffmpeg flushes + exits, then kill (in case it wedged and won't
        // exit on stdin close) + reap. A session that took the child via `take_child`
        // owns this itself, so `child` is `None` here and Drop is a no-op.
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl infinigpu_hal::MediaEncoder for Encoder {
    fn caps(&self) -> infinigpu_hal::CodecCaps {
        use infinigpu_hal::{CodecCaps, Vendor, VideoCodec};
        CodecCaps {
            // NVENC on the GPU vs. libx264 on the CPU.
            vendor: if self.hardware { Vendor::Nvidia } else { Vendor::Software },
            hardware: self.hardware,
            // v0 encodes H.264 (the universal fallback); HEVC/AV1 are negotiated later.
            encode: vec![VideoCodec::H264],
            low_latency: true,
            // GA102 has a single NVENC block — a scarce, first-class admission resource
            // (ADR-0007). Software encode is bounded by CPU, not a fixed engine count.
            max_sessions: if self.hardware { Some(1) } else { None },
        }
    }
}

/// The write side of an [`Encoder`] — a producer thread pushes BGRA frames here.
pub struct FrameSink {
    stdin: ChildStdin,
}

impl FrameSink {
    /// Push one tightly-packed BGRA frame (`width*height*4` bytes).
    pub fn submit_bgra(&mut self, bgra: &[u8]) -> io::Result<()> {
        self.stdin.write_all(bgra)
    }
}

// ------------------------------------- Hub ------------------------------------------

struct Client {
    tx: Sender<Vec<u8>>,
}

/// Client list + the last keyframe, under **one** lock so that priming a joining client
/// and broadcasting a keyframe are mutually atomic (below).
#[derive(Default)]
struct HubState {
    clients: Vec<Client>,
    last_keyframe: Option<Vec<u8>>,
}

/// Fan-out of encoded frames to all connected WebSocket clients. A newly-connected
/// client is primed with the most recent keyframe so its decoder can start immediately.
///
/// `clients` and `last_keyframe` live under a **single** mutex: a race between them
/// would let a client be primed with keyframe K1, then miss K2 (stored + sent to the
/// already-listed clients only) yet be inserted just after — decoding P-frames against a
/// reference it never received until the next IDR. One lock makes a joining client land
/// strictly before or strictly after any given keyframe broadcast.
pub struct Hub {
    state: Mutex<HubState>,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Hub {
            state: Mutex::new(HubState::default()),
        })
    }

    /// Broadcast one already-framed message to every client (dropping dead ones). If
    /// this AU is a keyframe, cache it for priming future clients — atomically with the
    /// send, so no joining client can straddle the store/send boundary.
    pub fn broadcast(&self, msg: Vec<u8>, keyframe: bool) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if keyframe {
            st.last_keyframe = Some(msg.clone());
        }
        // Sends are non-blocking (unbounded std mpsc), so holding the lock across them
        // adds no stall while keeping prime+insert vs. store+send serialized.
        st.clients.retain(|c| c.tx.send(msg.clone()).is_ok());
    }

    /// Forget the cached keyframe (call when the encoder is re-created for a new
    /// resolution, so a newly-connecting client isn't primed with a stale-size frame).
    pub fn reset_keyframe(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).last_keyframe = None;
    }

    pub fn client_count(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clients.len()
    }

    /// Register a new WebSocket client, priming it with the last keyframe, and spawn its
    /// send thread. Runs the tungstenite handshake on `stream`.
    fn register(self: &Arc<Self>, stream: TcpStream) {
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        let mut ws = match tungstenite::accept(stream) {
            Ok(ws) => ws,
            Err(e) => {
                log::warn!("ws handshake failed for {peer}: {e}");
                return;
            }
        };
        let (tx, rx) = channel::<Vec<u8>>();
        // Prime + insert under the one lock, so this client is atomic w.r.t. broadcast:
        // it is primed with whatever keyframe is current AND joins the client list before
        // the next lock release — never primed with K1 but inserted after K2 was sent.
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(k) = st.last_keyframe.clone() {
                let _ = tx.send(k);
            }
            st.clients.push(Client { tx });
        }
        log::info!("infiniPixel client connected: {peer}");
        thread::spawn(move || {
            for msg in rx {
                if ws.send(tungstenite::Message::Binary(msg)).is_err() {
                    break;
                }
            }
            let _ = ws.close(None);
            log::info!("infiniPixel client disconnected: {peer}");
        });
    }

    /// Accept clients into this hub forever from an already-bound listener. Split from
    /// the bind so callers can surface `EADDRINUSE` synchronously (see [`Hub::serve`]).
    fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming().flatten() {
            let hub = Arc::clone(&self);
            thread::spawn(move || hub.register(stream));
        }
    }

    /// Bind a WebSocket server on `addr` and accept clients into this hub forever. The
    /// bind is synchronous, so a port conflict is returned to the caller rather than
    /// swallowed on a background thread.
    pub fn serve(self: Arc<Self>, addr: &str) -> io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        log::info!("infiniPixel WebSocket server on ws://{addr}");
        self.accept_loop(listener);
        Ok(())
    }
}

// ------------------------------- Damage / idle-skip ---------------------------------

/// Fast FNV-1a-style hash over 64-bit words — used to detect an unchanged framebuffer.
fn frame_hash(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let chunks = b.chunks_exact(8);
    let rem = chunks.remainder();
    for c in chunks {
        let w = u64::from_le_bytes(c.try_into().unwrap());
        h = (h ^ w).wrapping_mul(0x0000_0100_0000_01b3);
    }
    for &x in rem {
        h = (h ^ x as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Idle-skip: reports whether a frame differs from the last one it saw. An idle desktop
/// presents identical framebuffers → they hash equal → we don't re-encode or send them,
/// so **idle ⇒ ~0 bits and ~0 encode** (ADR-0009's common-case density win). A
/// full-frame hash is the v0 proxy for the guest damage map.
#[derive(Default)]
struct Deduper {
    last: Option<u64>,
}

impl Deduper {
    /// True if `bgra` differs from the previous frame (and records it).
    fn changed(&mut self, bgra: &[u8]) -> bool {
        let h = frame_hash(bgra);
        if self.last == Some(h) {
            false
        } else {
            self.last = Some(h);
            true
        }
    }
}

// ------------------------------- Latest-frame mailbox -------------------------------

/// A single-slot, latest-frame-wins hand-off from the (frame-producing) caller thread to
/// the encoder feeder thread. `put` never blocks and coalesces: if a frame is already
/// pending, it is dropped in favour of the newer one. This is what keeps a slow or
/// wedged ffmpeg from applying back-pressure up the call chain — critical because the
/// device's `submit_bgra` runs on the vfio-user callback thread, where a blocking write
/// would freeze the guest vCPU (verify finding). Display streaming is inherently lossy,
/// so coalescing intermediate frames under a slow encoder is the correct behavior.
struct Mailbox {
    slot: Mutex<MailboxSlot>,
    cv: Condvar,
}

struct MailboxSlot {
    frame: Option<Vec<u8>>,
    closed: bool,
}

impl Mailbox {
    fn new() -> Arc<Self> {
        Arc::new(Mailbox {
            slot: Mutex::new(MailboxSlot {
                frame: None,
                closed: false,
            }),
            cv: Condvar::new(),
        })
    }

    /// Producer: publish the newest frame (dropping any still-pending one). Non-blocking.
    fn put(&self, frame: Vec<u8>) {
        let mut s = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if s.closed {
            return;
        }
        s.frame = Some(frame);
        self.cv.notify_one();
    }

    /// Consumer (feeder): block until a frame is available; `None` once closed & drained.
    fn take(&self) -> Option<Vec<u8>> {
        let mut s = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(f) = s.frame.take() {
                return Some(f);
            }
            if s.closed {
                return None;
            }
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Signal the feeder to stop; it wakes, sees no frame, and exits.
    fn close(&self) {
        let mut s = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        s.closed = true;
        s.frame = None;
        self.cv.notify_all();
    }
}

// ------------------------------- Encoder session ------------------------------------

/// One live ffmpeg encoder for a fixed `w`×`h`, with its two helper threads:
/// - a **feeder** that owns the [`FrameSink`] and does the blocking `write_all`, fed by
///   a latest-wins [`Mailbox`] so the producer never blocks;
/// - a **drain** that reads encoded AUs off the [`Encoder`] receiver and broadcasts them.
///
/// The [`Child`] handle stays here (not moved into a thread), so [`EncoderSession::shutdown`]
/// can `kill()` a wedged ffmpeg — otherwise the drain thread would park forever in `recv()`
/// and neither thread nor process would ever be reaped (verify findings).
struct EncoderSession {
    w: u32,
    h: u32,
    hardware: bool,
    mailbox: Arc<Mailbox>,
    child: Option<Child>,
    /// Cleared when either helper thread ends (write error, or ffmpeg exit) — i.e. the
    /// encoder is dead and must be re-created before it can accept more frames.
    alive: Arc<AtomicBool>,
    /// Count of AUs the drain thread actually broadcast — used to detect a hardware
    /// encoder that spawned but produced nothing (→ fall back to software).
    produced: Arc<AtomicU64>,
    feeder: Option<thread::JoinHandle<()>>,
    drain: Option<thread::JoinHandle<()>>,
}

impl EncoderSession {
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Publish a frame to the encoder without blocking the caller (latest-wins).
    fn submit(&self, bgra: &[u8]) {
        self.mailbox.put(bgra.to_vec());
    }

    /// Kill ffmpeg and join both helper threads. After this returns, the drain thread has
    /// stopped, so it can no longer broadcast a stale-size frame onto the shared hub.
    /// Returns whether this session ever produced output.
    fn shutdown(mut self) -> bool {
        self.mailbox.close(); // feeder wakes, drops the sink (closes ffmpeg stdin)
        if let Some(mut child) = self.child.take() {
            let _ = child.kill(); // guarantee a wedged ffmpeg dies → stdout EOF
            let _ = child.wait(); // reap
        }
        if let Some(f) = self.feeder.take() {
            let _ = f.join();
        }
        if let Some(d) = self.drain.take() {
            let _ = d.join();
        }
        self.produced.load(Ordering::Acquire) > 0
    }
}

// ---------------------------------- Streamer ----------------------------------------

/// One-call infiniPixel stream: a persistent WebSocket [`Hub`] plus an [`EncoderSession`]
/// that is created (and **re-created on a resolution change or encoder death**) to match
/// the frames pushed to it. Push BGRA frames with [`PixelStreamer::submit_bgra`];
/// connected browsers decode them live. The server binds the port once, so a resize
/// doesn't drop clients.
pub struct PixelStreamer {
    hub: Arc<Hub>,
    fps: u32,
    bitrate_kbps: u32,
    /// The current encoder session (sized to the last frame), if any.
    enc: Option<EncoderSession>,
    /// Latched once a hardware NVENC encoder is shown not to work on this host, so every
    /// subsequent encoder is spawned as software x264 instead of failing the same way.
    hardware_failed: bool,
    /// Idle-skip: drop frames identical to the previous one.
    dedup: Deduper,
    /// The last *changed* frame is still buffered inside ffmpeg's AU splitter (it needs
    /// the next frame's delimiter to emit). When true, the next idle frame is fed once to
    /// flush it — so a change followed by stillness still reaches the client.
    pending_flush: bool,
    sent: u64,
    skipped: u64,
}

impl PixelStreamer {
    /// Bind the WebSocket server on `0.0.0.0:port` (synchronously — a port conflict is
    /// returned, not swallowed). The encoder is created lazily on the first
    /// [`submit_bgra`](PixelStreamer::submit_bgra), sized to that frame.
    pub fn new(fps: u32, bitrate_kbps: u32, port: u16) -> io::Result<Self> {
        let hub = Hub::new();
        // Bind here (not on the spawned thread) so EADDRINUSE surfaces to the caller and
        // "serving on ws://…" is only logged on a genuinely bound socket.
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        log::info!("infiniPixel WebSocket server on ws://0.0.0.0:{port}");
        {
            let hub = Arc::clone(&hub);
            thread::spawn(move || hub.accept_loop(listener));
        }
        Ok(PixelStreamer {
            hub,
            fps,
            bitrate_kbps,
            enc: None,
            hardware_failed: false,
            dedup: Deduper::default(),
            pending_flush: false,
            sent: 0,
            skipped: 0,
        })
    }

    /// Convenience for a fixed-size producer: bind + prime the encoder for `w`×`h`.
    pub fn start(w: u32, h: u32, fps: u32, bitrate_kbps: u32, port: u16) -> io::Result<Self> {
        let mut s = Self::new(fps, bitrate_kbps, port)?;
        s.ensure_encoder(w, h)?;
        Ok(s)
    }

    /// Spawn a fresh encoder session for `w`×`h`, wiring its feeder (mailbox → ffmpeg
    /// stdin) and drain (ffmpeg AUs → hub) threads. Does not touch `self.enc`.
    fn spawn_session(&self, w: u32, h: u32, hardware: bool) -> io::Result<EncoderSession> {
        let cfg = EncoderConfig {
            width: w,
            height: h,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            prefer_hardware: hardware,
        };
        let mut enc = Encoder::spawn(&cfg)?;
        let sink = enc
            .take_sink()
            .ok_or_else(|| io::Error::other("encoder sink missing"))?;
        let rx = enc
            .take_rx()
            .ok_or_else(|| io::Error::other("encoder rx missing"))?;
        let child = enc.take_child();
        let codec = enc.codec().wire();
        // `enc` (an empty shell now) drops here — its Drop is a no-op since child/rx/stdin
        // were all taken.

        let mailbox = Mailbox::new();
        let alive = Arc::new(AtomicBool::new(true));
        let produced = Arc::new(AtomicU64::new(0));

        // Feeder: own the sink; blocking-write the newest frame; die (marking !alive) if
        // ffmpeg's pipe breaks. This blocking write is off the caller's thread by design.
        let feeder = {
            let mailbox = Arc::clone(&mailbox);
            let alive = Arc::clone(&alive);
            let mut sink = sink;
            thread::spawn(move || {
                while let Some(frame) = mailbox.take() {
                    if sink.submit_bgra(&frame).is_err() {
                        alive.store(false, Ordering::Release);
                        break;
                    }
                }
                // dropping `sink` closes ffmpeg's stdin
            })
        };

        // Drain: broadcast encoded AUs to the persistent hub; mark !alive on ffmpeg exit.
        let drain = {
            let hub = Arc::clone(&self.hub);
            let alive = Arc::clone(&alive);
            let produced = Arc::clone(&produced);
            let us_per_frame = 1_000_000u64 / self.fps.max(1) as u64;
            thread::spawn(move || {
                while let Ok(au) = rx.recv() {
                    produced.fetch_add(1, Ordering::Release);
                    let flags = if au.keyframe { proto::flags::KEYFRAME } else { 0 };
                    let hdr = FrameHeader {
                        flags,
                        codec,
                        frame_seq: au.seq as u32,
                        width: w as u16,
                        height: h as u16,
                        pts_us: au.seq * us_per_frame,
                        payload_len: au.data.len() as u32,
                    };
                    hub.broadcast(hdr.message(&au.data), au.keyframe);
                }
                alive.store(false, Ordering::Release);
            })
        };

        Ok(EncoderSession {
            w,
            h,
            hardware,
            mailbox,
            child,
            alive,
            produced,
            feeder: Some(feeder),
            drain: Some(drain),
        })
    }

    fn ensure_encoder(&mut self, w: u32, h: u32) -> io::Result<()> {
        // Reuse only a live session of the right size.
        if let Some(sess) = self.enc.as_ref() {
            if sess.w == w && sess.h == h && sess.is_alive() {
                return Ok(());
            }
        }
        // Tear down the old session FIRST — kill ffmpeg and *join its drain thread* — so
        // it can never broadcast a trailing stale-size frame after we reset the keyframe.
        if let Some(old) = self.enc.take() {
            let was_hardware = old.hardware;
            // Distinguish "the encoder died on its own" (NVENC broken) from "we tore down
            // a healthy encoder for a resize": only the former should latch software.
            let died = !old.is_alive();
            let produced_output = old.shutdown();
            // A hardware encoder that died without ever producing output means NVENC is
            // unusable here (no engine, session cap hit, driver mismatch): latch software.
            if was_hardware && died && !produced_output && !self.hardware_failed {
                log::warn!(
                    "infiniPixel: hardware NVENC produced no output — falling back to software x264"
                );
                self.hardware_failed = true;
            }
        }
        self.hub.reset_keyframe();

        // Prefer hardware unless we've already learned it fails here; on a spawn error
        // with hardware, drop to software once.
        let hardware = !self.hardware_failed;
        let session = match self.spawn_session(w, h, hardware) {
            Ok(s) => s,
            Err(e) if hardware => {
                log::warn!(
                    "infiniPixel: hardware encoder spawn failed ({e}); falling back to software"
                );
                self.hardware_failed = true;
                self.spawn_session(w, h, false)?
            }
            Err(e) => return Err(e),
        };
        self.enc = Some(session);
        // A brand-new encoder needs its startup keyframe: force the next frame through
        // even if it is byte-identical to the pre-respawn one.
        self.dedup = Deduper::default();
        self.pending_flush = false;
        Ok(())
    }

    /// Submit one tightly-packed `w`×`h` BGRA frame; (re)creates the encoder if the size
    /// changed or the previous one died. An unchanged frame is **skipped** (idle-skip) —
    /// no encode, no bytes — so a static desktop costs ~0, except that the first idle
    /// frame after a change is fed once to flush the last visible frame to clients.
    pub fn submit_bgra(&mut self, bgra: &[u8], w: u32, h: u32) -> io::Result<()> {
        if !self.dedup.changed(bgra) {
            self.skipped += 1;
            if self.pending_flush {
                // Feed this identical frame once so ffmpeg emits the delimiter that
                // flushes the last *changed* AU out to clients; then go quiet.
                self.pending_flush = false;
                self.ensure_encoder(w, h)?;
                self.push(bgra);
            }
            return Ok(());
        }
        self.ensure_encoder(w, h)?;
        self.pending_flush = true;
        self.push(bgra);
        Ok(())
    }

    /// Hand a frame to the current session (non-blocking) and count it.
    fn push(&mut self, bgra: &[u8]) {
        if let Some(sess) = self.enc.as_ref() {
            self.sent += 1;
            sess.submit(bgra);
        }
    }

    /// `(frames encoded, frames skipped as unchanged)`.
    pub fn stats(&self) -> (u64, u64) {
        (self.sent, self.skipped)
    }

    pub fn client_count(&self) -> usize {
        self.hub.client_count()
    }
}

// ------------------------------- Test frame source ----------------------------------

/// A synthetic animated BGRA source (moving bars + a bouncing box + a pulsing
/// background) — enough motion to prove the stream is live and decoding in order.
pub struct TestPattern {
    pub width: u32,
    pub height: u32,
    frame: u32,
    buf: Vec<u8>,
}

impl TestPattern {
    pub fn new(width: u32, height: u32) -> Self {
        TestPattern {
            width,
            height,
            frame: 0,
            buf: vec![0u8; (width * height * 4) as usize],
        }
    }

    /// Render the next frame; returns the tightly-packed BGRA buffer.
    pub fn next_bgra(&mut self) -> &[u8] {
        let (w, h) = (self.width as usize, self.height as usize);
        let f = self.frame as usize;
        // bouncing box
        let bx = ((f * 7) % (w.saturating_sub(80).max(1))) as i32;
        let by = (((f * 5) / 3) % (h.saturating_sub(80).max(1))) as i32;
        let pulse = (128.0 + 100.0 * ((f as f32) * 0.05).sin()) as u8;
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 4;
                // background: diagonal moving bars + pulse
                let bar = (((x + y + f * 4) / 24) % 2) as u8;
                let (mut b, mut g, mut r) = if bar == 0 {
                    (pulse / 3, (x * 255 / w) as u8, (y * 255 / h) as u8)
                } else {
                    (pulse, 40u8, 80u8)
                };
                // bouncing box (bright cyan)
                let (xi, yi) = (x as i32, y as i32);
                if xi >= bx && xi < bx + 80 && yi >= by && yi < by + 80 {
                    b = 230;
                    g = 230;
                    r = 20;
                }
                self.buf[o] = b;
                self.buf[o + 1] = g;
                self.buf[o + 2] = r;
                self.buf[o + 3] = 255;
            }
        }
        self.frame = self.frame.wrapping_add(1);
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips_little_endian() {
        let h = FrameHeader {
            flags: proto::flags::KEYFRAME,
            codec: proto::codec::H264,
            frame_seq: 0x0102_0304,
            width: 1280,
            height: 720,
            pts_us: 0x0011_2233_4455_6677,
            payload_len: 4096,
        };
        let b = h.to_bytes();
        assert_eq!(b.len(), proto::HEADER_LEN);
        assert_eq!(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), proto::MAGIC);
        assert_eq!(b[4], proto::VERSION);
        assert_eq!(b[5] & proto::flags::KEYFRAME, proto::flags::KEYFRAME);
        assert_eq!(b[6], proto::codec::H264);
        assert_eq!(u32::from_le_bytes([b[8], b[9], b[10], b[11]]), 0x0102_0304);
        assert_eq!(u16::from_le_bytes([b[12], b[13]]), 1280);
        assert_eq!(u16::from_le_bytes([b[14], b[15]]), 720);
        assert_eq!(
            u64::from_le_bytes([b[16], b[17], b[18], b[19], b[20], b[21], b[22], b[23]]),
            0x0011_2233_4455_6677
        );
        assert_eq!(u32::from_le_bytes([b[24], b[25], b[26], b[27]]), 4096);
    }

    #[test]
    fn au_splitter_separates_access_units_on_aud() {
        // Two fake AUs, each = AUD (00000109) + a slice NAL; the second carries an SPS.
        let aud = [0u8, 0, 0, 1, 9, 0x10];
        let slice = [0u8, 0, 0, 1, 0x61, 0xAA, 0xBB]; // non-IDR slice (type 1)
        let sps = [0u8, 0, 0, 1, 0x67, 0x42]; // SPS (type 7) → keyframe
        let mut stream = Vec::new();
        stream.extend_from_slice(&aud);
        stream.extend_from_slice(&slice);
        stream.extend_from_slice(&aud);
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&slice);
        // trailing AUD so the 2nd AU is terminated
        stream.extend_from_slice(&aud);

        let mut splitter = AuSplitter::default();
        let mut aus: Vec<Vec<u8>> = Vec::new();
        splitter.push(&stream, |au| aus.push(au));

        assert_eq!(aus.len(), 2, "expected two complete access units");
        assert!(!au_is_keyframe(&aus[0]), "first AU is a plain slice");
        assert!(au_is_keyframe(&aus[1]), "second AU carries an SPS → keyframe");
    }

    #[test]
    fn idle_skip_drops_unchanged_frames_only() {
        let mut d = Deduper::default();
        let a = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9];
        let b = vec![1u8, 2, 3, 4, 5, 6, 7, 42, 9]; // one byte different
        assert!(d.changed(&a), "first frame is always 'changed'");
        assert!(!d.changed(&a), "identical frame is skipped");
        assert!(!d.changed(&a), "still skipped");
        assert!(d.changed(&b), "a different frame re-encodes");
        assert!(!d.changed(&b), "then skipped again");
        assert!(d.changed(&a), "back to a is a change");
    }

    #[test]
    fn au_splitter_flush_emits_final_held_au() {
        // Two AUs but NO trailing AUD: push emits only the first (the 2nd is held back
        // waiting for a delimiter). flush() must then emit the held final AU, or the last
        // frame before EOF/resize would be silently dropped (verify finding).
        let aud = [0u8, 0, 0, 1, 9, 0x10];
        let slice = [0u8, 0, 0, 1, 0x61, 0xAA, 0xBB];
        let idr = [0u8, 0, 0, 1, 0x65, 0xCC, 0xDD]; // IDR (type 5) → keyframe
        let mut stream = Vec::new();
        stream.extend_from_slice(&aud);
        stream.extend_from_slice(&slice);
        stream.extend_from_slice(&aud);
        stream.extend_from_slice(&idr);

        let mut splitter = AuSplitter::default();
        let mut aus: Vec<Vec<u8>> = Vec::new();
        splitter.push(&stream, |au| aus.push(au));
        assert_eq!(aus.len(), 1, "push emits only the delimited first AU");

        splitter.flush(|au| aus.push(au));
        assert_eq!(aus.len(), 2, "flush emits the held final AU");
        assert!(au_is_keyframe(&aus[1]), "the flushed final AU is the IDR");
    }

    #[test]
    fn au_splitter_flush_ignores_bare_delimiter() {
        // A lone AUD (no following NAL) is not a real AU — flush must not emit it.
        let mut splitter = AuSplitter::default();
        let mut aus: Vec<Vec<u8>> = Vec::new();
        splitter.push(&[0u8, 0, 0, 1, 9, 0x10], |au| aus.push(au));
        splitter.flush(|au| aus.push(au));
        assert!(aus.is_empty(), "a bare AUD must not be emitted as an AU");
    }

    #[test]
    fn mailbox_coalesces_to_latest_frame() {
        // put never blocks and keeps only the newest frame; take drains it. This is what
        // keeps a slow encoder from stalling the caller.
        let mb = Mailbox::new();
        mb.put(vec![1]);
        mb.put(vec![2]);
        mb.put(vec![3]); // older pending frames dropped in favour of the newest
        assert_eq!(mb.take(), Some(vec![3]), "take yields the latest put");
        // close() is teardown: it discards any pending frame and unblocks the feeder,
        // which then drops its sink — we're about to kill ffmpeg, so a last frame is moot.
        mb.put(vec![4]);
        mb.close();
        assert_eq!(mb.take(), None, "close discards the pending frame and returns None");
    }

    #[test]
    fn au_splitter_handles_split_across_reads() {
        let aud = [0u8, 0, 0, 1, 9, 0x10];
        let slice = [0u8, 0, 0, 1, 0x65, 0xAA]; // IDR slice (type 5)
        let mut full = Vec::new();
        full.extend_from_slice(&aud);
        full.extend_from_slice(&slice);
        full.extend_from_slice(&aud); // terminator

        let mut splitter = AuSplitter::default();
        let mut aus: Vec<Vec<u8>> = Vec::new();
        // Feed one byte at a time to stress the incremental scanner.
        for b in &full {
            splitter.push(&[*b], |au| aus.push(au));
        }
        assert_eq!(aus.len(), 1);
        assert!(au_is_keyframe(&aus[0]));
    }
}
