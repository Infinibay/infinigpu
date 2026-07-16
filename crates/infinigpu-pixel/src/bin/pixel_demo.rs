//! `infinigpu-pixel-demo [--width W] [--height H] [--fps N] [--bitrate KBPS] [--port P] [--sw]`
//!
//! infiniPixel v0 end to end: encode an animated test pattern on NVENC (H.264, low
//! latency), frame it in the owned infiniPixel protocol, and stream it over WebSocket.
//! Open `client/infinipixel.html?port=<P>` in a browser to watch it decode with
//! WebCodecs — or run `scripts/infinipixel-test.sh` for a headless round-trip check.

use infinigpu_pixel::{PixelStreamer, TestPattern};
use std::thread;
use std::time::Duration;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (mut w, mut h, mut fps, mut bitrate, mut port) = (1280u32, 720u32, 30u32, 6000u32, 8090u32);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut next = || args.next().and_then(|s| s.parse().ok());
        match a.as_str() {
            "--width" => w = next().unwrap_or(w),
            "--height" => h = next().unwrap_or(h),
            "--fps" => fps = next().unwrap_or(fps),
            "--bitrate" => bitrate = next().unwrap_or(bitrate),
            "--port" => port = next().unwrap_or(port),
            other => {
                eprintln!("infinigpu-pixel-demo: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }

    let mut streamer = match PixelStreamer::start(w, h, fps, bitrate, port as u16) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to start streamer (need ffmpeg): {e}");
            std::process::exit(1);
        }
    };
    println!(">> infiniPixel v0: {w}x{h} @ {fps}fps, {bitrate} kbps H.264 (NVENC)");
    println!(">> stream:  ws://<host>:{port}");
    println!(">> watch:   open client/infinipixel.html?port={port} in a browser");

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
