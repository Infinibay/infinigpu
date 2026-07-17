//! `infinigpu-pixel-demo [--width W] [--height H] [--fps N] [--bitrate KBPS] [--port P] [--codec h264|hevc] [--intra-refresh]`
//!
//! infiniPixel end to end: encode an animated test pattern on NVENC (H.264 or HEVC, low
//! latency), frame it in the owned infiniPixel protocol, and stream it over WebSocket.
//! Open `client/infinipixel.html?port=<P>` in a browser (or the native `infinigpu-viewer`)
//! to watch it decode — or run `scripts/infinipixel-test.sh` for a headless round-trip.

use infinigpu_pixel::{Codec, PixelStreamer, TestPattern};
use std::thread;
use std::time::Duration;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (mut w, mut h, mut fps, mut bitrate, mut port) = (1280u32, 720u32, 30u32, 6000u32, 8090u32);
    let mut codec = Codec::H264;
    let mut intra_refresh = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--width" => w = args.next().and_then(|s| s.parse().ok()).unwrap_or(w),
            "--height" => h = args.next().and_then(|s| s.parse().ok()).unwrap_or(h),
            "--fps" => fps = args.next().and_then(|s| s.parse().ok()).unwrap_or(fps),
            "--bitrate" => bitrate = args.next().and_then(|s| s.parse().ok()).unwrap_or(bitrate),
            "--port" => port = args.next().and_then(|s| s.parse().ok()).unwrap_or(port),
            "--codec" => {
                codec = match args.next().as_deref() {
                    Some("h264") | None => Codec::H264,
                    Some("hevc") | Some("h265") => Codec::Hevc,
                    Some(other) => {
                        eprintln!("infinigpu-pixel-demo: unknown codec {other:?} (h264|hevc)");
                        std::process::exit(2);
                    }
                }
            }
            "--intra-refresh" => intra_refresh = true,
            other => {
                eprintln!("infinigpu-pixel-demo: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }

    // new() binds the server + creates the encoder lazily on the first frame, so the
    // codec/intra-refresh builders apply to it.
    let mut streamer = match PixelStreamer::new(fps, bitrate, port as u16) {
        Ok(s) => s.with_codec(codec).with_intra_refresh(intra_refresh),
        Err(e) => {
            eprintln!("failed to start streamer (need ffmpeg): {e}");
            std::process::exit(1);
        }
    };
    let codec_name = match codec {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
    };
    let refresh = if intra_refresh { ", intra-refresh" } else { "" };
    println!(">> infiniPixel: {w}x{h} @ {fps}fps, {bitrate} kbps {codec_name} (NVENC{refresh})");
    println!(">> stream:  ws://<host>:{port}");
    println!(">> watch:   client/infinipixel.html?port={port} — or: infinigpu-viewer --port {port}");

    // Feed the animated test pattern at the target fps.
    let mut pat = TestPattern::new(w, h);
    let interval = Duration::from_micros(1_000_000 / fps as u64);
    loop {
        let frame = pat.next_bgra();
        if streamer.submit_bgra(frame, w, h).is_err() {
            break;
        }
        thread::sleep(interval);
    }
    eprintln!("encoder stream ended");
}
