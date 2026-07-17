//! `infinigpu-jailed-replay-demo` — the ADR-0003 isolation half, end to end.
//!
//! Spawns the `infinigpu-replay-server` as a **separate jailed process**, renders a clear
//! and a shader triangle over the UNIX-socket protocol (verifying the read-back pixels),
//! and then uses **NVML** to show that the GPU VRAM is attributed to *that process's pid* —
//! the exact per-VM accounting the in-process design can't do. Requires the A5000; no QEMU.

use infinigpu_nvml::NvmlProbe;
use infinigpu_replay::process::{RenderRequest, ReplayProcess};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The replay-server binary sits next to this demo in the same target dir.
    let server_bin = std::env::current_exe()?
        .parent()
        .unwrap()
        .join("infinigpu-replay-server");
    if !server_bin.exists() {
        return Err(format!("build it first: cargo build -p infinigpu-replay --bin infinigpu-replay-server ({} missing)", server_bin.display()).into());
    }
    let socket = std::env::temp_dir().join(format!("infinigpu-replay-demo-{}.sock", std::process::id()));

    println!("spawning jailed replay process: {}", server_bin.display());
    let mut replay = ReplayProcess::spawn(&server_bin, &socket, 8000)?;
    let pid = replay.pid();
    println!("  replay process pid = {pid}");

    // --- render a clear over IPC and verify a pixel ---
    let clear = [0.0_f32, 0.6, 0.8, 1.0]; // → [0,153,204,255]
    let f = replay.render(RenderRequest::Clear { width: 256, height: 256, rgba: clear })?;
    let p = f.pixel(128, 128);
    println!("  clear  256x256 → center pixel {p:?} (expect ~[0,153,204,255])");
    let clear_ok = (p[1] as i32 - 153).abs() <= 2 && (p[2] as i32 - 204).abs() <= 2;

    // --- render the shader triangle over IPC and verify it's a gradient ---
    let t = replay.render(RenderRequest::Triangle { width: 256, height: 256, bg: [0.02, 0.02, 0.03, 1.0] })?;
    let center = t.pixel(128, 128);
    let corner = t.pixel(2, 2);
    let bright = center[0].max(center[1]).max(center[2]) > 100;
    let dark = corner[0] < 60 && corner[1] < 60 && corner[2] < 60;
    println!("  triangle 256x256 → center {center:?} (bright), corner {corner:?} (bg)");

    // --- NVML: attribute this process's VRAM (the ADR-0003 payoff) ---
    let mut attributed = None;
    if let Ok(probe) = NvmlProbe::open() {
        // The render forced a Vulkan context in the replay process, so it now holds VRAM.
        for gpu in probe.snapshot_all().unwrap_or_default() {
            if let Ok(procs) = probe.process_vram(gpu.index) {
                if let Some(pv) = procs.iter().find(|p| p.pid == pid) {
                    attributed = pv.used_mb;
                    println!(
                        "  NVML GPU{} attributes pid {} → {} VRAM (per-VM accounting)",
                        gpu.index,
                        pid,
                        pv.used_mb.map(|m| format!("{m} MB")).unwrap_or_else(|| "n/a".into())
                    );
                }
            }
        }
        if attributed.is_none() {
            println!("  NVML: pid {pid} not listed with a graphics context (driver may attribute lazily)");
        }
    } else {
        println!("  NVML unavailable here — skipping attribution check");
    }

    drop(replay); // reaps the process; its GPU context + VRAM go with it

    if !clear_ok {
        return Err("clear render mismatch".into());
    }
    if !(bright && dark) {
        return Err("triangle render did not look like a gradient over background".into());
    }
    println!("\nOK — jailed replay process rendered a clear + shader triangle over IPC; a GPU fault's blast radius is this one process.");
    Ok(())
}
