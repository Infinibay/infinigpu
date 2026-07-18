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

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
        /// Reserved for AV1. Two blockers before it's wired: (1) AV1 is OBU-framed (no
        /// Annex-B start codes / AUDs), so it needs an OBU splitter — temporal-delimiter
        /// OBU (type 2) splits temporal units, SEQ_HDR OBU (type 1) marks keyframes —
        /// not the [`super::super::AuSplitter`]; (2) NVENC AV1 encode needs an **Ada+**
        /// GPU — Ampere (e.g. the RTX A5000) is AV1 *decode*-only, so it can't be
        /// hardware-validated on this project's GPU. HEVC covers the compression need.
        pub const AV1: u8 = 3;
    }
    pub mod flags {
        /// This access unit is a keyframe (contains SPS/PPS + IDR) — a client may
        /// start decoding here.
        pub const KEYFRAME: u8 = 1 << 0;
    }

    /// The **plane sideband** protocol (see `docs/adr/CLIENT-PLANE-COMPOSITOR.md`). A typed
    /// op-family multiplexed onto the same WebSocket as video frames. A message whose first four
    /// bytes are [`plane::MAGIC`] (`"XIPL"`, distinct from the video [`MAGIC`] `"XIPI"`) is routed
    /// to the plane handler; an un-updated client's [`super::FrameHeader::parse`] returns `None` on
    /// the mismatched magic and safely drops it — sideband bytes never reach the video decoder. The
    /// cursor is the first [`plane::kind`]; a future media region reuses the same header verbatim.
    pub mod plane {
        /// Plane-sideband header magic, read little-endian (`"XIPL"` bytes).
        pub const MAGIC: u32 = 0x4C50_4958;
        /// Header size in bytes; the op body (ARGB sprite / bitstream chunk) follows immediately.
        pub const HEADER_LEN: usize = 36;
        pub const VERSION: u8 = 1;

        /// Plane op ([`super::super::PlaneHeader::op`]).
        pub mod op {
            /// Shape + dims + body (a cursor sprite / a media keyframe region).
            pub const DEFINE: u8 = 1;
            /// Header only — a fresh server position and/or visibility change.
            pub const MOVE: u8 = 2;
            /// A media bitstream chunk (the `VIDEO` plane kind).
            pub const DATA: u8 = 3;
            /// Header only — tear the plane down.
            pub const DESTROY: u8 = 4;
        }
        /// Plane kind ([`super::super::PlaneHeader::plane_kind`]).
        pub mod kind {
            pub const CURSOR: u8 = 1;
            pub const VIDEO: u8 = 2;
        }
        /// Sideband flag bits — a **repacked subset** of `infinigpu_abi::wire::cursor_flags`
        /// (the `op` field already carries move-vs-define, and the device resolves the shape before
        /// forwarding, so `MOVE_ONLY`/`SHAPE_BY_RESID` are not on the wire here).
        pub mod flags {
            pub const VISIBLE: u8 = 1 << 0;
            pub const PREMULTIPLIED: u8 = 1 << 1;
            pub const WARP: u8 = 1 << 2;
            pub const RELATIVE: u8 = 1 << 3;
        }
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

    /// The NAL unit type of this codec's access-unit delimiter (what the splitter cuts
    /// on). H.264 AUD = 9; HEVC AUD = 35.
    fn aud_type(self) -> u8 {
        match self {
            Codec::H264 => 9,
            Codec::Hevc => 35,
        }
    }

    /// NAL unit type from the byte after a `00 00 01` start code. H.264 has a 1-byte NAL
    /// header (`type = b & 0x1F`); HEVC a 2-byte header (`type = (b >> 1) & 0x3F`).
    fn nal_type(self, byte_after_startcode: u8) -> u8 {
        match self {
            Codec::H264 => byte_after_startcode & 0x1F,
            Codec::Hevc => (byte_after_startcode >> 1) & 0x3F,
        }
    }

    /// Whether a NAL unit type marks a keyframe / decodable stream-start for this codec.
    /// H.264: IDR(5) or SPS(7). HEVC: IDR_W_RADL(19)/IDR_N_LP(20)/CRA(21) or
    /// VPS(32)/SPS(33)/PPS(34).
    fn is_keyframe_nal(self, t: u8) -> bool {
        match self {
            Codec::H264 => t == 5 || t == 7,
            Codec::Hevc => matches!(t, 19 | 20 | 21 | 32 | 33 | 34),
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

    /// Parse a [`FrameHeader`] from the first [`proto::HEADER_LEN`] bytes of a wire
    /// message (the access unit follows). The **client** side of the same contract
    /// [`to_bytes`](Self::to_bytes) writes — kept here so both ends share one source of
    /// truth. Returns `None` if the buffer is too short or the magic doesn't match.
    pub fn parse(buf: &[u8]) -> Option<FrameHeader> {
        if buf.len() < proto::HEADER_LEN {
            return None;
        }
        let u32le = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
        let u16le = |o: usize| u16::from_le_bytes([buf[o], buf[o + 1]]);
        if u32le(0) != proto::MAGIC {
            return None;
        }
        Some(FrameHeader {
            flags: buf[5],
            codec: buf[6],
            frame_seq: u32le(8),
            width: u16le(12),
            height: u16le(14),
            pts_us: u64::from_le_bytes([
                buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
            ]),
            payload_len: u32le(24),
        })
    }

    /// Whether this access unit is a keyframe (SPS/PPS + IDR) — a client may start
    /// decoding here.
    pub fn is_keyframe(&self) -> bool {
        self.flags & proto::flags::KEYFRAME != 0
    }
}

/// The plane-sideband header (see [`proto::plane`]). Little-endian, 36 bytes, mirrored in the JS
/// client; the op body (a cursor's ARGB sprite on `DEFINE`, a media chunk on `DATA`, nothing on
/// `MOVE`/`DESTROY`) follows immediately.
///
/// ```text
///  off size field
///   0   4   magic ("XIPL", LE u32)
///   4   1   version
///   5   1   op          (DEFINE=1 | MOVE=2 | DATA=3 | DESTROY=4)
///   6   1   plane_kind  (CURSOR=1 | VIDEO=2)
///   7   1   flags       (bit0 VISIBLE | bit1 PREMULTIPLIED | bit2 WARP | bit3 RELATIVE)
///   8   4   plane_id (LE u32; 0 reserved = cursor)
///  12   1   codec_or_format (cursor: pixel format; video: codec id)
///  13   1   z_order
///  14   2   (pad)
///  16   2   width  (LE u16) — shape/region dims on DEFINE, else 0
///  18   2   height (LE u16)
///  20   2   hot_x  (LE u16) — cursor hotspot
///  22   2   hot_y  (LE u16)
///  24   4   pos_x  (LE i32) — authoritative server position, guest space
///  28   4   pos_y  (LE i32)
///  32   4   payload_len (LE u32)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneHeader {
    pub op: u8,
    pub plane_kind: u8,
    pub flags: u8,
    pub plane_id: u32,
    pub codec_or_format: u8,
    pub z_order: u8,
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub pos_x: i32,
    pub pos_y: i32,
    pub payload_len: u32,
}

impl PlaneHeader {
    /// Serialize into a fresh 36-byte buffer.
    pub fn to_bytes(&self) -> [u8; proto::plane::HEADER_LEN] {
        let mut b = [0u8; proto::plane::HEADER_LEN];
        b[0..4].copy_from_slice(&proto::plane::MAGIC.to_le_bytes());
        b[4] = proto::plane::VERSION;
        b[5] = self.op;
        b[6] = self.plane_kind;
        b[7] = self.flags;
        b[8..12].copy_from_slice(&self.plane_id.to_le_bytes());
        b[12] = self.codec_or_format;
        b[13] = self.z_order;
        b[16..18].copy_from_slice(&self.width.to_le_bytes());
        b[18..20].copy_from_slice(&self.height.to_le_bytes());
        b[20..22].copy_from_slice(&self.hot_x.to_le_bytes());
        b[22..24].copy_from_slice(&self.hot_y.to_le_bytes());
        b[24..28].copy_from_slice(&self.pos_x.to_le_bytes());
        b[28..32].copy_from_slice(&self.pos_y.to_le_bytes());
        b[32..36].copy_from_slice(&self.payload_len.to_le_bytes());
        b
    }

    /// Build a full wire message: header followed by the op body (empty for `MOVE`/`DESTROY`).
    pub fn message(&self, body: &[u8]) -> Vec<u8> {
        let mut m = Vec::with_capacity(proto::plane::HEADER_LEN + body.len());
        m.extend_from_slice(&self.to_bytes());
        m.extend_from_slice(body);
        m
    }

    /// Parse from the first [`proto::plane::HEADER_LEN`] bytes of a wire message — the client half
    /// of the same contract [`to_bytes`](Self::to_bytes) writes. Returns `None` if the buffer is too
    /// short or the magic isn't [`proto::plane::MAGIC`] (so a video frame is never misread as a plane).
    pub fn parse(buf: &[u8]) -> Option<PlaneHeader> {
        if buf.len() < proto::plane::HEADER_LEN {
            return None;
        }
        let u32le = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
        let i32le = |o: usize| i32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
        let u16le = |o: usize| u16::from_le_bytes([buf[o], buf[o + 1]]);
        if u32le(0) != proto::plane::MAGIC {
            return None;
        }
        Some(PlaneHeader {
            op: buf[5],
            plane_kind: buf[6],
            flags: buf[7],
            plane_id: u32le(8),
            codec_or_format: buf[12],
            z_order: buf[13],
            width: u16le(16),
            height: u16le(18),
            hot_x: u16le(20),
            hot_y: u16le(22),
            pos_x: i32le(24),
            pos_y: i32le(28),
            payload_len: u32le(32),
        })
    }
}

// ------------------------------- Annex-B AU splitting -------------------------------

/// Find the next access-unit delimiter (start code + this codec's AUD NAL) at or after
/// `from`, returning the index of its `00 00 01`.
fn find_aud(buf: &[u8], from: usize, codec: Codec) -> Option<usize> {
    let aud = codec.aud_type();
    let mut i = from;
    while i + 3 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 && codec.nal_type(buf[i + 3]) == aud {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// True if the access unit contains a keyframe NAL for `codec` (H.264 IDR/SPS; HEVC
/// IDR/CRA or VPS/SPS/PPS).
fn au_is_keyframe(au: &[u8], codec: Codec) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if codec.is_keyframe_nal(codec.nal_type(au[i + 3])) {
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
/// the `{h264,hevc}_metadata=aud=insert` bitstream filter) into complete access units.
struct AuSplitter {
    buf: Vec<u8>,
    codec: Codec,
}

impl AuSplitter {
    fn new(codec: Codec) -> Self {
        AuSplitter { buf: Vec::new(), codec }
    }

    fn push(&mut self, incoming: &[u8], mut emit: impl FnMut(Vec<u8>)) {
        self.buf.extend_from_slice(incoming);
        // Drop any leading bytes before the first AUD.
        match find_aud(&self.buf, 0, self.codec) {
            Some(0) => {}
            Some(first) => {
                self.buf.drain(0..first);
            }
            None => return,
        }
        // buf[0] is now an AUD. Emit [AUD_n .. AUD_{n+1}) for every complete AU.
        while let Some(next) = find_aud(&self.buf, 4, self.codec) {
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
        if self.buf.len() > 6 && find_aud(&self.buf, 0, self.codec) == Some(0) {
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
    /// Prefer the NVENC hardware encoder; fall back to the software encoder if it fails.
    pub prefer_hardware: bool,
    /// H.264 (universal fallback) or HEVC (better compression, `hevc_nvenc`).
    pub codec: Codec,
    /// Use NVENC **Periodic Intra Refresh** (a rolling refresh wave) instead of periodic
    /// IDR frames — smoother bitrate, no IDR spikes (ADR-0009 v1). Note: with intra-refresh
    /// there are no mid-stream IDRs, so a late-joining keyframe-gated client can only
    /// resync after a full refresh cycle; keep it **off** (the default) when clients join
    /// mid-stream, on for a single long-lived viewer. Hardware only.
    pub intra_refresh: bool,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        EncoderConfig {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 8000,
            prefer_hardware: true,
            codec: Codec::H264,
            intra_refresh: false,
        }
    }
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
        let codec = cfg.codec;
        thread::spawn(move || Self::reader_loop(stdout, tx, codec));

        Ok(Encoder {
            child: Some(child),
            stdin,
            rx: Some(rx),
            codec,
            hardware,
        })
    }

    fn ffmpeg_args(cfg: &EncoderConfig, hardware: bool) -> Vec<String> {
        let gop = (cfg.fps * 2).max(2); // periodic IDR every ~2s (unless intra-refresh)
        // Per-codec: NVENC encoder, software fallback, the AUD-insert metadata bsf, the
        // software params key, and the raw-bitstream muxer.
        let (nvenc, sw_enc, meta_bsf, sw_params_key, muxer) = match cfg.codec {
            Codec::H264 => (
                "h264_nvenc",
                "libx264",
                "h264_metadata=aud=insert",
                "-x264-params",
                "h264",
            ),
            Codec::Hevc => (
                "hevc_nvenc",
                "libx265",
                "hevc_metadata=aud=insert",
                "-x265-params",
                "hevc",
            ),
        };
        let bitrate = format!("{}k", cfg.bitrate_kbps);
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
            let mut hw: Vec<String> = [
                "-c:v", nvenc, "-preset", "p1", "-tune", "ull",
                "-rc", "cbr", "-b:v", &bitrate, "-bf", "0", "-delay", "0",
            ]
            .into_iter()
            .map(String::from)
            .collect();
            if cfg.intra_refresh {
                // Rolling refresh instead of IDRs: no mid-stream keyframes, smoother rate.
                hw.extend(["-intra-refresh", "1", "-g", &gop.to_string()].map(String::from));
            } else {
                hw.extend(["-g", &gop.to_string(), "-forced-idr", "1"].map(String::from));
            }
            a.extend(hw);
        } else {
            a.extend(
                [
                    "-c:v", sw_enc, "-preset", "ultrafast", "-tune", "zerolatency",
                    sw_params_key, "bframes=0:scenecut=0",
                    "-b:v", &bitrate, "-g", &gop.to_string(),
                ]
                .into_iter()
                .map(String::from),
            );
        }
        // AUD before each AU so the reader can split cleanly; raw Annex-B on stdout.
        a.extend(
            ["-bsf:v", meta_bsf, "-f", muxer, "-"]
                .into_iter()
                .map(String::from),
        );
        a
    }

    fn reader_loop(mut stdout: std::process::ChildStdout, tx: Sender<EncodedAu>, codec: Codec) {
        let mut splitter = AuSplitter::new(codec);
        let mut seq: u64 = 0;
        let mut chunk = [0u8; 64 * 1024];
        // `Fn` (captures nothing) so both the streaming loop and the final flush can
        // reuse it while each takes its own mutable borrow of `seq`.
        let send_au = |au: Vec<u8>, seq: &mut u64, tx: &Sender<EncodedAu>| {
            let keyframe = au_is_keyframe(&au, codec);
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
            // The codec this encoder is actually configured for (H.264 universal fallback,
            // or HEVC). AV1 is not yet wired (OBU framing).
            encode: vec![match self.codec {
                Codec::H264 => VideoCodec::H264,
                Codec::Hevc => VideoCodec::Hevc,
            }],
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

/// Max frames buffered per client before we shed. At 30fps this caps the standing fan-out
/// latency at ~`MAX_CLIENT_QUEUE / fps` seconds; overflow collapses to the next keyframe.
/// Override with `INFINIPIXEL_CLIENT_QUEUE`.
fn max_client_queue() -> usize {
    std::env::var("INFINIPIXEL_CLIENT_QUEUE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(8)
}

/// A **bounded, drop-to-keyframe** per-client send queue. Unlike a plain unbounded mpsc
/// (the previous design), a slow viewer cannot accumulate unbounded standing latency: when
/// the backlog exceeds `max_client_queue()` the whole queue is dropped and the client is
/// re-admitted only at the **next keyframe** — because delivering a subset of the queued
/// P-frames would desync openh264 until the next IDR anyway. The producer is simultaneously
/// asked to emit a fresh IDR so the shed client resyncs in ~one frame. This mirrors the
/// latest-wins [`Mailbox`] coalescing on the encoder-feed side, but with the P-frame-correct
/// shed (collapse to a keyframe boundary, never drop an arbitrary mid-GOP P-frame).
struct ClientQueue {
    inner: Mutex<ClientQueueInner>,
    cv: Condvar,
}

struct ClientQueueInner {
    items: VecDeque<Vec<u8>>,
    closed: bool,
    /// After an overflow shed: skip every frame until the next keyframe re-establishes a
    /// contiguous, decodable run (mirrors the viewer's own keyframe-gated resync).
    dropping: bool,
    cap: usize,
}

impl ClientQueue {
    fn new() -> Arc<Self> {
        Arc::new(ClientQueue {
            inner: Mutex::new(ClientQueueInner {
                items: VecDeque::new(),
                closed: false,
                dropping: false,
                cap: max_client_queue(),
            }),
            cv: Condvar::new(),
        })
    }

    /// Producer (broadcast side): enqueue one framed message. Returns `true` if this client
    /// just shed its backlog (overflow). The caller uses that only for observability — the
    /// client resyncs on its own at the next periodic IDR; forcing one would respawn ffmpeg on
    /// the vfio-user callback thread and stall the guest (see `Hub::broadcast`).
    fn push(&self, msg: Vec<u8>, keyframe: bool) -> bool {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.closed {
            return false;
        }
        if q.dropping {
            // Shedding a backlog: drop P-frames, re-admit the stream only at a keyframe so
            // the decoder starts from a self-contained reference.
            if keyframe {
                q.dropping = false;
                q.items.push_back(msg);
                self.cv.notify_one();
            }
            return false;
        }
        q.items.push_back(msg);
        if q.items.len() > q.cap {
            // The viewer is draining slower than the encoder produces. Delivering only some
            // of the queued P-frames would desync openh264 until the next IDR anyway, so
            // collapse to the latest state: drop the whole backlog and wait for a fresh
            // keyframe. Bounds standing latency at `cap` frames.
            q.items.clear();
            q.dropping = true;
            return true; // ask the producer to emit a fresh IDR now
        }
        self.cv.notify_one();
        false
    }

    /// Enqueue a **control** (plane-sideband) message. Independent of the video keyframe chain:
    /// it bypasses the overflow-shed gate (a shed video backlog must never drop the cursor) and is
    /// not counted against the video `cap`. Plane control is low-rate and coalesced upstream (the
    /// device caps MOVE forwarding), so it cannot bloat the queue in practice.
    fn push_control(&self, msg: Vec<u8>) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.closed {
            return;
        }
        q.items.push_back(msg);
        self.cv.notify_one();
    }

    /// Consumer (send thread): block for the next message; `None` once closed and drained.
    fn pop(&self) -> Option<Vec<u8>> {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(m) = q.items.pop_front() {
                return Some(m);
            }
            if q.closed {
                return None;
            }
            q = self.cv.wait(q).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Wake the send thread and make it exit (it drains nothing further).
    fn close(&self) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.closed = true;
        q.items.clear();
        self.cv.notify_all();
    }
}

struct Client {
    q: Arc<ClientQueue>,
    /// Cleared by the send thread when its blocking `ws.send` fails (viewer gone), so the
    /// next broadcast evicts this client — the liveness signal the old mpsc got for free
    /// from a dropped `Receiver`.
    alive: Arc<AtomicBool>,
}

/// Cached last state of one plane (cursor / future media), so a late-joining or reconnecting
/// client is primed with the current cursor shape + position immediately — video self-heals via
/// `last_keyframe`+IDR, but a static cursor otherwise shows no bitmap until its next change, which
/// may never come.
#[derive(Default, Clone)]
struct PlaneCache {
    /// Last `DEFINE` (shape + dims + body) for this plane.
    last_define: Option<Vec<u8>>,
    /// Last `MOVE` (server position / visibility), latest-wins.
    last_move: Option<Vec<u8>>,
}

/// Client list + the last keyframe + per-plane sideband cache, under **one** lock so that priming
/// a joining client and broadcasting a keyframe/plane are mutually atomic (below).
#[derive(Default)]
struct HubState {
    clients: Vec<Client>,
    last_keyframe: Option<Vec<u8>>,
    /// Per-plane sideband cache keyed by `plane_id` (0 = cursor).
    planes: HashMap<u32, PlaneCache>,
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
    /// Set when a client connects so the producer can force a fresh IDR on the next frame.
    /// A late client is primed with the cached keyframe (instant image), but the live
    /// P-frames reference frames it never received, so it loses decode sync until the next
    /// IDR. With idle-skip the encoder's periodic IDR (one per GOP *encoded* frames) can be
    /// seconds of wall-clock away. Servicing this flag — respawn → first AU is an IDR — lets
    /// a joining client resync in ~one present instead of one GOP.
    keyframe_requested: AtomicBool,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Hub {
            state: Mutex::new(HubState::default()),
            keyframe_requested: AtomicBool::new(false),
        })
    }

    /// Ask the producer to emit a fresh IDR as soon as the next frame is submitted.
    fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Release);
    }

    /// Consume a pending keyframe request (true at most once per request).
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe_requested.swap(false, Ordering::AcqRel)
    }

    /// Broadcast one already-framed message to every client (dropping dead ones). If
    /// this AU is a keyframe, cache it for priming future clients — atomically with the
    /// send, so no joining client can straddle the store/send boundary.
    pub fn broadcast(&self, msg: Vec<u8>, keyframe: bool) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if keyframe {
            st.last_keyframe = Some(msg.clone());
        }
        // Enqueue into each client's BOUNDED queue (non-blocking). A client whose send thread
        // has died (blocking `ws.send` failed) is evicted. A client that sheds its backlog
        // (overflow) is left to resync on its own — we deliberately do NOT force a fresh IDR
        // here (see below). Holding the lock across the pushes is fine — each push only touches
        // that client's own mutex briefly and never blocks.
        let mut shed = false;
        st.clients.retain(|c| {
            if !c.alive.load(Ordering::Acquire) {
                c.q.close();
                return false;
            }
            if c.q.push(msg.clone(), keyframe) {
                shed = true;
            }
            true
        });
        drop(st);
        // A shed client is NOT force-resynced. Forcing a fresh IDR means respawning ffmpeg
        // (`ensure_encoder`: kill+wait+join+spawn), which runs INLINE on the vfio-user callback
        // thread — it blocks the guest's non-posted scanout doorbell, parks the vCPU holding
        // QEMU's BQL, and stalls the QMP monitor into multi-second cursor/input freezes
        // (mouse-lag-hunt rank 1/2: a slow viewer re-armed this every 500ms). A shed client
        // instead resyncs at the encoder's existing ~2s periodic IDR (`-g fps*2 -forced-idr 1`),
        // which costs the callback thread nothing. On a healthy LAN a client rarely sheds; when
        // it does, ≤2s of stale video for that one client beats freezing every VM's cursor.
        if shed {
            log::debug!("infiniPixel: slow client shed its backlog; resyncs at next periodic IDR");
        }
    }

    /// Broadcast one already-framed **plane-sideband** message (`XIPL`) to every client, and
    /// update the per-plane cache so a future joiner is primed with the current cursor state.
    /// `op`/`plane_id` come from the message's `PlaneHeader`.
    ///
    /// Control messages ride a separate lane from video: they **never** touch the encoder, never
    /// force an IDR, and bypass the video shed gate — so this is safe to call on the single
    /// vfio-user callback thread (it only enqueues into each client's non-blocking queue). Dead
    /// clients are evicted here too, so a static screen (plane traffic only, no video) still reaps.
    pub fn broadcast_control(&self, plane_id: u32, op: u8, msg: Vec<u8>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Update the priming cache. DEFINE replaces the shape (and clears the stale move so a
        // reconnecting client doesn't get a position from a previous shape); MOVE is latest-wins;
        // DESTROY drops the plane. DATA (media) isn't primed in v1.
        match op {
            o if o == proto::plane::op::DEFINE => {
                let e = st.planes.entry(plane_id).or_default();
                e.last_define = Some(msg.clone());
                e.last_move = None;
            }
            o if o == proto::plane::op::MOVE => {
                st.planes.entry(plane_id).or_default().last_move = Some(msg.clone());
            }
            o if o == proto::plane::op::DESTROY => {
                st.planes.remove(&plane_id);
            }
            _ => {}
        }
        st.clients.retain(|c| {
            if !c.alive.load(Ordering::Acquire) {
                c.q.close();
                return false;
            }
            c.q.push_control(msg.clone());
            true
        });
    }

    /// Forget the cached keyframe (call when the encoder is re-created for a new
    /// resolution, so a newly-connecting client isn't primed with a stale-size frame).
    pub fn reset_keyframe(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).last_keyframe = None;
    }

    /// Forget all cached plane state (e.g. on VM re-adoption, so a stale cursor shape isn't
    /// primed into a new client before the device re-solicits a fresh `CURSOR_UPDATE`).
    pub fn reset_planes(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).planes.clear();
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
        let q = ClientQueue::new();
        let alive = Arc::new(AtomicBool::new(true));
        // Prime + insert under the one lock, so this client is atomic w.r.t. broadcast:
        // it is primed with whatever keyframe is current AND joins the client list before
        // the next lock release — never primed with K1 but inserted after K2 was sent. The
        // primed frame is a keyframe, so a later overflow shed can resync against it.
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(k) = st.last_keyframe.clone() {
                let _ = q.push(k, true);
            }
            // Prime current plane state (cursor shape, then its last position) so a late/reconnecting
            // client shows the cursor immediately instead of waiting for the next shape change.
            for cache in st.planes.values() {
                if let Some(def) = &cache.last_define {
                    q.push_control(def.clone());
                }
                if let Some(mv) = &cache.last_move {
                    q.push_control(mv.clone());
                }
            }
            st.clients.push(Client { q: Arc::clone(&q), alive: Arc::clone(&alive) });
        }
        // The cached keyframe primed above is stale relative to the live P-frame stream;
        // ask the producer to emit a fresh IDR so this client can decode forward from it.
        self.request_keyframe();
        log::info!("infiniPixel client connected: {peer}");
        thread::spawn(move || {
            while let Some(msg) = q.pop() {
                if ws.send(tungstenite::Message::Binary(msg)).is_err() {
                    break;
                }
            }
            // Signal the broadcast side to evict us, and stop draining.
            alive.store(false, Ordering::Release);
            q.close();
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
/// Result of a deadlined [`Mailbox::take_timeout`].
enum MailboxTake {
    Frame(Vec<u8>),
    Timeout,
    Closed,
}

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
    #[allow(dead_code)]
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

    /// Consumer with an idle deadline: returns the newest frame, or [`MailboxTake::Timeout`]
    /// if none arrived within `dur` (so the feeder can re-feed the last frame to flush the
    /// AU ffmpeg is holding), or [`MailboxTake::Closed`] once the producer is done.
    fn take_timeout(&self, dur: Duration) -> MailboxTake {
        let mut s = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(f) = s.frame.take() {
                return MailboxTake::Frame(f);
            }
            if s.closed {
                return MailboxTake::Closed;
            }
            let (guard, wt) = self.cv.wait_timeout(s, dur).unwrap_or_else(|e| e.into_inner());
            s = guard;
            if s.frame.is_none() && !s.closed && wt.timed_out() {
                return MailboxTake::Timeout;
            }
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
    /// Codec for spawned encoders (H.264 default; HEVC for better compression).
    codec: Codec,
    /// Use NVENC intra-refresh instead of periodic IDRs (see [`EncoderConfig::intra_refresh`]).
    intra_refresh: bool,
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
    /// Set when a client connects: the next frame respawns the encoder so its first AU is a
    /// fresh IDR the late client can decode forward from. A same-size respawn KEEPS the
    /// cached keyframe (see `ensure_encoder`), so a client that joins while the guest is idle
    /// is still primed with the last valid frame instead of a black screen.
    force_respawn: bool,
    /// Last time a keyframe-request was serviced by respawning ffmpeg. Rate-limits forced
    /// IDRs so a persistently-too-slow viewer (which sheds and re-requests every few frames)
    /// cannot thrash the encoder — it resyncs at most once per [`MIN_FORCED_IDR_INTERVAL`].
    last_forced_idr: Option<Instant>,
    sent: u64,
    skipped: u64,
}

/// Minimum spacing between forced-IDR encoder respawns (see [`PixelStreamer::last_forced_idr`]).
const MIN_FORCED_IDR_INTERVAL: Duration = Duration::from_millis(500);

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
            codec: Codec::H264,
            intra_refresh: false,
            enc: None,
            hardware_failed: false,
            dedup: Deduper::default(),
            pending_flush: false,
            force_respawn: false,
            last_forced_idr: None,
            sent: 0,
            skipped: 0,
        })
    }

    /// Select the codec for future encoders (H.264 default; HEVC = better compression,
    /// decodable by the browser/ffmpeg path — the native openh264 client is H.264-only).
    /// Takes effect on the next encoder (re)creation.
    pub fn with_codec(mut self, codec: Codec) -> Self {
        self.codec = codec;
        self
    }

    /// Enable NVENC intra-refresh (smoother bitrate, no IDR spikes). Off by default —
    /// see [`EncoderConfig::intra_refresh`] for the late-join caveat.
    pub fn with_intra_refresh(mut self, on: bool) -> Self {
        self.intra_refresh = on;
        self
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
            codec: self.codec,
            // Intra-refresh is a hardware-only NVENC feature; software falls back to IDRs.
            intra_refresh: self.intra_refresh && hardware,
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
                // The AUD-delimited AU splitter downstream can only emit an access unit once
                // the NEXT frame's AUD arrives, so ffmpeg holds the last frame's AU (incl. a
                // keyframe's SPS/PPS+IDR) until another frame is fed. A guest that presents a
                // frame then goes fully idle would therefore never flush that AU — the last
                // visible frame (and the cached keyframe used to prime joiners) would never
                // reach clients, leaving a late viewer black. So: after a short idle with an
                // un-flushed frame outstanding, re-feed the last frame ONCE. That makes the
                // splitter emit the held AU; the re-fed (identical) frame costs ~0 and its own
                // AU is flushed by the next real frame. One extra frame per idle period keeps
                // the "idle ⇒ ~0 bits" property intact.
                let mut last: Option<Vec<u8>> = None;
                let mut needs_flush = false;
                loop {
                    match mailbox.take_timeout(Duration::from_millis(80)) {
                        MailboxTake::Frame(frame) => {
                            if sink.submit_bgra(&frame).is_err() {
                                alive.store(false, Ordering::Release);
                                break;
                            }
                            last = Some(frame);
                            needs_flush = true;
                        }
                        MailboxTake::Timeout => {
                            if needs_flush {
                                if let Some(f) = &last {
                                    if sink.submit_bgra(f).is_err() {
                                        alive.store(false, Ordering::Release);
                                        break;
                                    }
                                }
                                needs_flush = false;
                            }
                        }
                        MailboxTake::Closed => break,
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
        // A forced respawn (keyframe request) re-creates even a live, right-size session.
        let force = std::mem::take(&mut self.force_respawn);
        // Reuse only a live session of the right size (unless a respawn was forced).
        if !force {
            if let Some(sess) = self.enc.as_ref() {
                if sess.w == w && sess.h == h && sess.is_alive() {
                    return Ok(());
                }
            }
        }
        // Whether this respawn changes resolution — the ONLY case where the cached keyframe
        // (used to prime joiners) becomes invalid and must be dropped. A same-size respawn
        // keeps it: it is still a decodable IDR, so a client joining before the new encoder's
        // first IDR flushes — or while the guest is idle and that IDR never flushes — is
        // primed with a valid frame instead of a black screen.
        let size_changed = self.enc.as_ref().map(|s| s.w != w || s.h != h).unwrap_or(false);
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
        if size_changed {
            self.hub.reset_keyframe();
        }

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
        // A client just connected: respawn the encoder so its next AU is a fresh IDR the
        // late client can decode forward from, and push this frame past idle-skip so that IDR
        // is produced now (not one GOP later). The respawn KEEPS the cached keyframe (same
        // size), so a client that joins while the guest is idle — where the fresh IDR sits
        // un-flushed inside ffmpeg's AU splitter forever — is still primed with the last valid
        // frame rather than a black screen. Serviced even for an unchanged frame.
        if self.hub.take_keyframe_request() {
            let now = Instant::now();
            let due = self
                .last_forced_idr
                .map_or(true, |t| now.duration_since(t) >= MIN_FORCED_IDR_INTERVAL);
            if due {
                self.last_forced_idr = Some(now);
                self.force_respawn = true;
                self.ensure_encoder(w, h)?;
                self.dedup.changed(bgra); // seed the dedup baseline with this frame
                self.pending_flush = true;
                self.push(bgra);
                return Ok(());
            }
            // Rate-limited: keep the request pending (re-arm) and service it once the interval
            // clears — an overflow-shed client stays in `dropping` until then or a periodic
            // IDR. Fall through to normal idle-skip / encode handling for this frame.
            self.hub.request_keyframe();
        }
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

    /// Forward one plane-sideband op (cursor / future media) to every connected client on the
    /// **control lane**. Builds the `XIPL` wire message from `hdr` + `body` and hands it to the
    /// Hub, which caches it for late-joiner priming and enqueues it non-blocking per client.
    ///
    /// This **never** touches the encoder (no `ensure_encoder`, no ffmpeg spawn, no IDR) — the
    /// whole point of the sideband is that it is safe to call from the single vfio-user callback
    /// thread without risking the guest-scanout/BQL stall a re-encode would cause.
    pub fn send_plane(&self, hdr: &PlaneHeader, body: &[u8]) {
        self.hub
            .broadcast_control(hdr.plane_id, hdr.op, hdr.message(body));
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

        // parse() is the client half of the same contract → must round-trip to_bytes().
        let p = FrameHeader::parse(&b).expect("parse a valid header");
        assert_eq!(p.flags, h.flags);
        assert_eq!(p.codec, h.codec);
        assert_eq!(p.frame_seq, 0x0102_0304);
        assert_eq!(p.width, 1280);
        assert_eq!(p.height, 720);
        assert_eq!(p.pts_us, 0x0011_2233_4455_6677);
        assert_eq!(p.payload_len, 4096);
        assert!(p.is_keyframe());
        // Bad magic / short buffer → None.
        assert!(FrameHeader::parse(&[0u8; proto::HEADER_LEN]).is_none());
        assert!(FrameHeader::parse(&b[..10]).is_none());
    }

    #[test]
    fn plane_header_round_trips_little_endian() {
        let h = PlaneHeader {
            op: proto::plane::op::DEFINE,
            plane_kind: proto::plane::kind::CURSOR,
            flags: proto::plane::flags::VISIBLE | proto::plane::flags::PREMULTIPLIED,
            plane_id: 0,
            codec_or_format: 1,
            z_order: 7,
            width: 32,
            height: 48,
            hot_x: 4,
            hot_y: 5,
            pos_x: -12, // signed: cursor origin goes negative at a screen edge
            pos_y: 300,
            payload_len: 32 * 48 * 4,
        };
        let b = h.to_bytes();
        assert_eq!(b.len(), proto::plane::HEADER_LEN);
        assert_eq!(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), proto::plane::MAGIC);
        assert_eq!(b[4], proto::plane::VERSION);

        let p = PlaneHeader::parse(&b).expect("parse a valid plane header");
        assert_eq!(p, h);
        assert_eq!(p.pos_x, -12);
        // Too-short / wrong-magic → None.
        assert!(PlaneHeader::parse(&b[..20]).is_none());
        assert!(PlaneHeader::parse(&[0u8; proto::plane::HEADER_LEN]).is_none());
    }

    #[test]
    fn video_and_plane_magics_never_cross_parse() {
        // The lockstep-demux invariant (ADR risk #1): an old client must NEVER feed a plane
        // message to the video decoder, and a plane-aware client must NEVER feed a video frame
        // to the plane handler. The distinct magics guarantee each parser rejects the other's wire.
        let video = FrameHeader {
            flags: proto::flags::KEYFRAME,
            codec: proto::codec::H264,
            frame_seq: 1,
            width: 640,
            height: 480,
            pts_us: 0,
            payload_len: 0,
        }
        .to_bytes();
        let plane = PlaneHeader {
            op: proto::plane::op::MOVE,
            plane_kind: proto::plane::kind::CURSOR,
            flags: proto::plane::flags::VISIBLE,
            plane_id: 0,
            codec_or_format: 0,
            z_order: 0,
            width: 0,
            height: 0,
            hot_x: 0,
            hot_y: 0,
            pos_x: 10,
            pos_y: 20,
            payload_len: 0,
        }
        .to_bytes();
        assert_ne!(proto::MAGIC, proto::plane::MAGIC);
        // A plane message is dropped by the video parser…
        assert!(FrameHeader::parse(&plane).is_none());
        // …and a video frame is dropped by the plane parser.
        assert!(PlaneHeader::parse(&video).is_none());
    }

    fn cursor_hdr(op: u8, payload_len: u32) -> PlaneHeader {
        PlaneHeader {
            op,
            plane_kind: proto::plane::kind::CURSOR,
            flags: proto::plane::flags::VISIBLE,
            plane_id: 0,
            codec_or_format: 1,
            z_order: 0,
            width: 16,
            height: 16,
            hot_x: 0,
            hot_y: 0,
            pos_x: 5,
            pos_y: 6,
            payload_len,
        }
    }

    #[test]
    fn control_lane_bypasses_video_shed() {
        let q = ClientQueue::new();
        // Overflow the bounded video queue → it sheds (drops the backlog, enters `dropping`).
        let mut shed = false;
        for _ in 0..(max_client_queue() + 1) {
            shed |= q.push(vec![0xAA], false); // P-frames, never keyframes
        }
        assert!(shed, "video queue must shed once it exceeds cap");
        // A cursor control message must still be delivered while the video backlog is shed.
        q.push_control(vec![0xC0, 0xC1]);
        assert_eq!(q.pop(), Some(vec![0xC0, 0xC1]));
    }

    #[test]
    fn broadcast_control_caches_and_delivers() {
        let hub = Hub::new();
        let q = ClientQueue::new();
        hub.state.lock().unwrap().clients.push(Client {
            q: Arc::clone(&q),
            alive: Arc::new(AtomicBool::new(true)),
        });

        let def = cursor_hdr(proto::plane::op::DEFINE, 4);
        hub.broadcast_control(def.plane_id, def.op, def.message(&[1, 2, 3, 4]));
        let got = q.pop().expect("client received the DEFINE");
        assert_eq!(&got[0..4], &proto::plane::MAGIC.to_le_bytes());
        {
            let st = hub.state.lock().unwrap();
            let c = st.planes.get(&0).unwrap();
            assert!(c.last_define.is_some());
            assert!(c.last_move.is_none(), "DEFINE clears any stale move");
        }

        let mv = cursor_hdr(proto::plane::op::MOVE, 0);
        hub.broadcast_control(mv.plane_id, mv.op, mv.message(&[]));
        let _ = q.pop().unwrap();
        assert!(hub.state.lock().unwrap().planes.get(&0).unwrap().last_move.is_some());

        let de = cursor_hdr(proto::plane::op::DESTROY, 0);
        hub.broadcast_control(de.plane_id, de.op, de.message(&[]));
        let _ = q.pop().unwrap();
        assert!(hub.state.lock().unwrap().planes.get(&0).is_none(), "DESTROY drops the plane");
    }

    #[test]
    fn late_joiner_is_primed_with_cursor_state() {
        let hub = Hub::new();
        // Cache a shape then a position with no clients connected.
        let def = cursor_hdr(proto::plane::op::DEFINE, 4);
        hub.broadcast_control(def.plane_id, def.op, def.message(&[9, 9, 9, 9]));
        let mv = cursor_hdr(proto::plane::op::MOVE, 0);
        hub.broadcast_control(mv.plane_id, mv.op, mv.message(&[]));

        // Replay register()'s prime step into a fresh queue.
        let q = ClientQueue::new();
        {
            let st = hub.state.lock().unwrap();
            for cache in st.planes.values() {
                if let Some(d) = &cache.last_define {
                    q.push_control(d.clone());
                }
                if let Some(m) = &cache.last_move {
                    q.push_control(m.clone());
                }
            }
        }
        let first = q.pop().unwrap();
        let second = q.pop().unwrap();
        assert_eq!(first.len(), proto::plane::HEADER_LEN + 4, "DEFINE carries the shape body");
        assert_eq!(second.len(), proto::plane::HEADER_LEN, "MOVE is header-only");
        assert_eq!(PlaneHeader::parse(&first).unwrap().op, proto::plane::op::DEFINE);
        assert_eq!(PlaneHeader::parse(&second).unwrap().op, proto::plane::op::MOVE);
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

        let mut splitter = AuSplitter::new(Codec::H264);
        let mut aus: Vec<Vec<u8>> = Vec::new();
        splitter.push(&stream, |au| aus.push(au));

        assert_eq!(aus.len(), 2, "expected two complete access units");
        assert!(!au_is_keyframe(&aus[0], Codec::H264), "first AU is a plain slice");
        assert!(au_is_keyframe(&aus[1], Codec::H264), "second AU carries an SPS → keyframe");
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

        let mut splitter = AuSplitter::new(Codec::H264);
        let mut aus: Vec<Vec<u8>> = Vec::new();
        splitter.push(&stream, |au| aus.push(au));
        assert_eq!(aus.len(), 1, "push emits only the delimited first AU");

        splitter.flush(|au| aus.push(au));
        assert_eq!(aus.len(), 2, "flush emits the held final AU");
        assert!(au_is_keyframe(&aus[1], Codec::H264), "the flushed final AU is the IDR");
    }

    #[test]
    fn au_splitter_hevc_splits_and_detects_keyframe() {
        // HEVC NAL header is 2 bytes; type = (first byte >> 1) & 0x3F. AUD=35 (0x46),
        // TRAIL_R=1 (0x02), VPS=32 (0x40), IDR_W_RADL=19 (0x26).
        let aud = [0u8, 0, 0, 1, 0x46, 0x01];
        let trail = [0u8, 0, 0, 1, 0x02, 0x01, 0xAA]; // non-keyframe slice
        let vps = [0u8, 0, 0, 1, 0x40, 0x01, 0x0c]; // VPS → keyframe marker
        let idr = [0u8, 0, 0, 1, 0x26, 0x01, 0xBB]; // IDR_W_RADL
        let mut stream = Vec::new();
        stream.extend_from_slice(&aud);
        stream.extend_from_slice(&trail);
        stream.extend_from_slice(&aud);
        stream.extend_from_slice(&vps);
        stream.extend_from_slice(&idr);
        stream.extend_from_slice(&aud); // terminator

        let mut splitter = AuSplitter::new(Codec::Hevc);
        let mut aus: Vec<Vec<u8>> = Vec::new();
        splitter.push(&stream, |au| aus.push(au));
        assert_eq!(aus.len(), 2, "two HEVC access units");
        assert!(!au_is_keyframe(&aus[0], Codec::Hevc), "first HEVC AU is a plain trailing slice");
        assert!(au_is_keyframe(&aus[1], Codec::Hevc), "second HEVC AU has VPS+IDR → keyframe");
        // The H.264 splitter must NOT match HEVC's AUD (35), so it sees no AUs here.
        let mut h264 = AuSplitter::new(Codec::H264);
        let mut none: Vec<Vec<u8>> = Vec::new();
        h264.push(&stream, |au| none.push(au));
        assert!(none.is_empty(), "H.264 splitter ignores HEVC AUDs");
    }

    #[test]
    fn au_splitter_flush_ignores_bare_delimiter() {
        // A lone AUD (no following NAL) is not a real AU — flush must not emit it.
        let mut splitter = AuSplitter::new(Codec::H264);
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

        let mut splitter = AuSplitter::new(Codec::H264);
        let mut aus: Vec<Vec<u8>> = Vec::new();
        // Feed one byte at a time to stress the incremental scanner.
        for b in &full {
            splitter.push(&[*b], |au| aus.push(au));
        }
        assert_eq!(aus.len(), 1);
        assert!(au_is_keyframe(&aus[0], Codec::H264));
    }
}
