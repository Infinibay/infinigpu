//! `infinigpu-replay-server --socket <path>` — the per-VM jailed replay process (ADR-0003).
//!
//! One instance per VM: it applies a process jail (see [`infinigpu_replay::process::apply_jail`]),
//! opens the physical GPU once, and serves render requests over a UNIX socket. Running the
//! replay out-of-process means a GPU fault's blast radius is a single VM, and NVML attributes
//! this process's VRAM to that VM exactly (one pid per VM).

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut socket: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" | "-s" => socket = args.next(),
            other => {
                eprintln!("infinigpu-replay-server: unknown argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(socket) = socket else {
        eprintln!("usage: infinigpu-replay-server --socket <path>");
        return ExitCode::from(2);
    };

    match infinigpu_replay::process::serve(&PathBuf::from(socket)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infinigpu-replay-server: {e}");
            ExitCode::FAILURE
        }
    }
}
