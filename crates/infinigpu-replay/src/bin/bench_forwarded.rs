//! Micro-benchmark for the 3D forwarded-submit **render hop** (perf audit, Phase 2/3).
//!
//! Runs `render_forwarded(builtin triangle)` N times and prints p50/p90/p99/p999 of the per-call
//! latency plus the pipeline-cache hit stats. Toggle the fixes via env:
//!   INFINIGPU_PIPELINE_CACHE=0|1  (Fix A, default on)   INFINIGPU_SCRATCH_CACHE=1 (Fix B host)
//! Tune with BENCH_ITERS / BENCH_WARMUP / BENCH_W / BENCH_H / BENCH_TAG.
//!
//! **Multi-VM tail:** each process opens its own VkDevice, so launching N copies concurrently on
//! one physical GPU is the shared-GPU multi-VM scenario — compare each copy's p99 across N.

use infinigpu_replay::{ForwardedDraw, HostGpu};
use std::time::Instant;

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[idx]
}

fn main() {
    let iters = env_usize("BENCH_ITERS", 2000);
    let warmup = env_usize("BENCH_WARMUP", 20);
    let w = env_usize("BENCH_W", 256) as u32;
    let h = env_usize("BENCH_H", 256) as u32;
    let tag = std::env::var("BENCH_TAG").unwrap_or_default();

    let gpu = match HostGpu::open() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("bench{tag}: cannot open GPU: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "bench{tag}: device={} pipeline_cache={} scratch_cache={} iters={iters} {w}x{h}",
        gpu.device_name(),
        std::env::var("INFINIGPU_PIPELINE_CACHE").unwrap_or_else(|_| "on(default)".into()),
        std::env::var("INFINIGPU_SCRATCH_CACHE").unwrap_or_else(|_| "off(default)".into()),
    );

    let draw = ForwardedDraw::builtin_triangle();
    let bg = [0.02f32, 0.02, 0.03, 1.0];

    // Simulated guest scanout: the device dma.writes the frame here. BENCH_PRESENT=1 uses the
    // one-copy present callback (readback→guest directly); default is the production two-copy path
    // (render_forwarded allocs+copies a Vec, then we copy that Vec into the scanout like dma.write).
    let present = env_usize("BENCH_PRESENT", 0) != 0;
    let mut scanout = vec![0u8; (w * h * 4) as usize];
    eprintln!("bench{tag}: present(one-copy)={present}");

    for _ in 0..warmup {
        if present {
            let _ = gpu.render_forwarded_present(w, h, bg, &draw, |px| {
                scanout[..px.len()].copy_from_slice(px);
                true
            });
        } else {
            if let Ok(f) = gpu.render_forwarded(w, h, bg, &draw) {
                scanout[..f.rgba.len()].copy_from_slice(&f.rgba);
            }
        }
    }

    let mut samples: Vec<u64> = Vec::with_capacity(iters);
    let t_all = Instant::now();
    for _ in 0..iters {
        let t = Instant::now();
        let ok = if present {
            gpu.render_forwarded_present(w, h, bg, &draw, |px| {
                scanout[..px.len()].copy_from_slice(px);
                true
            })
            .is_ok()
        } else {
            match gpu.render_forwarded(w, h, bg, &draw) {
                Ok(f) => {
                    scanout[..f.rgba.len()].copy_from_slice(&f.rgba);
                    true
                }
                Err(_) => false,
            }
        };
        let us = t.elapsed().as_micros() as u64;
        if ok {
            samples.push(us);
        }
    }
    let wall = t_all.elapsed();
    std::hint::black_box(&scanout);

    samples.sort_unstable();
    let (hits, misses, cached) = gpu.cache_stats();
    let sum: u128 = samples.iter().map(|&x| x as u128).sum();
    let mean = if samples.is_empty() {
        0
    } else {
        (sum / samples.len() as u128) as u64
    };
    println!(
        "bench{tag}: n={} p50={}us p90={}us p99={}us p999={}us max={}us mean={}us | {:.0} submit/s | cache {}h/{}m ({} cached)",
        samples.len(),
        pct(&samples, 0.50),
        pct(&samples, 0.90),
        pct(&samples, 0.99),
        pct(&samples, 0.999),
        samples.last().copied().unwrap_or(0),
        mean,
        samples.len() as f64 / wall.as_secs_f64().max(1e-9),
        hits,
        misses,
        cached,
    );
}
