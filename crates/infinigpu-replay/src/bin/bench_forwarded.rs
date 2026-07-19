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

    // Simulated guest scanout: the device writes the frame here. BENCH_PRESENT selects the datapath:
    //   0 = production two-copy (render_forwarded allocs+copies a Vec, then we copy into scanout)
    //   1 = one-copy present callback (readback→scanout directly)
    //   2 = zero-copy (Fix D): GPU DMAs straight into the imported scanout — no CPU copy at all.
    let mode = env_usize("BENCH_PRESENT", 0);
    // Page-aligned scanout (required for the zero-copy import; harmless for the others).
    let size = (w * h * 4) as usize;
    let align = 4096usize;
    let alloc = size.div_ceil(align) * align;
    let layout = std::alloc::Layout::from_size_align(alloc, align).unwrap();
    // SAFETY: nonzero layout; the process owns this buffer for its whole lifetime (never freed).
    let scan_ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!scan_ptr.is_null(), "alloc scanout");
    let scanout = unsafe { std::slice::from_raw_parts_mut(scan_ptr, size) };
    eprintln!("bench{tag}: mode={mode} (0=two-copy 1=one-copy 2=zero-copy)");
    if mode == 2 && !gpu.supports_zerocopy_scanout() {
        eprintln!("bench{tag}: zero-copy unsupported on this device");
        std::process::exit(2);
    }

    let run_once = |scanout: &mut [u8]| -> bool {
        match mode {
            // SAFETY: scan_ptr is page-aligned and valid for `size` bytes for the whole run.
            2 => unsafe {
                gpu.render_forwarded_zerocopy(w, h, bg, &draw, scanout.as_mut_ptr())
                    .is_ok()
            },
            1 => gpu
                .render_forwarded_present(w, h, bg, &draw, |px| {
                    scanout[..px.len()].copy_from_slice(px);
                    true
                })
                .is_ok(),
            _ => match gpu.render_forwarded(w, h, bg, &draw) {
                Ok(f) => {
                    scanout[..f.rgba.len()].copy_from_slice(&f.rgba);
                    true
                }
                Err(_) => false,
            },
        }
    };

    for _ in 0..warmup {
        run_once(scanout);
    }

    let mut samples: Vec<u64> = Vec::with_capacity(iters);
    let t_all = Instant::now();
    for _ in 0..iters {
        let t = Instant::now();
        let ok = run_once(scanout);
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
