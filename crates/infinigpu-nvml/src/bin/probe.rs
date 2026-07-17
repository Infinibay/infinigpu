//! `infinigpu-nvml-probe` — print real GPU capacity + per-process VRAM via NVML.
//!
//! Proves the ADR-0003/0007 measurement path on real silicon: the numbers the broker
//! should admit against (free VRAM, encoder sessions) instead of config guesses.

use infinigpu_nvml::NvmlProbe;

fn main() {
    let probe = match NvmlProbe::open() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("NVML unavailable (no NVIDIA driver here?): {e}");
            std::process::exit(1);
        }
    };
    let snaps = match probe.snapshot_all() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("NVML query failed: {e}");
            std::process::exit(1);
        }
    };
    println!("infinigpu-nvml — {} GPU(s)", snaps.len());
    for s in &snaps {
        let sessions = s
            .encoder_sessions
            .map(|n| n.to_string())
            .unwrap_or_else(|| "n/a".into());
        println!(
            "  GPU{} {:<22} VRAM {:>6}/{:>6} MB free ({} used) | util gpu {}% mem {}% | \
             enc-sessions {} | gfx-procs {}",
            s.index, s.name, s.free_mb, s.total_mb, s.used_mb, s.gpu_util_pct, s.mem_util_pct,
            sessions, s.graphics_processes,
        );
        println!(
            "       broker-usable VRAM (free − 1024 MB reserve): {} MB",
            s.usable_vram_mb(1024)
        );
        match probe.process_vram(s.index) {
            Ok(procs) if !procs.is_empty() => {
                for p in procs {
                    let mb = p.used_mb.map(|m| format!("{m} MB")).unwrap_or_else(|| "n/a".into());
                    println!("       pid {:<7} → {}", p.pid, mb);
                }
            }
            Ok(_) => println!("       (no per-process graphics contexts right now)"),
            Err(e) => println!("       per-process attribution unavailable: {e}"),
        }
    }
}
