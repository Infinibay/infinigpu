//! infiniPixel network + decode: connect to the server's WebSocket, parse the owned
//! [`FrameHeader`] contract, and decode each H.264 access unit to RGBA with openh264.
//!
//! One WebSocket **binary message = one full frame** (header + one Annex-B access unit),
//! exactly as the server sends it (`FrameHeader::message`) — so there is no reassembly:
//! parse the 32-byte header, feed the rest to the decoder. A newly-connected client is
//! primed by the server with the last keyframe (SPS/PPS + IDR), so the decoder
//! initialises on the first message.

use infinigpu_pixel::{proto, FrameHeader};
use openh264::formats::YUVSource; // brings `dimensions()` into scope for DecodedYUV
use std::error::Error;
use std::sync::{Arc, Mutex};
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
pub fn run_stream(
    url: &str,
    mut on_frame: impl FnMut(DecodedFrame) -> bool,
) -> Result<(), Box<dyn Error>> {
    let (mut ws, _resp) = tungstenite::connect(url)?;
    log::info!("infiniPixel: connected to {url}");
    let mut decoder = openh264::decoder::Decoder::new()?;

    // Sync discipline: the server may prime a joining client with a cached keyframe that
    // is then followed by a *gap* (the P-frames between that keyframe and "now" were sent
    // before we connected). A P-frame that references a frame we never got makes a strict
    // decoder (openh264) lose its reference. So: only start decoding at a keyframe, and on
    // any decode error drop back to waiting for the next keyframe — which begins a run we
    // *do* receive contiguously. A late client thus resyncs at the next periodic IDR.
    let mut synced = false;

    loop {
        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(e) => return Err(Box::new(e)),
        };
        let data = match msg {
            Message::Binary(b) => b,
            Message::Ping(p) => {
                let _ = ws.send(Message::Pong(p));
                continue;
            }
            Message::Close(_) => break,
            _ => continue,
        };
        let Some(hdr) = FrameHeader::parse(&data) else {
            log::warn!("dropped a message with a bad/short infiniPixel header");
            continue;
        };
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
