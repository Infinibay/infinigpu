//! `infinigpu-pixel-demo [--width W] [--height H] [--fps N] [--bitrate KBPS] [--port P] [--sw]`
//!
//! infiniPixel v0 end to end: encode an animated test pattern on NVENC (H.264, low
//! latency), frame it in the owned infiniPixel protocol, and stream it over WebSocket.
//! Open `client/infinipixel.html?port=<P>` in a browser to watch it decode with
//! WebCodecs — or run `scripts/infinipixel-test.sh` for a headless round-trip check.

use infinigpu_pixel::{Encoder, EncoderConfig, FrameHeader, Hub, TestPattern};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut cfg = EncoderConfig {
        width: 1280,
        height: 720,
        fps: 30,
        bitrate_kbps: 6000,
        prefer_hardware: true,
    };
    let mut port = 8090u16;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut next_u32 = || args.next().and_then(|s| s.parse().ok());
        match a.as_str() {
            "--width" => cfg.width = next_u32().unwrap_or(cfg.width),
            "--height" => cfg.height = next_u32().unwrap_or(cfg.height),
            "--fps" => cfg.fps = next_u32().unwrap_or(cfg.fps),
            "--bitrate" => cfg.bitrate_kbps = next_u32().unwrap_or(cfg.bitrate_kbps),
            "--port" => port = args.next().and_then(|s| s.parse().ok()).unwrap_or(port),
            "--sw" => cfg.prefer_hardware = false,
            other => {
                eprintln!("infinigpu-pixel-demo: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }

    let enc = match Encoder::spawn(&cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to start encoder (need ffmpeg): {e}");
            std::process::exit(1);
        }
    };
    println!(
        ">> infiniPixel v0: {}x{} @ {}fps, {} kbps, {} H.264",
        cfg.width,
        cfg.height,
        cfg.fps,
        cfg.bitrate_kbps,
        if enc.is_hardware() { "NVENC (hardware)" } else { "libx264 (software)" }
    );

    let hub = Hub::new();
    {
        let hub = Arc::clone(&hub);
        let addr = format!("0.0.0.0:{port}");
        thread::spawn(move || {
            if let Err(e) = hub.serve(&addr) {
                eprintln!("server error: {e}");
            }
        });
    }
    println!(">> stream:  ws://<host>:{port}");
    println!(">> watch:   open client/infinipixel.html?port={port} in a browser");

    // Producer: render the test pattern and feed the encoder at the target fps.
    let mut enc = enc;
    let mut sink = enc.take_sink().expect("encoder sink");
    let (w, h, fps) = (cfg.width, cfg.height, cfg.fps);
    thread::spawn(move || {
        let mut pat = TestPattern::new(w, h);
        let interval = Duration::from_micros(1_000_000 / fps as u64);
        loop {
            let frame = pat.next_bgra();
            if sink.submit_bgra(frame).is_err() {
                break;
            }
            thread::sleep(interval);
        }
    });

    // Consumer: drain encoded AUs, frame them, broadcast to all clients.
    let codec = enc.codec().wire();
    let us_per_frame = 1_000_000u64 / fps as u64;
    while let Some(au) = enc.recv() {
        let flags = if au.keyframe {
            infinigpu_pixel::proto::flags::KEYFRAME
        } else {
            0
        };
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
    eprintln!("encoder stream ended");
}
