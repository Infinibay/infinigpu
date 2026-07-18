//! infiniPixel network + decode: connect to the server's WebSocket, parse the owned
//! [`FrameHeader`] contract, and decode each H.264 access unit to RGBA with openh264.
//!
//! One WebSocket **binary message = one full frame** (header + one Annex-B access unit),
//! exactly as the server sends it (`FrameHeader::message`) — so there is no reassembly:
//! parse the 32-byte header, feed the rest to the decoder. A newly-connected client is
//! primed by the server with the last keyframe (SPS/PPS + IDR), so the decoder
//! initialises on the first message.

use infinigpu_pixel::{proto, FrameHeader, PlaneHeader};
use openh264::formats::YUVSource; // brings `dimensions()` into scope for DecodedYUV
use std::error::Error;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

/// A decoded frame ready to upload to the GPU. `rgba` is tightly packed `width*height*4`
/// in R8G8B8A8 order (openh264's output order).
pub struct DecodedFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub seq: u32,
    pub keyframe: bool,
}

/// Latest-frame-wins hand-off from the network/decode thread to the render loop: the
/// producer overwrites, the consumer takes. Decode jitter never stalls presentation and
/// intermediate frames are dropped when the display can't keep up (display is lossy).
#[derive(Default)]
pub struct FrameSlot {
    inner: Mutex<Option<DecodedFrame>>,
}

impl FrameSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(FrameSlot::default())
    }
    /// Publish the newest decoded frame, dropping any not-yet-consumed one.
    pub fn put(&self, f: DecodedFrame) {
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }
    /// Take the latest frame if one arrived since the last call.
    pub fn take(&self) -> Option<DecodedFrame> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Connect to `url` (e.g. `ws://127.0.0.1:8090`) and drive decoding, invoking `on_frame`
/// for every decoded frame. Blocks until the socket closes, `on_frame` returns `false`,
/// or an error occurs. Runs on the caller's thread (spawn it for a windowed client).
///
/// `input_rx`, if given, is the guest-input back-channel: JSON strings (the compact
/// infiniPixel input protocol — see the viewer's `input` module) pushed by the window
/// thread are forwarded to the server as **text** WebSocket messages, which the master
/// relay injects into the guest over QMP. Frames arrive as **binary**; the two directions
/// share this one socket. When `input_rx` is present the read is time-boxed so a static
/// screen (no incoming frames) never stalls outgoing input.
pub fn run_stream(
    url: &str,
    input_rx: Option<Receiver<String>>,
    mut on_frame: impl FnMut(DecodedFrame) -> bool,
) -> Result<(), Box<dyn Error>> {
    let (mut ws, _resp) = tungstenite::connect(url)?;
    log::info!("infiniPixel: connected to {url}");
    // TCP_NODELAY: disable Nagle so a small frame or an input event isn't held for
    // delayed-ACK coalescing (up to ~40ms of avoidable tail latency). Applies both ways.
    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        let _ = s.set_nodelay(true);
    }
    // Time-box reads so we can interleave outgoing input on the same socket. A tighter box
    // bounds input-send latency (input is flushed each loop turn); 4ms keeps the idle spin
    // cheap. Without a pending-input channel we leave the socket blocking (headless path).
    if input_rx.is_some() {
        if let MaybeTlsStream::Plain(s) = ws.get_ref() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(4)));
        }
    }
    let mut decoder = openh264::decoder::Decoder::new()?;

    // Sync discipline: the server may prime a joining client with a cached keyframe that
    // is then followed by a *gap* (the P-frames between that keyframe and "now" were sent
    // before we connected). A P-frame that references a frame we never got makes a strict
    // decoder (openh264) lose its reference. So: only start decoding at a keyframe, and on
    // any decode error drop back to waiting for the next keyframe — which begins a run we
    // *do* receive contiguously. A late client thus resyncs at the next periodic IDR.
    let mut synced = false;

    loop {
        // Forward any pending guest input on the SAME socket first, so a static screen —
        // where the time-boxed reads below just keep timing out — still delivers mouse and
        // keyboard promptly (bounded ~16ms latency). Non-blocking drain; the window thread is
        // the producer. Frames are binary, input is text; the two directions share the socket.
        if let Some(rx) = &input_rx {
            while let Ok(js) = rx.try_recv() {
                if let Err(e) = ws.send(Message::Text(js.into())) {
                    log::debug!("infiniPixel: input send failed: {e}");
                }
            }
        }

        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            // Time-boxed read (input interleaving) with no frame ready yet: NOT an error —
            // loop back to drain input + retry. tungstenite buffers any partial frame, so a
            // large keyframe split across several timeouts resumes cleanly. This arm only
            // fires when `input_rx` set the read timeout above (headless path stays blocking).
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(Box::new(e)),
        };
        let data = match msg {
            Message::Binary(b) => {
                log::debug!("recv binary message: {} bytes", b.len());
                b
            }
            Message::Ping(p) => {
                log::debug!("recv ping: {} bytes", p.len());
                let _ = ws.send(Message::Pong(p));
                continue;
            }
            Message::Close(c) => {
                log::debug!("recv close: {c:?}");
                break;
            }
            other => {
                log::debug!("recv non-frame message ({} bytes), ignoring", other.len());
                continue;
            }
        };
        // Demux by magic: a plane-sideband message (`XIPL`) is not a video frame — route it
        // away from the decoder so its bytes never reach openh264. The distinct magic means
        // even an un-updated build safe-drops it in `FrameHeader::parse` below; recognizing it
        // here keeps it out of the "bad header" warning and readies the PR-C4 overlay handler.
        if data.len() >= 4
            && u32::from_le_bytes([data[0], data[1], data[2], data[3]]) == proto::plane::MAGIC
        {
            if let Some(ph) = PlaneHeader::parse(&data) {
                log::debug!(
                    "recv plane sideband: op={} kind={} id={} ({} body bytes) [overlay handled in PR-C4]",
                    ph.op,
                    ph.plane_kind,
                    ph.plane_id,
                    data.len().saturating_sub(proto::plane::HEADER_LEN)
                );
            }
            continue;
        }
        let Some(hdr) = FrameHeader::parse(&data) else {
            log::warn!("dropped a message with a bad/short infiniPixel header");
            continue;
        };
        log::debug!(
            "frame hdr: seq={} keyframe={} au={} bytes",
            hdr.frame_seq,
            hdr.is_keyframe(),
            data.len().saturating_sub(proto::HEADER_LEN)
        );
        // Until we're synced, ignore everything but a keyframe (feeding a P-frame with no
        // reference just spams decode errors).
        if !synced && !hdr.is_keyframe() {
            continue;
        }
        let au = &data[proto::HEADER_LEN..];
        // openh264 takes Annex-B; our AUs are AUD-delimited Annex-B — feed directly.
        match decoder.decode(au) {
            Ok(Some(yuv)) => {
                synced = true;
                let (w, h) = yuv.dimensions();
                log::debug!("decoded frame seq={} {w}x{h} key={}", hdr.frame_seq, hdr.is_keyframe());
                let mut rgba = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                let frame = DecodedFrame {
                    rgba,
                    width: w as u32,
                    height: h as u32,
                    seq: hdr.frame_seq,
                    keyframe: hdr.is_keyframe(),
                };
                if !on_frame(frame) {
                    break;
                }
            }
            // The decoder buffered the NALs but has no complete picture yet — normal.
            Ok(None) => {}
            // Lost reference / missing param sets (typically the join gap described above):
            // drop back to waiting for the next keyframe rather than feeding more P-frames.
            Err(e) => {
                if synced {
                    log::debug!("decode lost sync (seq {}: {e}); waiting for next keyframe", hdr.frame_seq);
                }
                synced = false;
            }
        }
    }
    log::info!("infiniPixel: stream ended");
    Ok(())
}
