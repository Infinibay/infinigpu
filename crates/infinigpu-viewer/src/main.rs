//! # infinigpu-viewer — native infiniPixel client
//!
//! The desktop client for the infiniPixel remote-display stream — the `virt-viewer`
//! replacement, **without GTK or Qt**: [`winit`] for native windowing (Wayland on Linux,
//! Win32 on Windows), [`ash`]/Vulkan for presentation (swapchain + `vkCmdBlitImage`),
//! [`tungstenite`] for the WebSocket transport, and [`openh264`] for H.264 decode (a
//! small, embeddable, BSD codec — no external `ffmpeg` on the client). GPU decode
//! (Vulkan Video / NVDEC) is the v1 upgrade behind the same decoder seam.
//!
//! ```text
//! infinigpu-viewer [--headless] [--frames N] [--out FILE] [--url ws://HOST:PORT | --port PORT]
//! ```
//! Windowed by default; `--headless` decodes N frames and (optionally) writes a PPM — the
//! path that runs on a box with no display.

mod headless;
mod stream;
mod window;

use std::process::ExitCode;

const DEFAULT_URL: &str = "ws://127.0.0.1:8090";

fn usage() -> ! {
    eprintln!(
        "infinigpu-viewer — native infiniPixel client\n\n\
         USAGE:\n  infinigpu-viewer [--headless] [--frames N] [--out FILE] \
         [--url ws://HOST:PORT | --port PORT]\n\n\
         Defaults: --url {DEFAULT_URL}; windowed unless --headless."
    );
    std::process::exit(2);
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut headless = false;
    let mut frames = 60usize;
    let mut out: Option<String> = None;
    let mut url: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--headless" => headless = true,
            "--frames" => {
                i += 1;
                frames = args.get(i).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
            }
            "--out" => {
                i += 1;
                out = Some(args.get(i).cloned().unwrap_or_else(|| usage()));
            }
            "--url" => {
                i += 1;
                url = Some(args.get(i).cloned().unwrap_or_else(|| usage()));
            }
            "--port" => {
                i += 1;
                let p: u16 = args.get(i).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
                url = Some(format!("ws://127.0.0.1:{p}"));
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
        i += 1;
    }

    let url = url.unwrap_or_else(|| DEFAULT_URL.to_string());
    let result = if headless {
        headless::run(&url, frames, out.as_deref())
    } else {
        window::run(&url)
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infinigpu-viewer: {e}");
            ExitCode::FAILURE
        }
    }
}
