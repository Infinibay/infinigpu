//! # infinigpu-sched — the VDI GPU broker ("the brain", ADR-0007)
//!
//! A per-host cooperative GPU broker that lets many VM desktops share one physical
//! GPU **without MPS and without a per-VM license**. It implements the three
//! enforceable knobs from ADR-0007, in the documented build order (accounting →
//! admission → fair-share):
//!
//! 1. **Admission** at GPU-attach: a broker-owned **VRAM commit ledger** + a
//!    concurrent-GPU-VM cap + a per-VM VRAM cap. **Fail-closed** — over capacity is
//!    denied, never best-effort. Reservation is released by an RAII [`VmTicket`]
//!    (the explicit reap on stop/crash).
//! 2. **GPU-time accounting**: every render is measured and **debited from a per-VM
//!    token bucket** whose refill rate is proportional to the VM's `weight`
//!    (`gpuTimeWeight`). The bucket is the **hard QoS backstop** (ADR-0007
//!    corrections: `VK_EXT_global_priority` is only a soft MEDIUM/LOW hint on NVIDIA,
//!    so all real QoS is the token bucket + submission back-pressure).
//! 3. **Weighted fair-share**: a hog that empties its bucket is throttled (blocked)
//!    until it refills, so a weight-3 VM sustains ~3× the GPU-time of a weight-1 VM.
//!    A per-VM **anti-starvation floor** guarantees the lightest VM still progresses;
//!    a **watchdog** flags a render that overruns its per-VM budget (the real design
//!    kills the per-VM replay *process* — ADR-0003; in-process we can only flag).
//!
//! The broker is **GPU-agnostic**: it never touches Vulkan. Callers pass the actual
//! render as a closure to [`GpuBroker::run`]; the broker serializes it under one GPU
//! run-lock (cooperative multiplexing) and does all accounting around it. That keeps
//! the scheduling logic deterministically unit-testable with a [`ManualClock`] and no
//! GPU — the whole test suite here runs on a machine with no `/dev/dri`.
//!
//! ## Faithful-but-simplified for the Phase-1 first cut
//!
//! ADR-0003's north star is **one jailed replay process per VM** (so NVML gives free
//! per-process attribution and `kill` = reap). This crate validates the *scheduling
//! brain* with all VMs in one process sharing one Vulkan context behind the run-lock.
//! GPU-time is measured as wall-clock of the serialized render (a proxy for the
//! authoritative Vulkan-timestamp currency). Both simplifications are called out in
//! the docs and do not change the admission/fair-share math being proven here.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Microseconds — the broker's single time/GPU-time unit (wall and GPU-time alike).
pub type Micros = u64;

/// Monotonic clock abstraction so the scheduler is deterministically testable.
pub trait Clock: Send + Sync {
    /// Monotonic microseconds since some fixed base.
    fn now_us(&self) -> Micros;
}

/// Real monotonic clock (`Instant`-based).
pub struct MonotonicClock {
    base: Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self {
            base: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now_us(&self) -> Micros {
        self.base.elapsed().as_micros() as Micros
    }
}

/// Test clock: time only advances when you tell it to.
#[derive(Default)]
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    pub fn advance_us(&self, d: Micros) {
        self.now.fetch_add(d, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_us(&self) -> Micros {
        self.now.load(Ordering::SeqCst)
    }
}

/// VDI priority tier (maps to a foreground-boost multiplier on the refill rate and,
/// on real NVIDIA, the soft MEDIUM-vs-LOW `global_priority` hint). Office/interactive
/// desktops are latency-critical but GPU-time-cheap, so they get a boost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityTier {
    /// Foreground interactive desktop — boosted refill.
    Interactive,
    /// Default.
    Normal,
    /// Background / batch — no boost.
    Batch,
}

impl PriorityTier {
    /// Refill multiplier (numerator/256) applied on top of `weight`.
    fn boost_num(self) -> u64 {
        match self {
            PriorityTier::Interactive => 384, // 1.5×
            PriorityTier::Normal => 256,      // 1.0×
            PriorityTier::Batch => 256,       // 1.0× (throttled elsewhere by low weight)
        }
    }
}

/// Per-VM policy (from the 7 `Department` Prisma fields, ADR-0007 §Infinibay mapping).
#[derive(Debug, Clone)]
pub struct VmConfig {
    pub vm_id: String,
    /// `gpuTimeWeight` (≥1). Refill rate ∝ weight → GPU-time share ∝ weight.
    pub weight: u32,
    /// `vramCapMB` — hard per-VM VRAM ceiling.
    pub vram_cap_mb: u64,
    pub priority: PriorityTier,
}

impl VmConfig {
    pub fn new(vm_id: impl Into<String>, weight: u32, vram_cap_mb: u64) -> Self {
        Self {
            vm_id: vm_id.into(),
            weight: weight.max(1),
            vram_cap_mb,
            priority: PriorityTier::Normal,
        }
    }

    pub fn with_priority(mut self, p: PriorityTier) -> Self {
        self.priority = p;
        self
    }

    /// `weight × priority boost`, in /256 fixed point — the **effective weight** that
    /// drives the token refill rate, the throttle-wait, *and* the vruntime yardstick,
    /// so those three never diverge (verify-scheduler finding: they must agree).
    fn effective_weight_num(&self) -> u64 {
        self.weight as u64 * self.priority.boost_num()
    }
}

/// Host-wide broker policy (from the host + `Department` config).
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// Total device VRAM in MB (per GPU; placement across GPUs is a later step).
    pub total_vram_mb: u64,
    /// VRAM held back for the driver/host (never admitted).
    pub vram_reserve_mb: u64,
    /// `maxConcurrentGpuVMs` — hard cap on admitted GPU VMs.
    pub max_concurrent_gpu_vms: u32,
    /// Base token refill: microseconds of GPU-time granted per wall-second, per unit
    /// weight. A weight-`w` VM refills `w * base` µs of GPU budget per wall-second.
    pub refill_us_per_s_per_weight: u64,
    /// Token-bucket burst ceiling (µs of GPU-time a VM may bank while idle).
    pub bucket_burst_us: Micros,
    /// Anti-starvation floor: every admitted VM refills at least this many µs/s,
    /// regardless of weight, so the lightest desktop still makes progress.
    pub min_refill_us_per_s: u64,
    /// A single render exceeding this per-VM GPU-time trips the watchdog (real design
    /// kills the replay process; here it is flagged and counted).
    pub watchdog_us: Micros,
    /// Concurrent hardware-encode (NVENC) sessions the host allows — the scarce ADR-0007
    /// admission resource (a GA102 has a single NVENC block; some driver/SKU combos cap the
    /// concurrent session count). `None` = unlimited (modern drivers lifted the consumer cap):
    /// admission never denies on encoder grounds. `Some(n)` makes an `(n+1)`-th streaming VM a
    /// **fail-closed `NoEncoderSession` denial** instead of a silent black stream (the failure
    /// mode PR8 exists to surface). Set host-wide via env `INFINIGPU_MAX_ENC_SESSIONS`.
    pub max_enc_sessions: Option<u32>,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            total_vram_mb: 24 * 1024, // one A5000
            vram_reserve_mb: 1024,
            max_concurrent_gpu_vms: 16,
            // 200 ms of GPU-time per wall-second per weight unit (i.e. up to 20% of a
            // GPU at weight 1). Tuned so weight ratios show up clearly.
            refill_us_per_s_per_weight: 200_000,
            bucket_burst_us: 50_000,
            min_refill_us_per_s: 20_000,
            watchdog_us: 2_000_000, // 2 s single-submission budget
            // Unlimited by default (matches drivers ≥550, which removed the consumer NVENC
            // session cap). A host on an older driver/SKU sets this to surface the limit.
            max_enc_sessions: None,
        }
    }
}

/// Bytes a single BGRA (`B8G8R8A8`) scanout surface of `width`×`height` occupies, times a
/// working-set `factor` — the PR8 VRAM-accounting input. A per-VM host `ScanoutTarget` keeps a
/// staging buffer **and** a persistent out image (so undamaged pixels survive), plus transient
/// blit scratch, so the live footprint is ~2–3× the raw frame; pass `factor=3` for the honest
/// upper bound. Overflow-safe (a hostile geometry saturates rather than wrapping) and rounded up
/// to whole MB so a sub-MB surface still counts as 1.
pub fn scanout_vram_estimate_mb(width: u32, height: u32, factor: u32) -> u64 {
    const MB: u64 = 1024 * 1024;
    let raw = (width as u64)
        .saturating_mul(height as u64)
        .saturating_mul(4)
        .saturating_mul(factor.max(1) as u64);
    raw.div_ceil(MB)
}

/// An admission request (PR8): how much VRAM the VM reserves, and whether it needs a scarce
/// hardware-encode session. Existing call sites pass a bare `vram_mb` (via `From<u64>`, which
/// sets `needs_encoder: false`); a streaming GPU VM builds one with `needs_encoder: true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmitRequest {
    pub vram_mb: u64,
    /// True if this VM will hold an NVENC session (its infiniPixel stream). Counted against
    /// `BrokerConfig::max_enc_sessions`.
    pub needs_encoder: bool,
}

impl AdmitRequest {
    /// A VRAM-only request (no encoder session) — the pre-PR8 behavior.
    pub fn vram(vram_mb: u64) -> Self {
        Self { vram_mb, needs_encoder: false }
    }
    /// A streaming GPU VM: reserves VRAM **and** one hardware-encode session.
    pub fn streaming(vram_mb: u64) -> Self {
        Self { vram_mb, needs_encoder: true }
    }
}

impl From<u64> for AdmitRequest {
    /// Bare `vram_mb` → a VRAM-only request. Keeps every pre-PR8 `admit(cfg, mb)` call compiling.
    fn from(vram_mb: u64) -> Self {
        Self::vram(vram_mb)
    }
}

/// Why admission was denied (all fail-closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitError {
    /// At `max_concurrent_gpu_vms`.
    AtConcurrencyCap { cap: u32 },
    /// Not enough free VRAM in the ledger.
    InsufficientVram { requested_mb: u64, available_mb: u64 },
    /// Request exceeds this VM's `vram_cap_mb`.
    ExceedsVmCap { requested_mb: u64, cap_mb: u64 },
    /// This vm_id is already admitted.
    AlreadyAdmitted,
    /// All hardware-encode (NVENC) sessions are in use (`cap` concurrent). PR8: surfaced as a
    /// fail-closed denial instead of a silent black stream.
    NoEncoderSession { cap: u32 },
}

impl std::fmt::Display for AdmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmitError::AtConcurrencyCap { cap } => {
                write!(f, "at concurrent-GPU-VM cap ({cap})")
            }
            AdmitError::InsufficientVram {
                requested_mb,
                available_mb,
            } => write!(
                f,
                "insufficient VRAM: requested {requested_mb} MB, {available_mb} MB free"
            ),
            AdmitError::ExceedsVmCap { requested_mb, cap_mb } => {
                write!(f, "requested {requested_mb} MB exceeds VM cap {cap_mb} MB")
            }
            AdmitError::AlreadyAdmitted => write!(f, "VM already admitted"),
            AdmitError::NoEncoderSession { cap } => {
                write!(f, "no free NVENC session ({cap} in use — host encode cap)")
            }
        }
    }
}
impl std::error::Error for AdmitError {}

/// Returned when a VM tries to run without being admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotAdmitted;

/// Why a scheduled [`GpuBroker::run`] did not return a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The VM was never admitted (or was already reaped).
    NotAdmitted,
    /// The render closure panicked. It was **contained** (caught before it could
    /// poison the shared GPU run-lock and brick every other VM), and this VM was still
    /// charged the GPU-time it burned. Fleet unaffected.
    Panicked,
}

/// Result of asking whether a VM may run right now.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Begin {
    /// Bucket has budget — go.
    Ready,
    /// Bucket empty — wait this many wall-µs before retrying.
    Throttled { wait_us: Micros },
}

struct VmState {
    cfg: VmConfig,
    /// GPU-time budget remaining (µs). May dip slightly negative after an overrun.
    tokens_us: i64,
    last_refill_us: Micros,
    /// Whether this VM holds a hardware-encode session (decremented from
    /// `State::enc_sessions_used` on reap).
    holds_encoder: bool,
    // ---- observability (FleetView) ----
    vram_reserved_mb: u64,
    gpu_time_used_us: Micros,
    /// Weighted virtual time (`gpu_time / effective_weight`, i.e. weight × boost) — the
    /// fair-share yardstick, consistent with the share the token bucket enforces.
    vruntime_us: Micros,
    submissions: u64,
    throttle_events: u64,
    watchdog_trips: u64,
}

impl VmState {
    /// Refill the bucket for the wall-time elapsed since the last refill.
    fn refill(&mut self, now: Micros, cfg: &BrokerConfig) {
        let dt = now.saturating_sub(self.last_refill_us);
        if dt == 0 {
            return;
        }
        self.last_refill_us = now;
        let rate = (self.cfg.effective_weight_num() * cfg.refill_us_per_s_per_weight / 256)
            .max(cfg.min_refill_us_per_s); // anti-starvation floor
        let add = (rate as u128 * dt as u128 / 1_000_000) as i64;
        self.tokens_us = (self.tokens_us + add).min(cfg.bucket_burst_us as i64);
    }
}

/// A live snapshot of one VM's capacity/usage — the per-VM row of the FleetView.
#[derive(Debug, Clone)]
pub struct VmStat {
    pub vm_id: String,
    pub weight: u32,
    pub priority: PriorityTier,
    pub vram_reserved_mb: u64,
    pub tokens_us: i64,
    pub gpu_time_used_us: Micros,
    pub vruntime_us: Micros,
    pub submissions: u64,
    pub throttle_events: u64,
    pub watchdog_trips: u64,
}

/// A host-wide capacity snapshot (ADR-0007 "FleetView").
#[derive(Debug, Clone)]
pub struct FleetView {
    pub total_vram_mb: u64,
    pub vram_used_mb: u64,
    pub vram_free_mb: u64,
    pub admitted_vms: u32,
    pub max_concurrent_gpu_vms: u32,
    /// Hardware-encode sessions in use (PR8).
    pub enc_sessions_used: u32,
    /// Host encode-session cap, or `None` when unlimited.
    pub max_enc_sessions: Option<u32>,
    pub vms: Vec<VmStat>,
}

struct State {
    vram_used_mb: u64,
    /// Concurrent hardware-encode sessions currently held (sum of admitted VMs with
    /// `holds_encoder`). Checked against `BrokerConfig::max_enc_sessions` at admit.
    enc_sessions_used: u32,
    vms: HashMap<String, VmState>,
}

/// The cooperative GPU broker. Cheap to `clone` an `Arc` of; share one across all the
/// per-VM device backends.
pub struct GpuBroker {
    cfg: BrokerConfig,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    /// Serializes actual GPU work — one render at a time (cooperative, never MPS).
    run_lock: Mutex<()>,
}

impl GpuBroker {
    pub fn new(cfg: BrokerConfig, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            clock,
            state: Mutex::new(State {
                vram_used_mb: 0,
                enc_sessions_used: 0,
                vms: HashMap::new(),
            }),
            run_lock: Mutex::new(()),
        })
    }

    /// Convenience: a broker on the real monotonic clock.
    pub fn with_real_clock(cfg: BrokerConfig) -> Arc<Self> {
        Self::new(cfg, Arc::new(MonotonicClock::default()))
    }

    pub fn config(&self) -> &BrokerConfig {
        &self.cfg
    }

    /// Admission control at GPU-attach (ADR-0007). Fail-closed: any check that fails
    /// denies the VM. On success the VRAM is reserved in the ledger and the VM is
    /// registered with a full burst bucket; the returned [`VmTicket`] releases both on
    /// drop (the reap).
    ///
    /// `req` is anything `Into<AdmitRequest>`: a bare `vram_mb: u64` reserves VRAM only (the
    /// pre-PR8 behavior), while `AdmitRequest::streaming(mb)` also claims one hardware-encode
    /// session against [`BrokerConfig::max_enc_sessions`] — an over-cap streaming VM is denied
    /// with [`AdmitError::NoEncoderSession`] rather than silently getting a black stream.
    pub fn admit(
        self: &Arc<Self>,
        cfg: VmConfig,
        req: impl Into<AdmitRequest>,
    ) -> Result<VmTicket, AdmitError> {
        let req = req.into();
        let vram_mb = req.vram_mb;
        let now = self.clock.now_us();
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());

        if st.vms.contains_key(&cfg.vm_id) {
            return Err(AdmitError::AlreadyAdmitted);
        }
        if st.vms.len() as u32 >= self.cfg.max_concurrent_gpu_vms {
            return Err(AdmitError::AtConcurrencyCap {
                cap: self.cfg.max_concurrent_gpu_vms,
            });
        }
        if vram_mb > cfg.vram_cap_mb {
            return Err(AdmitError::ExceedsVmCap {
                requested_mb: vram_mb,
                cap_mb: cfg.vram_cap_mb,
            });
        }
        // Hardware-encode session admission (checked BEFORE the ledger is mutated, so a denial
        // leaves the VRAM ledger untouched). `None` = unlimited → never denies.
        if req.needs_encoder {
            if let Some(cap) = self.cfg.max_enc_sessions {
                if st.enc_sessions_used >= cap {
                    return Err(AdmitError::NoEncoderSession { cap });
                }
            }
        }
        let budget = self
            .cfg
            .total_vram_mb
            .saturating_sub(self.cfg.vram_reserve_mb);
        let available = budget.saturating_sub(st.vram_used_mb);
        if vram_mb > available {
            return Err(AdmitError::InsufficientVram {
                requested_mb: vram_mb,
                available_mb: available,
            });
        }

        st.vram_used_mb += vram_mb;
        if req.needs_encoder {
            st.enc_sessions_used += 1;
        }
        let vm_id = cfg.vm_id.clone();
        st.vms.insert(
            vm_id.clone(),
            VmState {
                cfg,
                tokens_us: self.cfg.bucket_burst_us as i64,
                last_refill_us: now,
                holds_encoder: req.needs_encoder,
                vram_reserved_mb: vram_mb,
                gpu_time_used_us: 0,
                vruntime_us: 0,
                submissions: 0,
                throttle_events: 0,
                watchdog_trips: 0,
            },
        );
        log::info!(
            "admit: vm={vm_id} vram={vram_mb}MB enc={} (used {}/{} MB, {} enc, {} VMs)",
            req.needs_encoder,
            st.vram_used_mb,
            budget,
            st.enc_sessions_used,
            st.vms.len()
        );
        Ok(VmTicket {
            broker: Arc::clone(self),
            vm_id,
            released: false,
        })
    }

    /// PR8: revise a VM's VRAM reservation after admission — used once the guest negotiates its
    /// real framebuffer size (admission happens at attach with a baseline estimate; the per-VM
    /// `ScanoutTarget` is only sized at the first present). Fail-closed: if the new reservation
    /// would exceed the free budget it is **rejected** and the old reservation stands (the caller
    /// keeps running on its baseline — never a hard failure, never black). Shrinking always
    /// succeeds. Returns the reservation now in force.
    pub fn adjust_vram(&self, vm_id: &str, new_mb: u64) -> Result<u64, AdmitError> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let old = match st.vms.get(vm_id) {
            Some(vm) => vm.vram_reserved_mb,
            None => return Err(AdmitError::AlreadyAdmitted), // not admitted / already reaped
        };
        if new_mb > old {
            let budget = self.cfg.total_vram_mb.saturating_sub(self.cfg.vram_reserve_mb);
            let available = budget.saturating_sub(st.vram_used_mb);
            let delta = new_mb - old;
            if delta > available {
                return Err(AdmitError::InsufficientVram {
                    requested_mb: delta,
                    available_mb: available,
                });
            }
            st.vram_used_mb += delta;
        } else {
            st.vram_used_mb = st.vram_used_mb.saturating_sub(old - new_mb);
        }
        if let Some(vm) = st.vms.get_mut(vm_id) {
            vm.vram_reserved_mb = new_mb;
        }
        Ok(new_mb)
    }

    /// Refill + check whether `vm_id` may run now. `Ready` if it has budget, else the
    /// wall-µs to wait before it will.
    fn begin(&self, vm_id: &str) -> Result<Begin, NotAdmitted> {
        let now = self.clock.now_us();
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let vm = st.vms.get_mut(vm_id).ok_or(NotAdmitted)?;
        vm.refill(now, &self.cfg);
        if vm.tokens_us > 0 {
            Ok(Begin::Ready)
        } else {
            // Wait until the bucket climbs back to a 1 ms quantum.
            let deficit = (1_000 - vm.tokens_us).max(1) as u128;
            let rate = (vm.cfg.effective_weight_num() * self.cfg.refill_us_per_s_per_weight / 256)
                .max(self.cfg.min_refill_us_per_s)
                .max(1) as u128;
            let wait_us = (deficit * 1_000_000 / rate) as Micros;
            vm.throttle_events += 1;
            Ok(Begin::Throttled {
                wait_us: wait_us.max(1),
            })
        }
    }

    /// Record a completed render: debit GPU-time, advance vruntime, run the watchdog.
    fn record(&self, vm_id: &str, gpu_us: Micros) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(vm) = st.vms.get_mut(vm_id) {
            vm.tokens_us -= gpu_us as i64;
            vm.gpu_time_used_us += gpu_us;
            // vruntime divides by *effective* weight (weight × boost), matching the
            // share the token bucket enforces; full-precision to avoid double rounding.
            vm.vruntime_us += (gpu_us as u128 * 256 / vm.cfg.effective_weight_num() as u128) as u64;
            vm.submissions += 1;
            if gpu_us > self.cfg.watchdog_us {
                vm.watchdog_trips += 1;
                log::warn!(
                    "watchdog: vm={vm_id} render {gpu_us}µs > budget {}µs (real design would kill the replay process)",
                    self.cfg.watchdog_us
                );
            }
        }
    }

    /// Schedule and run one GPU submission for `vm_id`. Blocks under the token-bucket
    /// back-pressure until the VM has budget, then runs `f` under the single GPU
    /// run-lock (cooperative serialization) and debits the measured GPU-time. This is
    /// the call the per-VM device backend wraps every render in.
    pub fn run<R>(&self, vm_id: &str, f: impl FnOnce() -> R) -> Result<R, RunError> {
        loop {
            match self.begin(vm_id).map_err(|_| RunError::NotAdmitted)? {
                Begin::Ready => break,
                Begin::Throttled { wait_us } => {
                    std::thread::sleep(std::time::Duration::from_micros(wait_us.min(50_000)));
                    // Loop: re-check (with a real clock time has advanced).
                }
            }
        }
        // Poison-tolerant: a panic in *any* VM's render must never brick the fleet, so
        // never `.unwrap()` a shared lock (verify-scheduler finding #1).
        let guard = self.run_lock.lock().unwrap_or_else(|e| e.into_inner());
        let t0 = self.clock.now_us();
        // Contain a panicking render to THIS VM: catch_unwind means the run-lock guard
        // drops *normally* (no poison), the hog is still charged the GPU-time it burned,
        // and every other VM keeps running. Real per-VM isolation lands with the
        // ADR-0003 per-VM replay process; this is the in-process backstop until then.
        let result = catch_unwind(AssertUnwindSafe(f));
        let gpu_us = self.clock.now_us().saturating_sub(t0);
        drop(guard);
        self.record(vm_id, gpu_us);
        match result {
            Ok(r) => Ok(r),
            Err(_) => {
                log::error!("render panicked for vm={vm_id}; contained — fleet unaffected");
                Err(RunError::Panicked)
            }
        }
    }

    /// A live capacity snapshot for logging / telemetry.
    pub fn fleet_view(&self) -> FleetView {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let budget = self
            .cfg
            .total_vram_mb
            .saturating_sub(self.cfg.vram_reserve_mb);
        let mut vms: Vec<VmStat> = st
            .vms
            .values()
            .map(|v| VmStat {
                vm_id: v.cfg.vm_id.clone(),
                weight: v.cfg.weight,
                priority: v.cfg.priority,
                vram_reserved_mb: v.vram_reserved_mb,
                tokens_us: v.tokens_us,
                gpu_time_used_us: v.gpu_time_used_us,
                vruntime_us: v.vruntime_us,
                submissions: v.submissions,
                throttle_events: v.throttle_events,
                watchdog_trips: v.watchdog_trips,
            })
            .collect();
        vms.sort_by(|a, b| a.vm_id.cmp(&b.vm_id));
        FleetView {
            total_vram_mb: self.cfg.total_vram_mb,
            vram_used_mb: st.vram_used_mb,
            vram_free_mb: budget.saturating_sub(st.vram_used_mb),
            admitted_vms: st.vms.len() as u32,
            max_concurrent_gpu_vms: self.cfg.max_concurrent_gpu_vms,
            enc_sessions_used: st.enc_sessions_used,
            max_enc_sessions: self.cfg.max_enc_sessions,
            vms,
        }
    }

    /// Internal reap (called by [`VmTicket::drop`]): free the VRAM ledger entry and
    /// deregister the VM so its capacity is immediately re-admittable (ADR-0007).
    fn release(&self, vm_id: &str) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(vm) = st.vms.remove(vm_id) {
            st.vram_used_mb = st.vram_used_mb.saturating_sub(vm.vram_reserved_mb);
            if vm.holds_encoder {
                st.enc_sessions_used = st.enc_sessions_used.saturating_sub(1);
            }
            log::info!(
                "reap: vm={vm_id} freed {}MB enc={} (used {} MB, {} enc, {} VMs)",
                vm.vram_reserved_mb,
                vm.holds_encoder,
                st.vram_used_mb,
                st.enc_sessions_used,
                st.vms.len()
            );
        }
    }
}

/// RAII admission ticket. While held, the VM occupies a concurrency slot + its VRAM
/// reservation; dropping it (stop **or** crash unwind) reaps both. Mirrors the
/// "explicit reap sequence on replay-process exit" in ADR-0007.
pub struct VmTicket {
    broker: Arc<GpuBroker>,
    vm_id: String,
    released: bool,
}

impl VmTicket {
    pub fn vm_id(&self) -> &str {
        &self.vm_id
    }

    /// Run one GPU submission for this VM under the scheduler (see [`GpuBroker::run`]).
    pub fn run<R>(&self, f: impl FnOnce() -> R) -> Result<R, RunError> {
        self.broker.run(&self.vm_id, f)
    }

    /// Revise this VM's VRAM reservation (see [`GpuBroker::adjust_vram`]) — called once the real
    /// framebuffer size is negotiated. Fail-closed: a rejected grow leaves the old reservation.
    pub fn adjust_vram(&self, new_mb: u64) -> Result<u64, AdmitError> {
        self.broker.adjust_vram(&self.vm_id, new_mb)
    }
}

impl std::fmt::Debug for VmTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmTicket")
            .field("vm_id", &self.vm_id)
            .field("released", &self.released)
            .finish()
    }
}

impl Drop for VmTicket {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            self.broker.release(&self.vm_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker_with(clock: Arc<dyn Clock>, cfg: BrokerConfig) -> Arc<GpuBroker> {
        GpuBroker::new(cfg, clock)
    }

    #[test]
    fn admits_until_vram_ledger_is_full_then_fails_closed() {
        let cfg = BrokerConfig {
            total_vram_mb: 10_000,
            vram_reserve_mb: 1_000, // 9000 admittable
            max_concurrent_gpu_vms: 8,
            ..Default::default()
        };
        let b = broker_with(Arc::new(ManualClock::default()), cfg);

        let _a = b.admit(VmConfig::new("a", 1, 8_000), 4_000).unwrap();
        let _c = b.admit(VmConfig::new("c", 1, 8_000), 4_000).unwrap();
        // 8000/9000 used → a 2000 MB request must be denied (only 1000 free).
        let err = b.admit(VmConfig::new("d", 1, 8_000), 2_000).unwrap_err();
        assert_eq!(
            err,
            AdmitError::InsufficientVram {
                requested_mb: 2_000,
                available_mb: 1_000
            }
        );
        assert_eq!(b.fleet_view().vram_free_mb, 1_000);
    }

    #[test]
    fn enforces_concurrency_cap() {
        let cfg = BrokerConfig {
            max_concurrent_gpu_vms: 2,
            ..Default::default()
        };
        let b = broker_with(Arc::new(ManualClock::default()), cfg);
        let _a = b.admit(VmConfig::new("a", 1, 4_000), 1_000).unwrap();
        let _c = b.admit(VmConfig::new("c", 1, 4_000), 1_000).unwrap();
        assert_eq!(
            b.admit(VmConfig::new("d", 1, 4_000), 1_000).unwrap_err(),
            AdmitError::AtConcurrencyCap { cap: 2 }
        );
    }

    #[test]
    fn per_vm_cap_is_enforced() {
        let b = broker_with(Arc::new(ManualClock::default()), BrokerConfig::default());
        assert_eq!(
            b.admit(VmConfig::new("a", 1, 1_500), 2_000).unwrap_err(),
            AdmitError::ExceedsVmCap {
                requested_mb: 2_000,
                cap_mb: 1_500
            }
        );
    }

    #[test]
    fn double_admit_is_rejected() {
        let b = broker_with(Arc::new(ManualClock::default()), BrokerConfig::default());
        let _a = b.admit(VmConfig::new("a", 1, 4_000), 1_000).unwrap();
        assert_eq!(
            b.admit(VmConfig::new("a", 1, 4_000), 1_000).unwrap_err(),
            AdmitError::AlreadyAdmitted
        );
    }

    #[test]
    fn reap_on_drop_frees_capacity() {
        let cfg = BrokerConfig {
            total_vram_mb: 5_000,
            vram_reserve_mb: 0,
            max_concurrent_gpu_vms: 1,
            ..Default::default()
        };
        let b = broker_with(Arc::new(ManualClock::default()), cfg);
        {
            let _a = b.admit(VmConfig::new("a", 1, 4_000), 4_000).unwrap();
            assert_eq!(b.fleet_view().admitted_vms, 1);
            // Slot + VRAM are taken.
            assert!(b.admit(VmConfig::new("b", 1, 4_000), 1_000).is_err());
        }
        // Ticket dropped → capacity is back.
        let fv = b.fleet_view();
        assert_eq!(fv.admitted_vms, 0);
        assert_eq!(fv.vram_used_mb, 0);
        let _b = b.admit(VmConfig::new("b", 1, 4_000), 1_000).unwrap();
    }

    #[test]
    fn running_without_admission_errors() {
        let b = broker_with(Arc::new(MonotonicClock::default()), BrokerConfig::default());
        assert_eq!(b.run("ghost", || 1), Err(RunError::NotAdmitted));
    }

    /// Total GPU-time `vm` can actually **sustain** over `wall_us` of wall-clock —
    /// draining greedily as the bucket refills while the clock advances. This is the
    /// meaningful fair-share measure: the burst cap bounds the *instantaneous* bucket
    /// (identical for all VMs), but throughput over time tracks the weighted refill
    /// rate. Deterministic — no threads, no GPU, manual clock.
    fn sustained_over_us(b: &Arc<GpuBroker>, clock: &ManualClock, vm: &str, wall_us: Micros) -> Micros {
        const STEP: Micros = 10_000; // 10 ms
        const CHUNK: Micros = 100; // 100 µs GPU-time per drained submission
        let end = clock.now_us() + wall_us;
        let mut spent = 0;
        while clock.now_us() < end {
            while let Begin::Ready = b.begin(vm).unwrap() {
                b.record(vm, CHUNK);
                spent += CHUNK;
            }
            clock.advance_us(STEP);
        }
        spent
    }

    #[test]
    fn gpu_time_share_scales_with_weight() {
        let clock = Arc::new(ManualClock::default());
        let cfg = BrokerConfig {
            bucket_burst_us: 10_000, // small burst so it washes out over the window
            refill_us_per_s_per_weight: 100_000,
            min_refill_us_per_s: 0, // isolate the weight law for this test
            ..Default::default()
        };
        let b = broker_with(clock.clone(), cfg);
        let heavy = b.admit(VmConfig::new("heavy", 3, 4_000), 1_000).unwrap();
        let light = b.admit(VmConfig::new("light", 1, 4_000), 1_000).unwrap();

        // Over 10 s of contention each VM sustains GPU-time ∝ its weight (3× vs 1×).
        let bh = sustained_over_us(&b, &clock, heavy.vm_id(), 10_000_000);
        let bl = sustained_over_us(&b, &clock, light.vm_id(), 10_000_000);
        let ratio = bh as f64 / bl as f64;
        assert!(
            (2.9..=3.1).contains(&ratio),
            "weighted fair-share ratio {ratio} (heavy={bh} light={bl}) should be ≈3"
        );
    }

    #[test]
    fn interactive_priority_boosts_share() {
        let clock = Arc::new(ManualClock::default());
        let cfg = BrokerConfig {
            bucket_burst_us: 10_000,
            refill_us_per_s_per_weight: 100_000,
            min_refill_us_per_s: 0,
            ..Default::default()
        };
        let b = broker_with(clock.clone(), cfg);
        // Same weight, different tier: Interactive gets a 1.5× boost.
        let office = b
            .admit(
                VmConfig::new("office", 1, 4_000).with_priority(PriorityTier::Interactive),
                1_000,
            )
            .unwrap();
        let batch = b
            .admit(
                VmConfig::new("batch", 1, 4_000).with_priority(PriorityTier::Batch),
                1_000,
            )
            .unwrap();
        let ratio = sustained_over_us(&b, &clock, office.vm_id(), 10_000_000) as f64
            / sustained_over_us(&b, &clock, batch.vm_id(), 10_000_000) as f64;
        assert!(
            (1.45..=1.55).contains(&ratio),
            "interactive boost ratio {ratio} should be ≈1.5"
        );
    }

    #[test]
    fn anti_starvation_floor_keeps_the_lightest_vm_moving() {
        let clock = Arc::new(ManualClock::default());
        let cfg = BrokerConfig {
            bucket_burst_us: 1_000,
            refill_us_per_s_per_weight: 1, // weight-1 would be ~0 without the floor
            min_refill_us_per_s: 50_000,   // floor guarantees progress
            ..Default::default()
        };
        let b = broker_with(clock.clone(), cfg);
        let tiny = b.admit(VmConfig::new("tiny", 1, 4_000), 1_000).unwrap();
        // Floor: ≥50_000 µs/s × 10 s ⇒ ~500_000 µs of GPU-time must be sustainable.
        assert!(sustained_over_us(&b, &clock, tiny.vm_id(), 10_000_000) >= 400_000);
    }

    #[test]
    fn watchdog_flags_a_runaway_render() {
        let clock = Arc::new(ManualClock::default());
        let cfg = BrokerConfig {
            watchdog_us: 100_000,
            ..Default::default()
        };
        let b = broker_with(clock, BrokerConfig { ..cfg });
        let vm = b.admit(VmConfig::new("hog", 1, 4_000), 1_000).unwrap();
        b.record(vm.vm_id(), 500_000); // 5× the watchdog budget
        let fv = b.fleet_view();
        assert_eq!(fv.vms[0].watchdog_trips, 1);
    }

    #[test]
    fn run_serializes_and_accounts_on_the_real_clock() {
        // Two admitted VMs, one broker: run() must serialize (run-lock) and record
        // GPU-time for each. Uses the real clock + tiny sleeps.
        let b = broker_with(Arc::new(MonotonicClock::default()), BrokerConfig::default());
        let a = b.admit(VmConfig::new("a", 1, 4_000), 1_000).unwrap();
        let out = a
            .run(|| {
                std::thread::sleep(std::time::Duration::from_millis(2));
                42
            })
            .unwrap();
        assert_eq!(out, 42);
        assert_eq!(b.fleet_view().vms[0].submissions, 1);
        assert!(b.fleet_view().vms[0].gpu_time_used_us >= 1_000);
    }

    #[test]
    fn a_panicking_render_is_contained_and_does_not_brick_the_fleet() {
        // verify-scheduler finding #1: a render that panics must NOT poison the shared
        // run-lock and take down every other VM. catch_unwind contains it to this VM.
        let b = broker_with(Arc::new(MonotonicClock::default()), BrokerConfig::default());
        let a = b.admit(VmConfig::new("a", 1, 4_000), 1_000).unwrap();
        let other = b.admit(VmConfig::new("b", 1, 4_000), 1_000).unwrap();

        // 'a' panics inside its render closure.
        let r = a.run(|| -> i32 { panic!("boom in the render") });
        assert_eq!(r, Err(RunError::Panicked));

        // The run-lock is NOT poisoned: another VM — and 'a' itself — still submit fine.
        assert_eq!(other.run(|| 7).unwrap(), 7);
        assert_eq!(a.run(|| 9).unwrap(), 9);
    }

    // ---- PR8: VRAM + NVENC-session admission ------------------------------------------------

    #[test]
    fn encoder_session_admission_is_fail_closed() {
        // A host that caps concurrent NVENC sessions at 1 (the GA102 failure mode PR8 surfaces).
        let cfg = BrokerConfig { max_enc_sessions: Some(1), ..Default::default() };
        let b = broker_with(Arc::new(ManualClock::default()), cfg);

        // First streaming VM claims the single session.
        let a = b.admit(VmConfig::new("a", 1, 4_000), AdmitRequest::streaming(500)).unwrap();
        assert_eq!(b.fleet_view().enc_sessions_used, 1);
        // A second streaming VM is denied-with-reason (not a silent black stream) — and its VRAM
        // ledger is untouched by the denial.
        let before = b.fleet_view().vram_used_mb;
        assert_eq!(
            b.admit(VmConfig::new("b", 1, 4_000), AdmitRequest::streaming(500)).unwrap_err(),
            AdmitError::NoEncoderSession { cap: 1 }
        );
        assert_eq!(b.fleet_view().vram_used_mb, before, "denied admit must not reserve VRAM");
        // A NON-streaming VM still admits (it needs no encode session).
        let _c = b.admit(VmConfig::new("c", 1, 4_000), AdmitRequest::vram(500)).unwrap();
        assert_eq!(b.fleet_view().enc_sessions_used, 1);
        // Reaping the holder frees the session so a new streaming VM can take it.
        drop(a);
        assert_eq!(b.fleet_view().enc_sessions_used, 0);
        let _d = b.admit(VmConfig::new("d", 1, 4_000), AdmitRequest::streaming(500)).unwrap();
        assert_eq!(b.fleet_view().enc_sessions_used, 1);
    }

    #[test]
    fn unlimited_encoder_sessions_never_deny() {
        // Default (None) = modern driver, no consumer NVENC cap → streaming never denies on it.
        let b = broker_with(Arc::new(ManualClock::default()), BrokerConfig::default());
        let mut held = Vec::new();
        for i in 0..5 {
            held.push(
                b.admit(VmConfig::new(&format!("vm{i}"), 1, 4_000), AdmitRequest::streaming(100))
                    .unwrap(),
            );
        }
        let fv = b.fleet_view();
        assert_eq!(fv.enc_sessions_used, 5);
        assert_eq!(fv.max_enc_sessions, None);
    }

    #[test]
    fn bare_u64_admit_claims_no_encoder_session() {
        // The From<u64> path (every pre-PR8 call site) must NOT consume an encode session.
        let cfg = BrokerConfig { max_enc_sessions: Some(1), ..Default::default() };
        let b = broker_with(Arc::new(ManualClock::default()), cfg);
        let _a = b.admit(VmConfig::new("a", 1, 4_000), 1_000).unwrap();
        let _c = b.admit(VmConfig::new("c", 1, 4_000), 1_000).unwrap();
        assert_eq!(b.fleet_view().enc_sessions_used, 0, "vram-only admits hold no session");
    }

    #[test]
    fn adjust_vram_trues_up_and_is_fail_closed() {
        // 9000 MB admittable.
        let cfg = BrokerConfig {
            total_vram_mb: 10_000,
            vram_reserve_mb: 1_000,
            ..Default::default()
        };
        let b = broker_with(Arc::new(ManualClock::default()), cfg);
        // Admit at a baseline, then true up to the real per-VM ScanoutTarget footprint.
        let _a = b.admit(VmConfig::new("a", 1, 9_000), 256).unwrap();
        assert_eq!(b.adjust_vram("a", 2_000).unwrap(), 2_000);
        assert_eq!(b.fleet_view().vram_used_mb, 2_000);
        // A second VM eats most of the rest.
        let _c = b.admit(VmConfig::new("c", 1, 9_000), 6_500).unwrap(); // 8500 used, 500 free
        // Truing 'a' up beyond the free budget is REJECTED and the old reservation stands.
        let err = b.adjust_vram("a", 3_000).unwrap_err(); // needs +1000, only 500 free
        assert!(matches!(err, AdmitError::InsufficientVram { .. }));
        assert_eq!(b.fleet_view().vram_used_mb, 8_500, "rejected true-up must not change the ledger");
        // Shrinking always succeeds and returns capacity.
        assert_eq!(b.adjust_vram("a", 500).unwrap(), 500);
        assert_eq!(b.fleet_view().vram_used_mb, 7_000);
        // Adjusting an unknown VM is rejected, not a panic.
        assert!(b.adjust_vram("ghost", 100).is_err());
    }

    #[test]
    fn scanout_vram_estimate_is_overflow_safe_and_rounds_up() {
        // 1080p BGRA × 3 working-set = 1920*1080*4*3 = 24_883_200 B → 24 MB (ceil).
        assert_eq!(scanout_vram_estimate_mb(1920, 1080, 3), 24);
        // Sub-MB surface still counts as at least 1 MB.
        assert_eq!(scanout_vram_estimate_mb(16, 16, 1), 1);
        // factor 0 is treated as 1 (never zero-cost).
        assert_eq!(scanout_vram_estimate_mb(1920, 1080, 0), scanout_vram_estimate_mb(1920, 1080, 1));
        // A hostile u32::MAX geometry saturates instead of wrapping to a small value.
        assert!(scanout_vram_estimate_mb(u32::MAX, u32::MAX, 3) > 0);
    }
}
