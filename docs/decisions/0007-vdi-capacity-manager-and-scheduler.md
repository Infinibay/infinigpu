# ADR 0007 — VDI-specialized capacity manager & scheduler (the "brain")

- **Status:** accepted
- **Date:** 2026-07-16
- **Feeds from:** research/16-vdi-workload-and-host-scheduler.md, research/17-guest-intelligence-and-video-offload.md, decisions/0003 (topology)

## Context

The owner's explicit directive: **not a generic multiplexer — a specialized, intelligent, capacity-
aware driver for THIS VDI use case.** VDI desktops are mostly-idle with bursts, heterogeneous
(office vs designer), interactive-latency-critical, on 2× A5000 (48 GB VRAM). "Intelligent sharing"
must consider real-time total capacity (VRAM + GPU-time available *now*), persona, and priority — and
degrade gracefully, never hard-fail an interactive desktop.

## Decision

**A per-host cooperative GPU broker running a ~250 ms control loop, driving the three enforceable
knobs (VK_EXT_global_priority, GPU-time token-buckets, VRAM admission caps) with VDI-aware policy;
plus a cooperative guest that suppresses invisible work and offloads video.**

### The scheduling opportunity (the core insight)
Office/task personas are **latency-critical but GPU-time-cheap** (~1.5 GB VRAM, 1–5 % GPU-time);
designers are **GPU-time-heavy but latency-tolerant** (4–12 GB, bursty 40–100 %). "Cheap-but-urgent
vs expensive-but-patient" **is** the scheduling lever: office almost always wins the scheduler;
designers are the natural throttle target.

### Host brain
- **Capacity accounting:** fuse NVML per-process stats (free attribution — each replayer is its own
  process, ADR 0003) + arbiter-measured **Vulkan timestamp** queries (the authoritative scheduling
  currency the token bucket debits) + a broker-owned **VRAM commit ledger** for admission (never race
  the driver's lazy alloc). Maintain a live **FleetView** of what's available now.
- **Admission control** at GPU-attach: check VRAM headroom, department quota, concurrent-GPU-VM cap,
  priority tier; **fail-closed**; queue/deny over capacity; stagger boot storms.
- **Scheduler:** virtual-GPU-time **weighted fair-share** (vruntime, from the fair DRM scheduler) with
  an **interactive/foreground boost**; Best-Effort/Equal/Fixed presets map to token-refill; the
  **token bucket is the hard backstop** (global_priority is only a soft hint — spec guarantees no
  preemption, and NVIDIA caps it at Medium anyway, ADR 0008 SEAM 1).
- **VRAM overcommit:** LRU working-set manager via **VK_EXT_pageable_device_local_memory** +
  `vkSetDeviceMemoryPriorityEXT` (per-alloc eviction priority); soft caps (target) + hard caps
  (guest-visible `VK_ERROR_OUT_OF_DEVICE_MEMORY`). Avoids the K8s time-slice OOM-kill trap.
- **Placement** across the 2 GPUs: pack for VRAM locality, balance load, pin a VM to a GPU, cold-
  rebalance only.
- **Degradation ladder** under contention: spend **fps → bitrate → resolution → foveation strength**
  (ADR 0009), defer non-foreground desktops — **never hard-fail an interactive desktop.**

### Cooperative guest (density is won by doing less)
Idle/no-change suppression, **damage-rect** tracking, adaptive frame pacing, foreground/focus
reporting, HW-cursor plane, present-on-demand; a host↔guest feedback channel (extends the ADR-0004
control ring) where the host advertises budget/priority and the guest throttles itself. **Video
offload to the dedicated NVDEC/NVENC blocks** (separate engines from the 3D SMs) so video never
steals 3D time — the single biggest density multiplier for office VDI.

### Infinibay mapping
6 new `Department` Prisma fields (`gpuEnabled` default false, `vramReserveMB`, `vramCapMB`,
`priorityTier`, `maxConcurrentGpuVMs`, `gpuTimeWeight`); `Machine` already has `gpuPciAddress`/
`departmentId`/`nodeId`; RBAC-gated `attachGpu` mutation → a `GpuBrokerService` singleton (mirrors
`InfinizationService`); telemetry rides the existing Socket.IO health-slice bridge.

## Consequences

- **Positive:** capacity-aware, persona-aware, fail-closed, graceful — exactly the "intelligent,
  specialized" behavior required; reuses Infinibay's RBAC/telemetry plumbing.
- **Negative / accepted / NEEDS VERIFICATION:** persona GPU-time%/fps are **estimates** until Phase-1
  telemetry; per-VM GPU-time attribution has NVML jitter + invisible firmware time-slicing (validate
  the "granted vs actually-ran" loop); 250 ms quantum may be too coarse for strict input-to-photon
  (may need an event-driven foreground fast-path); VRAM page-in stall on demand-restore under boot
  storms is unmodeled.
- **Build order:** (1) accounting/**observe only** first — replace estimated persona numbers with
  measured; (2) admission + VRAM ledger + residency (the fail-closed floor); (3) fair-share +
  foreground boost + degradation last, tuned on real telemetry. Land the 6 Prisma fields + RBAC
  `attachGpu` early (low-risk, gates everything).

## Corrections (review 2026-07-16)

- **QoS is the token bucket, not priority bands.** `VK_EXT_global_priority` is capped at **MEDIUM** on
  NVIDIA (REALTIME denied even to privileged callers), and the **unprivileged** per-VM replay process
  (ADR 0003) cannot request above MEDIUM anyway — so doc 16 §4's REALTIME/HIGH bands do **not** work.
  Use a **MEDIUM-vs-LOW soft hint** only; all hard QoS = **GPU-time token bucket + submission
  back-pressure + a per-submission watchdog**. **No pre-emption of in-flight work on NVIDIA.**
- **NVENC is a first-class admission resource.** GA102 = **1 NVENC block** (not two), shared device-wide
  + a concurrent-session ceiling. For office fleets the binding density limit is **NVENC encode
  throughput, not SM time**. Add encoder-session count to `admit()`; on exhaustion degrade
  (rotate encode → x264 → SPICE), never fail.
- **In-flight GPU-time watchdog.** A never-completing shader is invisible to the token bucket. Add a
  broker watchdog that **kills the replay process** past a per-VM GPU-time budget — beating NVIDIA's RC
  watchdog and pre-empting device-wide-reset escalation.
- **Anti-starvation floor:** minimum guaranteed token-refill per active VM + vruntime aging + cap/decay
  FG_BOOST, so foreground office can't starve a designer indefinitely.
- **Bounded page-in restore:** rate-limit concurrent host-RAM→VRAM restores + account PCIe bandwidth
  (boot storm / post-reset stampede).
- **Explicit reap sequence** on replay-process exit (crash OR stop): free VRAM ledger → tear down
  encoder/NVENC session → release ring/memfd/socket → mark capacity re-admittable. Wire into backend VM
  crash-reconciliation.
- **Timestamp accounting caveat:** raw Vulkan timestamp deltas over-count under contention; reconcile
  vs device busy total, prefer GPM SM-active for busy time.
- **Canonical Prisma set (7):** `gpuEnabled, vramReserveMB, vramCapMB, priorityTier,
  maxConcurrentGpuVMs, gpuTimeWeight, submissionRateTokens` (identical in ADR 0003/0007, docs 10/16).

Full review log: [`../ERRATA.md`](../ERRATA.md). Failure-mode walkthroughs: [`../SCENARIOS.md`](../SCENARIOS.md).
