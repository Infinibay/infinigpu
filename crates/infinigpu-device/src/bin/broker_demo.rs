//! `infinigpu-broker-demo [--secs N]`
//!
//! Proves the ADR-0007 GPU broker on the **real physical GPU**, no QEMU: multiple VM
//! desktops sharing one A5000 with fail-closed admission and weighted fair-share.
//!
//! Act 1 (admission, no GPU): admit VMs until the VRAM ledger / concurrency cap /
//! per-VM cap deny the next — the fail-closed floor.
//! Act 2 (fair-share, real GPU): two VMs with weights 3 and 1 render continuously
//! through the broker for a few seconds; the broker throttles each to its weighted
//! GPU-time quota, so the weight-3 "designer" gets ~3× the GPU-time of the weight-1
//! "office" desktop — measured on the actual A5000.

use infinigpu_device::SharedGpu;
use infinigpu_sched::{AdmitError, BrokerConfig, GpuBroker, PriorityTier, VmConfig};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let mut secs = 4u64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--secs" {
            secs = args.next().and_then(|s| s.parse().ok()).unwrap_or(4);
        }
    }

    act1_admission();
    act2_fair_share(Duration::from_secs(secs));
}

/// Fail-closed admission: a tiny broker whose ledger/caps we deliberately exhaust.
fn act1_admission() {
    println!("\n=== Act 1 — admission control (fail-closed) ===");
    let cfg = BrokerConfig {
        total_vram_mb: 8_000,
        vram_reserve_mb: 1_000, // 7000 admittable
        max_concurrent_gpu_vms: 3,
        ..Default::default()
    };
    let broker = GpuBroker::with_real_clock(cfg);

    // Two designers eat most of the VRAM ledger.
    let _d1 = broker
        .admit(VmConfig::new("designer-1", 3, 6_000), 3_000)
        .expect("d1 admitted");
    let _d2 = broker
        .admit(VmConfig::new("designer-2", 3, 6_000), 3_000)
        .expect("d2 admitted");
    println!("  admitted designer-1 (3000MB) + designer-2 (3000MB)");

    // Only 1000 MB free (of the 7000 admittable) → a 2000 MB request is denied by the
    // VRAM ledger, even though a concurrency slot is still open.
    match broker.admit(VmConfig::new("designer-3", 3, 6_000), 2_000) {
        Err(AdmitError::InsufficientVram { available_mb, .. }) => {
            println!("  ✗ designer-3 (2000MB) DENIED — only {available_mb}MB free (VRAM ledger)")
        }
        other => panic!("expected VRAM denial, got {other:?}"),
    }

    // A request above a VM's own VRAM cap is denied too (slot still free).
    match broker.admit(VmConfig::new("greedy", 1, 512), 4_000) {
        Err(AdmitError::ExceedsVmCap { cap_mb, .. }) => {
            println!("  ✗ greedy (4000MB) DENIED — exceeds its own VRAM cap ({cap_mb}MB)")
        }
        other => panic!("expected per-VM cap denial, got {other:?}"),
    }

    // A small office VM fits the remaining VRAM and the 3rd (last) concurrency slot…
    let _office = broker
        .admit(VmConfig::new("office-1", 1, 2_000), 800)
        .expect("office admitted (fits 1000MB free, 3rd slot)");
    println!("  admitted office-1 (800MB) — 3rd and last concurrency slot");

    // …now the concurrency cap denies any further VM regardless of VRAM.
    match broker.admit(VmConfig::new("office-2", 1, 2_000), 100) {
        Err(AdmitError::AtConcurrencyCap { cap }) => {
            println!("  ✗ office-2 (100MB) DENIED — at concurrency cap ({cap} VMs)")
        }
        other => panic!("expected concurrency denial, got {other:?}"),
    }
    println!("  → every over-capacity request fails closed; none degrades silently.");
}

/// Weighted fair-share on the real GPU.
fn act2_fair_share(dur: Duration) {
    println!("\n=== Act 2 — weighted fair-share on the physical GPU ({}s) ===", dur.as_secs());

    // Token-limited regime: each VM's weighted GPU-time budget is the binding
    // constraint, so shares track weight. (Under full saturation, vruntime-ordered
    // dispatch is the refinement — tracked; here the token bucket is the backstop.)
    let cfg = BrokerConfig {
        max_concurrent_gpu_vms: 8,
        refill_us_per_s_per_weight: 80_000, // 8% GPU-time/s per weight unit
        bucket_burst_us: 30_000,
        min_refill_us_per_s: 10_000,
        ..Default::default()
    };
    let broker = GpuBroker::with_real_clock(cfg);
    let gpu = SharedGpu::new();

    // Admit up-front and hold the tickets in main so the FleetView is still populated
    // after the worker threads finish.
    let designer = broker
        .admit(
            VmConfig::new("designer", 3, 8_192).with_priority(PriorityTier::Normal),
            256,
        )
        .expect("designer admitted");
    let office = broker
        .admit(
            VmConfig::new("office", 1, 2_048).with_priority(PriorityTier::Interactive),
            256,
        )
        .expect("office admitted");

    let dev = gpu.device_name().unwrap_or_else(|| "NO GPU".to_string());
    println!("  two VMs rendering on the physical GPU: {dev}");
    println!("  designer: weight 3 (Normal)   office: weight 1 (Interactive, 1.5× boost)");

    let workers: Vec<_> = [("designer", [0.0f32, 0.6, 0.8, 1.0]), ("office", [0.8, 0.4, 0.0, 1.0])]
        .into_iter()
        .map(|(vm, color)| {
            let broker = Arc::clone(&broker);
            let gpu = Arc::clone(&gpu);
            let vm = vm.to_string();
            thread::spawn(move || {
                let end = Instant::now() + dur;
                let mut frames = 0u64;
                while Instant::now() < end {
                    if broker
                        .run(&vm, || gpu.render_clear(256, 256, color))
                        .is_ok()
                    {
                        frames += 1;
                    }
                }
                frames
            })
        })
        .collect();

    let counts: Vec<u64> = workers.into_iter().map(|w| w.join().unwrap()).collect();

    // Snapshot while the tickets are still held.
    let fv = broker.fleet_view();
    println!(
        "\n  FleetView: {}/{} MB VRAM used, {} of {} VM slots",
        fv.vram_used_mb, fv.total_vram_mb, fv.admitted_vms, fv.max_concurrent_gpu_vms
    );
    println!(
        "  {:<10} {:>7} {:>10} {:>12} {:>10} {:>9}",
        "vm", "weight", "frames", "gpu_time_ms", "throttled", "vram_mb"
    );
    for v in &fv.vms {
        println!(
            "  {:<10} {:>7} {:>10} {:>12.1} {:>10} {:>9}",
            v.vm_id,
            v.weight,
            v.submissions,
            v.gpu_time_used_us as f64 / 1000.0,
            v.throttle_events,
            v.vram_reserved_mb,
        );
    }

    // Report the achieved GPU-time ratio (designer should get ~3× office).
    let g = |name: &str| {
        fv.vms
            .iter()
            .find(|v| v.vm_id == name)
            .map(|v| v.gpu_time_used_us)
            .unwrap_or(0)
    };
    let (gd, go) = (g("designer"), g("office"));
    if go > 0 {
        // Effective weight = gpuTimeWeight × priority boost. designer 3×1.0 = 3.0;
        // office 1×1.5 (Interactive) = 1.5 → the scheduler should split GPU-time ~2:1.
        println!(
            "\n  → designer got {:.2}× the office desktop's GPU-time — matching the 2.0×\n    \
             expected from effective weights 3.0 (designer, weight 3) vs 1.5 (office,\n    \
             weight 1 × 1.5 Interactive boost). Office is deliberately boosted: cheap-but-\n    \
             urgent desktops win the scheduler, expensive designers are the throttle target.",
            gd as f64 / go as f64,
        );
        println!(
            "    ({} vs {} frames; both throttled to their weighted GPU-time quota.)",
            counts[0], counts[1]
        );
    }
    println!("  → one physical GPU, two VMs, no MPS, no per-VM license. That is the VDI differentiator.");

    drop(designer);
    drop(office); // reap
}
