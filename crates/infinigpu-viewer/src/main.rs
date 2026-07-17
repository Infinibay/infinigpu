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
mod input;
mod stream;
mod window;

use std::process::ExitCode;

const DEFAULT_URL: &str = "ws://127.0.0.1:8090";

/// Return `url` with its port replaced (or appended) to `port`, preserving the scheme,
/// host, and any path. Tolerates a URL that carries no port yet (e.g. `ws://192.168.0.199`).
fn set_ws_port(url: &str, port: u16) -> String {
    let (scheme, rest) = match url.find("://") {
        Some(idx) => (&url[..idx + 3], &url[idx + 3..]),
        None => ("ws://", url),
    };
    let (hostport, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    let host = hostport.split(':').next().unwrap_or(hostport);
    format!("{scheme}{host}:{port}{path}")
}

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    // --debug raises the default log level to `debug` so run_stream's per-message trace is
    // visible (RUST_LOG still overrides). Parsed before logger init since it sets the level.
    // Use it to tell "connected but no frames arriving" (server/guest side) from "frames
    // arrive but the window is black" (decode/present side).
    let debug = args.iter().any(|a| a == "--debug");
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(if debug { "debug" } else { "info" }),
    )
    .init();

    let mut headless = false;
    let mut frames = 60usize;
    let mut out: Option<String> = None;
    let mut url: Option<String> = None;
    let mut port: Option<u16> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--headless" => headless = true,
            "--debug" => {} // consumed above (sets the log level); listed so it isn't "unknown"
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
                port = Some(args.get(i).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()));
            }
            "-h" | "--help" => usage(),
            // Accept a bare ws://HOST:PORT positional — it's what the UI prints
            // ("infinigpu-viewer ws://192.168.0.199:6120"), so it should just work.
            other if other.starts_with("ws://") || other.starts_with("wss://") => {
                url = Some(other.to_string());
            }
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
        i += 1;
    }

    // Resolve host + port. `--port` sets ONLY the port: it combines with the host from
    // `--url` (or a bare ws:// positional) and falls back to localhost only when no host
    // was given. Order-independent, so `--url ws://HOST --port 6120` targets HOST — not
    // 127.0.0.1 (previously `--port` overwrote the whole URL with a hardcoded localhost).
    let url = match (url, port) {
        (Some(u), Some(p)) => set_ws_port(&u, p),
        (Some(u), None) => u,
        (None, Some(p)) => format!("ws://127.0.0.1:{p}"),
        (None, None) => DEFAULT_URL.to_string(),
    };
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
