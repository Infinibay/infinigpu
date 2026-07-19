//! Opt-in per-submit latency profiling for the 3D hot path (env `INFINIGPU_PROFILE=1`).
//!
//! Zero-cost when off: the backend holds an `Option<SubmitProfiler>` that is `None` unless the
//! env var is set, so the hot path pays nothing (one `Option::is_some`-style branch) in
//! production. When on, each submit's per-hop durations are folded into fixed-boundary µs
//! histograms and p50/p99/p999 are logged periodically — the multi-VM **queue-tail** breakdown
//! the performance audit targets, reported as absolute µs *and* as a share of the frame budget
//! (env `INFINIGPU_FRAME_BUDGET_US`, default 16667 = one 60 Hz frame).
//!
//! Dependency-free by design (this crate stays lean): a coarse bucketed histogram, not HdrHistogram.
//! The device backend serves one VM on a single thread, so no locking is needed here.

use log::info;

/// Upper bounds (µs) of each histogram bucket; a sample lands in the first bucket whose bound is
/// `>= sample`. Anything larger falls in an implicit overflow bucket (reported via `max`). Chosen
/// dense around the 60/30 Hz frame budgets where the tail matters, out to the ~2 s cold-init case.
const BOUNDS_US: &[u64] = &[
    25, 50, 100, 200, 350, 500, 750, 1_000, 1_500, 2_000, 3_000, 4_000, 6_000, 8_000, 11_000,
    16_667, 22_000, 33_333, 50_000, 75_000, 100_000, 200_000, 350_000, 500_000, 1_000_000, 2_000_000,
];

#[derive(Clone)]
struct Histogram {
    /// `BOUNDS_US.len() + 1` counters (last = overflow).
    buckets: Vec<u64>,
    count: u64,
    sum_us: u128,
    max_us: u64,
}

impl Histogram {
    fn new() -> Self {
        Histogram { buckets: vec![0; BOUNDS_US.len() + 1], count: 0, sum_us: 0, max_us: 0 }
    }

    fn record(&mut self, us: u64) {
        let idx = BOUNDS_US.iter().position(|&b| us <= b).unwrap_or(BOUNDS_US.len());
        self.buckets[idx] += 1;
        self.count += 1;
        self.sum_us += us as u128;
        if us > self.max_us {
            self.max_us = us;
        }
    }

    /// Upper-bound estimate of the `p`-quantile (0.0..=1.0): the bound of the bucket that holds it.
    fn pct(&self, p: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = ((self.count as f64) * p).ceil() as u64;
        let mut acc = 0u64;
        for (i, &b) in self.buckets.iter().enumerate() {
            acc += b;
            if acc >= target {
                return BOUNDS_US.get(i).copied().unwrap_or(self.max_us);
            }
        }
        self.max_us
    }

    fn mean_us(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            (self.sum_us / self.count as u128) as u64
        }
    }
}

/// One submit's per-hop timing (µs). `wait`/`render` come from the broker's `RunStats`.
#[derive(Clone, Copy, Default)]
pub struct HopSample {
    pub decode_us: u64,
    pub wait_us: u64,
    pub render_us: u64,
    pub dma_us: u64,
    pub total_us: u64,
}

/// Accumulates per-hop histograms for one VM and flushes p50/p99/p999 every `flush_every` submits.
pub struct SubmitProfiler {
    vm_id: String,
    frame_budget_us: u64,
    flush_every: u64,
    since_flush: u64,
    decode: Histogram,
    wait: Histogram,
    render: Histogram,
    dma: Histogram,
    total: Histogram,
}

impl SubmitProfiler {
    /// `Some` iff `INFINIGPU_PROFILE` is set. `INFINIGPU_PROFILE_EVERY` (default 300) sets the
    /// flush cadence; `INFINIGPU_FRAME_BUDGET_US` (default 16667) sets the % baseline.
    pub fn from_env(vm_id: &str) -> Option<Self> {
        if std::env::var_os("INFINIGPU_PROFILE").is_none() {
            return None;
        }
        let flush_every = std::env::var("INFINIGPU_PROFILE_EVERY")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(300);
        let frame_budget_us = std::env::var("INFINIGPU_FRAME_BUDGET_US")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(16_667);
        info!(
            "profile: per-submit latency profiling ON for vm={vm_id} (flush every {flush_every} submits, frame budget {frame_budget_us}µs)"
        );
        Some(SubmitProfiler {
            vm_id: vm_id.to_string(),
            frame_budget_us,
            flush_every,
            since_flush: 0,
            decode: Histogram::new(),
            wait: Histogram::new(),
            render: Histogram::new(),
            dma: Histogram::new(),
            total: Histogram::new(),
        })
    }

    pub fn record(&mut self, s: HopSample) {
        self.decode.record(s.decode_us);
        self.wait.record(s.wait_us);
        self.render.record(s.render_us);
        self.dma.record(s.dma_us);
        self.total.record(s.total_us);
        self.since_flush += 1;
        if self.since_flush >= self.flush_every {
            self.flush();
        }
    }

    /// Log cumulative p50/p99/p999 per hop. Histograms are cumulative since process start, so the
    /// final line is a whole-run summary (useful for a stable before/after Fix-A comparison).
    pub fn flush(&mut self) {
        self.since_flush = 0;
        let n = self.total.count;
        if n == 0 {
            return;
        }
        let fb = self.frame_budget_us as f64;
        let line = |name: &str, h: &Histogram| -> String {
            let p99 = h.pct(0.99);
            format!(
                "{name} p50={}µs p99={}µs p999={}µs max={}µs mean={}µs (p99={:.0}% frame)",
                h.pct(0.50),
                p99,
                h.pct(0.999),
                h.max_us,
                h.mean_us(),
                (p99 as f64) / fb * 100.0,
            )
        };
        info!(
            "profile vm={} n={n} | {} | {} | {} | {} | {}",
            self.vm_id,
            line("total", &self.total),
            line("decode", &self.decode),
            line("runlock_wait", &self.wait),
            line("render", &self.render),
            line("dma_write", &self.dma),
        );
    }
}

impl Drop for SubmitProfiler {
    fn drop(&mut self) {
        // Final whole-run summary on VM teardown.
        self.flush();
    }
}
