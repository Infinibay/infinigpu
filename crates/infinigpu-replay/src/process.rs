//! # Per-VM jailed replay process (ADR-0003, isolation half)
//!
//! The north-star architecture runs the Vulkan replay **one jailed process per VM**, not
//! in-process in the device. Two things fall out of the split that the in-process design
//! can't have:
//! - **Blast radius = one VM.** A GPU fault or a driver crash takes down *that* replay
//!   process, not the shared device serving every VM.
//! - **Exact attribution.** Each replay process is one OS pid, so NVML
//!   ([`infinigpu_nvml`]) attributes its VRAM/utilization precisely — no per-VM estimate.
//!
//! This module is the mechanism: a tiny length-prefixed protocol over a UNIX socket, a
//! [`serve`] loop (the `infinigpu-replay-server` binary) that owns one [`HostGpu`] and
//! renders requests, and a [`ReplayProcess`] client that spawns + talks to one server.
//! Jailing today is `setrlimit` (no core dumps of VRAM, capped fds/procs); namespaces +
//! seccomp are the documented hardening step (see [`apply_jail`]).

use crate::{Frame, HostGpu};
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

type R<T> = Result<T, Box<dyn std::error::Error>>;

const TAG_CLEAR: u8 = 1;
const TAG_TRIANGLE: u8 = 2;
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;

/// A render request sent from the device to a VM's replay process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RenderRequest {
    /// Clear-render `w`×`h` to `rgba` (0.0–1.0), read back.
    Clear { width: u32, height: u32, rgba: [f32; 4] },
    /// Shader-executed triangle over background `bg`, read back.
    Triangle { width: u32, height: u32, bg: [f32; 4] },
}

impl RenderRequest {
    fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let (tag, width, height, color) = match *self {
            RenderRequest::Clear { width, height, rgba } => (TAG_CLEAR, width, height, rgba),
            RenderRequest::Triangle { width, height, bg } => (TAG_TRIANGLE, width, height, bg),
        };
        w.write_all(&[tag])?;
        w.write_all(&width.to_le_bytes())?;
        w.write_all(&height.to_le_bytes())?;
        for c in color {
            w.write_all(&c.to_le_bytes())?;
        }
        w.flush()
    }

    fn read_from(r: &mut impl Read) -> io::Result<Option<RenderRequest>> {
        let mut tag = [0u8; 1];
        // A clean EOF (peer closed) → None; a partial read → error.
        match r.read_exact(&mut tag) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let width = read_u32(r)?;
        let height = read_u32(r)?;
        let mut color = [0f32; 4];
        for c in color.iter_mut() {
            *c = read_f32(r)?;
        }
        match tag[0] {
            TAG_CLEAR => Ok(Some(RenderRequest::Clear { width, height, rgba: color })),
            TAG_TRIANGLE => Ok(Some(RenderRequest::Triangle { width, height, bg: color })),
            other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad request tag {other}"))),
        }
    }
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_f32(r: &mut impl Read) -> io::Result<f32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(f32::from_le_bytes(b))
}

fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    w.write_all(&[STATUS_OK])?;
    w.write_all(&frame.width.to_le_bytes())?;
    w.write_all(&frame.height.to_le_bytes())?;
    w.write_all(&(frame.rgba.len() as u32).to_le_bytes())?;
    w.write_all(&frame.rgba)?;
    w.flush()
}

fn write_error(w: &mut impl Write, msg: &str) -> io::Result<()> {
    let bytes = msg.as_bytes();
    w.write_all(&[STATUS_ERR])?;
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

fn read_response(r: &mut impl Read) -> R<Frame> {
    let mut status = [0u8; 1];
    r.read_exact(&mut status)?;
    if status[0] == STATUS_OK {
        let width = read_u32(r)?;
        let height = read_u32(r)?;
        let len = read_u32(r)? as usize;
        // Cap so a hostile server can't make us allocate wildly.
        if len > 256 * 1024 * 1024 {
            return Err("replay response frame too large".into());
        }
        let mut rgba = vec![0u8; len];
        r.read_exact(&mut rgba)?;
        Ok(Frame { width, height, rgba })
    } else {
        let len = read_u32(r)? as usize;
        let mut msg = vec![0u8; len.min(4096)];
        r.read_exact(&mut msg)?;
        Err(format!("replay error: {}", String::from_utf8_lossy(&msg)).into())
    }
}

/// Best-effort process jail for the replay server (Linux). Reduces blast radius:
/// no core dumps (VRAM contents never hit disk), capped open files and child processes.
/// **Not** a full sandbox — the strong version adds a mount/PID/net namespace (`unshare`)
/// and a seccomp allowlist; both are the documented next step (they need CAP_SYS_ADMIN or
/// user-namespaces and a syscall policy tuned to the GPU driver). We deliberately do NOT
/// set `RLIMIT_AS`: GPU drivers map very large virtual ranges and a low AS cap breaks them.
pub fn apply_jail() {
    // SAFETY: setrlimit with valid resource ids and a well-formed rlimit struct.
    unsafe {
        let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &no_core);
        let fds = libc::rlimit { rlim_cur: 256, rlim_max: 256 };
        libc::setrlimit(libc::RLIMIT_NOFILE, &fds);
        let procs = libc::rlimit { rlim_cur: 64, rlim_max: 64 };
        libc::setrlimit(libc::RLIMIT_NPROC, &procs);
    }
}

/// Run the replay server: bind `socket_path`, apply the jail, open the GPU once, and serve
/// render requests until the peer disconnects. This is the body of `infinigpu-replay-server`.
pub fn serve(socket_path: &Path) -> R<()> {
    apply_jail();
    // A stale socket from a prior run would make bind fail.
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    // One GPU context for this VM's whole lifetime (this process = this VM).
    let gpu = HostGpu::open()?;
    log_line(&format!("replay-server: {} ready on {}", gpu.device_name(), socket_path.display()));

    for stream in listener.incoming() {
        let mut stream = stream?;
        if let Err(e) = handle_conn(&mut stream, &gpu) {
            log_line(&format!("replay-server: connection ended: {e}"));
        }
    }
    Ok(())
}

fn handle_conn(stream: &mut UnixStream, gpu: &HostGpu) -> R<()> {
    loop {
        let req = match RenderRequest::read_from(stream)? {
            Some(r) => r,
            None => return Ok(()), // peer closed
        };
        let rendered = match req {
            RenderRequest::Clear { width, height, rgba } => gpu.render_clear(width, height, rgba),
            RenderRequest::Triangle { width, height, bg } => gpu.render_triangle(width, height, bg),
        };
        match rendered {
            Ok(frame) => write_frame(stream, &frame)?,
            Err(e) => write_error(stream, &e.to_string())?,
        }
    }
}

fn log_line(msg: &str) {
    // The server may run without a logger installed; keep it dependency-light.
    eprintln!("{msg}");
}

/// Client handle to a per-VM replay **process**. Spawns the `infinigpu-replay-server`
/// binary, waits for its socket, and issues render requests over it. Dropping it reaps the
/// process (the GPU context — and its NVML-attributed VRAM — goes with it).
pub struct ReplayProcess {
    child: std::process::Child,
    stream: UnixStream,
    socket_path: std::path::PathBuf,
}

impl ReplayProcess {
    /// Spawn the server binary `server_bin` on a fresh `socket_path` and connect. Waits up
    /// to `timeout_ms` for the socket to accept.
    pub fn spawn(server_bin: &Path, socket_path: &Path, timeout_ms: u64) -> R<Self> {
        let _ = std::fs::remove_file(socket_path);
        let child = std::process::Command::new(server_bin)
            .arg("--socket")
            .arg(socket_path)
            .spawn()?;

        // Poll for the socket to become connectable.
        let start = std::time::Instant::now();
        let stream = loop {
            match UnixStream::connect(socket_path) {
                Ok(s) => break s,
                Err(_) if start.elapsed().as_millis() < timeout_ms as u128 => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(format!("replay process socket never came up: {e}").into()),
            }
        };
        Ok(ReplayProcess { child, stream, socket_path: socket_path.to_path_buf() })
    }

    /// The server process's OS pid — feed to `infinigpu_nvml` for per-VM VRAM attribution.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Send a render request and read back the frame.
    pub fn render(&mut self, req: RenderRequest) -> R<Frame> {
        req.write_to(&mut self.stream)?;
        read_response(&mut self.stream)
    }
}

impl Drop for ReplayProcess {
    fn drop(&mut self) {
        // Closing the stream lets the server's accept loop see EOF; then reap the process.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_round_trips() {
        for req in [
            RenderRequest::Clear { width: 640, height: 480, rgba: [0.1, 0.2, 0.3, 1.0] },
            RenderRequest::Triangle { width: 1920, height: 1080, bg: [0.0, 0.5, 1.0, 1.0] },
        ] {
            let mut buf = Vec::new();
            req.write_to(&mut buf).unwrap();
            let mut cur = Cursor::new(buf);
            assert_eq!(RenderRequest::read_from(&mut cur).unwrap().unwrap(), req);
        }
    }

    #[test]
    fn empty_stream_is_clean_eof() {
        let mut cur = Cursor::new(Vec::new());
        assert!(RenderRequest::read_from(&mut cur).unwrap().is_none());
    }

    #[test]
    fn frame_response_round_trips() {
        let frame = Frame { width: 2, height: 1, rgba: vec![1, 2, 3, 4, 5, 6, 7, 8] };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        let got = read_response(&mut Cursor::new(buf)).unwrap();
        assert_eq!((got.width, got.height, got.rgba), (2, 1, frame.rgba));
    }

    #[test]
    fn error_response_surfaces_message() {
        let mut buf = Vec::new();
        write_error(&mut buf, "boom").unwrap();
        let err = read_response(&mut Cursor::new(buf)).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }
}
