# 16 — The Host-Side Brain: a VDI-Specialized GPU Capacity Manager & Scheduler

**Scope.** The core sharing model is decided (ADR 0002: API-remoting; ADR 0003: one
jailed replay process per VM + a per-host broker). Multiplexing is **cooperative** —
we do *not* schedule SMs; NVIDIA firmware time-slices host contexts. This doc designs
the **host brain**: the broker's capacity manager + scheduler, specialized for Infinibay
VDI, not a generic multiplexer. Its controllable knobs are exactly three (ADR 0003 §3.3):
`VK_EXT_global_priority` per-VM context, a token bucket metered by measured GPU-time, and
VRAM admission caps. This doc turns those knobs into a **control loop with concrete inputs,
data structures, policy, and Infinibay wiring**. It is the "intelligent driver, host side"
the owner asked for.

Hardware target: **2× RTX A5000 (GA102), 24 GB VRAM each, 48 GB total.** Density (many
desktops per GPU) is the whole point.

---

## 1. Workload characterization — concrete persona targets

Density planning needs numbers, not adjectives. I anchor **VRAM** in NVIDIA's own vGPU
sizing guides (real, published per-profile framebuffer sizes) and treat **GPU-time %** and
**frame demand** as engineering estimates for cooperative sharing (tagged NEEDS VERIFICATION
— to be replaced by measured telemetry once Phase 1 is live).

| Persona | VRAM working set | VRAM peak | GPU-time (busy) | Frame demand | Latency class | Mix |
|---|---|---|---|---|---|---|
| **(a) Office / task worker** | 0.8–1.5 GB | ~2 GB | 1–5% avg, spikes on video | 30–60 fps, mostly damage-rect | **critical** (input→photon) | 2D compositing + browser + HW video decode |
| **(b) Knowledge / power user** | 1.5–3 GB | ~4 GB | 5–20% | 60 fps multi-monitor | high | more video, multi-mon, light 3D |
| **(c) Designer / CAD / 3D / AI** | 4–12 GB+ | up to hard cap | 40–100% **bursty** | best-effort, tolerates dips | tolerant (batch-ish) | heavy 3D or CUDA bursts |

Grounding for the VRAM column: NVIDIA vPC profiles are **1B = 1 GB** (single/dual HD,
high-density), **2B = 2 GB** (dual QHD or single 4K — the recommended knowledge-worker
baseline), **3B = 3 GB** (added in vGPU 19.0), and designers move to **4 GB / 8 GB** vWS
profiles ([NVIDIA vPC sizing](https://docs.nvidia.com/vgpu/sizing/virtual-pc/latest/overview.html)).
These are our default `vramCapMB` presets per tier.

**The temporal facts that dominate the design** (from VDI operations reality, NEEDS
VERIFICATION against our own fleet): desktops are **mostly idle with bursts**; load is
**diurnal** (working-hours peak); and there are **login/boot storms** (~50–200 VMs start
within minutes at ~9am). The design consequence is decisive: **provision for the aggregate
working set, not the sum of peaks.** With 48 GB and a ~1.5 GB office working set, ~30 office
desktops fit resident; peaks are absorbed by overcommit (§6), not reservation. Office personas
are latency-critical but GPU-time-cheap, so they should almost always win the scheduler;
designers are GPU-time-heavy but latency-tolerant, so they are the natural throttle target.
**This asymmetry — cheap-but-urgent vs. expensive-but-patient — is the whole scheduling
opportunity.**

---

## 2. Live capacity accounting — the real-time fleet view

The broker maintains one authoritative in-memory structure, refreshed on a fixed cadence,
that answers "what is free **right now**?"

```rust
struct FleetView {                       // one per host, RwLock, ~250ms refresh
    gpus: [GpuState; 2],                 // per physical A5000
    vms:  HashMap<VmId, VmGpuState>,
    epoch: u64,                          // monotonically increments each refresh
}
struct GpuState {
    vram_total_mb: u32,                  // 24576
    vram_used_mb:  u32,                  // NVML: nvmlDeviceGetMemoryInfo
    vram_committed_mb: u32,              // sum of resident soft-caps (admission ledger)
    sm_util_pct:   u8,                   // NVML device SM util (short EWMA)
    gpu_busy_ns_window: u64,             // Σ per-VM GPU-time this window (see below)
    encoder_util_pct: u8,                // NVENC session load (present path budget)
    resident_vms:  SmallVec<VmId>,
}
struct VmGpuState {
    gpu: GpuId, dept: DeptId, tier: Tier,
    vram_soft_mb: u32, vram_hard_mb: u32, vram_resident_mb: u32,
    gpu_ns_window: u64,                  // GPU-time consumed this window (authoritative)
    tokens: f64,                         // token-bucket fill (GPU-nanoseconds)
    vruntime_ns: u128,                   // weighted virtual GPU-time (fair-share key)
    foreground: bool,                    // console/stream attached & focused
    last_submit: Instant, state: {Active, Idle, PagedOut},
}
```

**Two measurement sources, fused:**

1. **NVML/DCGM (device truth, coarse).** Per-GPU VRAM used (`nvmlDeviceGetMemoryInfo`),
   device SM/enc/dec utilization, and **per-process** stats via
   `nvmlDeviceGetProcessesUtilizationInfo` (sm, mem, enc, dec) or the newer **GPM**
   (`nvmlGpmMetricsGet`); `nvidia-smi pmon`/DCGM expose the same
   ([NVML](https://developer.nvidia.com/management-library-nvml),
   [NVML GPM workshop](https://dl.acm.org/doi/10.1145/3784828.3785156)). Because each VM's
   replayer is its **own host process** (ADR 0003), per-process = per-VM — NVML gives us
   free per-VM attribution without any guest cooperation.
2. **Arbiter-measured GPU-time (fine, authoritative for scheduling).** Each replayer brackets
   its submissions with **Vulkan timestamp queries** (`vkCmdWriteTimestamp` +
   `VkPhysicalDeviceLimits.timestampPeriod`) and reports `gpu_ns` per completed submission
   batch to the broker over the control ring. This is our scheduling currency — it is not
   subject to NVML's sampling jitter and it is what the token bucket debits.

NVML is polled at ~1 Hz (device totals, drift correction, OOM watch); arbiter GPU-time
flows continuously via ring messages and is aggregated into `gpu_ns_window` each 250 ms tick.
The **VRAM ledger** (`vram_committed_mb`) is *not* read from NVML — it is the broker's own
sum of admitted soft-caps, so admission decisions never race the driver's lazy allocation.

---

## 3. Admission control at GPU-attach (fail-closed)

GPU attach is a gated transition, evaluated by the broker when a VM requests a rendering
context. It is **fail-closed**: any check that cannot be satisfied → queue or deny, never
"attach and hope" (the K8s time-slicing failure mode where processes share the full VRAM
pool with *no boundaries* and one OOMs the other —
[kubenatives](https://www.kubenatives.com/p/mig-vs-time-slicing-vs-mps-which)).

```
admit(vm) →
  policy   = deptPolicy(vm.dept)                         // Prisma, §8
  if !policy.gpuEnabled                     → DENY(policy)
  if deptActiveGpuVMs(vm.dept) >= policy.maxConcurrentGpuVMs → QUEUE(dept_cap)
  gpu = place(vm, policy)                                 // §7; None if no host fits
  if gpu is None:
     if softFull(all_gpus)                  → QUEUE(vram)  // reclaim may free room
     else                                   → DENY(capacity)
  ledger[gpu].vram_committed_mb += vm.vram_soft_mb        // reserve soft-cap
  spawn_replayer(vm, gpu, global_priority = tierToPrio(policy.priorityTier))
  emit gpu:attached  → Socket.IO
```

Headroom test uses the **soft cap** against a reserve line, not physical free VRAM:
`vram_committed + vm.vram_soft ≤ 24576 − vramReserveMB(dept)`. Overcommit (§6) lets *resident*
bytes exceed committed soft-caps transiently, but admission is conservative so the resident
working set has a fighting chance to stay on-GPU. Queued attaches wait in a priority FIFO
(tier, then arrival) and are retried on every reclamation/detach event.

---

## 4. Dynamic scheduling — the cooperative control loop

We cannot preempt inside GA102's runlist. Our schedulable surface is **which VM's queued
submissions we release, at what `global_priority`, and how fast we let each drain** (ADR 0003
§3.3, doc 02 §5). The loop runs every **250 ms** (a "quantum") and re-derives per-VM release
budgets.

**What we borrow, and from where:**

- **Weighted fair-share via virtual GPU-time**, taken directly from the **Fair DRM
  scheduler** (CFS-like: an entity's `vruntime` is accumulated GPU-time divided by its weight;
  least-vruntime runs first; avoids priority starvation) —
  [Fair DRM Scheduler graduated from RFC, Oct 2025](https://www.phoronix.com/news/Fair-DRM-Scheduler-Post-RFC),
  [LWN](https://lwn.net/Articles/1026526/). We use `gpu_ns` (our timestamp measurement) as the
  virtual-time unit and `gpuTimeWeight × tierWeight` as the divisor.
- **The three vGPU policies as *presets*, not mechanism.** NVIDIA's Best-Effort / Equal-Share /
  Fixed-Share ([NVIDIA scheduling](https://docs.nvidia.com/ai-enterprise/release-8/latest/infra-software/vgpu/features/scheduling.html),
  [VxWorld](https://vxworld.co.uk/2025/06/30/understanding-nvidia-vgpu-time-slicing-policies-best-effort-vs-equal-share-vs-fixed-share/))
  map onto our knobs: **Best-Effort** = uncapped token refill, weight-only ordering (max
  density, our default for mixed office fleets); **Equal-Share** = per-active-VM equal token
  rate; **Fixed-Share** = reserved token rate sized by `gpuTimeWeight` regardless of who else
  is active (for a paying "guaranteed" department). Industry guidance matches our asymmetry:
  Fixed-Share for consistency, Best-Effort for density.
- **Token-bucket by measured GPU-time** (K8s/MPS fractional-GPU idea, but metered by *our*
  timestamps, not wall-clock slices). MPS's per-client compute-fraction cap is the reference
  ([NVIDIA GPU Operator sharing](https://docs.nvidia.com/datacenter/cloud-native/gpu-operator/latest/gpu-sharing.html)).
- **Foreground boost + input-to-photon focus** from cloud-gaming schedulers. GeForce NOW targets
  ~30–35 ms click-to-photon and packs **1:8–1:16 players/GPU**, giving each active session a
  slice while idle sessions cost nothing
  ([cloud gaming backend](https://gsb.supercraft.host/blog/cloud-gaming-backends-geforce-now-xcloud/)).
  VDI's win over gaming: most desktops are *idle*, so our achievable density is higher.

**Per-quantum algorithm:**

```
for gpu in gpus:
  active = [vm for vm in gpu.resident if vm.state==Active]
  budget_ns = QUANTUM_NS * headroom(gpu)            // headroom<1 under contention (§7)
  for vm in active:
     w = vm.gpuTimeWeight * TIER_W[vm.tier] * (FG_BOOST if vm.foreground else 1)
                                                     // FG_BOOST ≈ 4
     share = w / Σ active weights
     refill = budget_ns * share
     vm.tokens = min(vm.bucket_max, vm.tokens + refill)   // token bucket
     vm.global_prio = prio(vm)                      // realtime FG-office / high / medium / low
  // release: a replayer may submit while tokens>0; on submit-complete,
  // broker debits vm.tokens -= gpu_ns and adds vm.vruntime += gpu_ns*1e9/w
  // replayers with tokens<=0 are back-pressured (their command ring is not drained)
```

`prio(vm)` maps to **`VK_EXT_global_priority`**: foreground office → `REALTIME` (privileged;
the broker runs as the GPU-group user so the driver honors it), foreground knowledge → `HIGH`,
background/idle → `MEDIUM`, throttled designer under contention → `LOW`. The spec is explicit
that global priority *skews HW allocation* toward the higher queue and *takes precedence over
per-process priority*, though **preemption is not guaranteed**
([VK_EXT_global_priority](https://registry.khronos.org/vulkan/specs/1.3-extensions/man/html/VK_EXT_global_priority.html))
— hence we still need the token bucket as the hard throttle; `global_priority` is the soft
"who the driver favors" hint layered on top.

**Idle reclamation.** A VM with no submission for `IDLE_T` (e.g. 2 s) → `state=Idle`: dropped
from the active weight pool (its share redistributes instantly) and marked a residency
eviction candidate (§6). First submit after idle → immediate re-activate + one-quantum token
grant so wake-up feels instant (the office "click after coffee" case).

---

## 5. VRAM overcommit & residency — the working-set/LRU manager

48 GB will not hold every desktop's peak at once, so VRAM is **overcommitted** and managed as
a working set. The mechanism is Vulkan's pageable memory, which is *built* for exactly a
multi-process oversubscribed GPU: `VK_EXT_pageable_device_local_memory` tells us the OS/driver
pages device-local memory transparently, and `vkSetDeviceMemoryPriorityEXT` +
`VK_EXT_memory_priority` let us set a **0.0–1.0 priority per allocation** so the driver evicts
*low-priority allocations first* under pressure and pages high-priority back in first
([Khronos pageable memory](https://registry.khronos.org/vulkan/specs/1.3-extensions/man/html/VK_EXT_pageable_device_local_memory.html),
[VMA memory_priority](https://gpuopen-librariesandsdks.github.io/VulkanMemoryAllocator/html/vk_ext_memory_priority.html),
[NVIDIA Vulkan dos/don'ts](https://developer.nvidia.com/blog/vulkan-dos-donts/)).

**Broker → driver priority mapping (per replayer allocation):**

```
mem_priority(vm, alloc) =
   1.0   if vm.foreground and alloc.role in {swapchain, scanout, active_framebuffer}
   0.7   if vm.state==Active
   0.3   if vm.state==Idle
   0.05  if vm.state==PagedOut (evict-me-first)
```

Residency ledger (per GPU): an **LRU list of resident VM allocation sets**, keyed by
`last_submit`. The manager runs on every quantum and on `vram_used` crossing a high-water mark:

```
while gpu.vram_used_mb > HIGH_WATER(gpu):        // e.g. 22 GB of 24
   victim = lru.idle_tail()                       // oldest Idle VM, never foreground
   if victim is None: break                       // all resident VMs active → degrade (§7)
   demote(victim): set all its allocs priority→0.05, state=PagedOut,
                   let driver page it to host RAM; keep guest handles valid
   emit vram:evicted(victim)
```

Restore is **demand-driven**: a PagedOut VM's next submit raises priorities back to Active
and the driver pages it in; the broker grants a short grace quantum (no token debit) to hide
the page-in stall. **Soft cap** (`vramReserveMB`-derived) governs admission and the target
resident size; **hard cap** (`vramCapMB`) is enforced at allocation time — a replayer request
that would exceed hard cap is **refused to the guest** (Vulkan `VK_ERROR_OUT_OF_DEVICE_MEMORY`,
translated by our guest ICD) rather than allowed to blow the ledger. **Graceful refusal when
truly full:** if no idle victim exists and every resident VM is foreground-active, we do *not*
thrash-page; we hold new large allocations and shed via the degradation ladder (§7). This is
the deliberate opposite of the K8s time-slice trap where unbounded VRAM sharing OOM-kills a
tenant.

---

## 6. Multi-GPU placement across the 2× A5000

Placement is a **cold** decision (at attach). No live GPU migration — a VM is **pinned** to one
A5000 for its session (its replayer context and VRAM live there). `place(vm, policy)`:

```
candidates = [g for g in gpus if fits(g, vm.vram_soft, policy.vramReserveMB)
                              and activeGpuVMs(g) < GPU_VM_SOFT_CAP]
rank by:  1) VRAM-locality  — prefer the GPU already holding this dept's VMs
                              (shared textures/golden-image pages amortize; pack for locality)
          2) load-balance   — then least (gpu_busy_ns_window + vram_committed) 
pick highest-ranked; None → admission QUEUE/DENY
```

Two objectives intentionally fight: **pack** department-affine VMs together (locality, future
shared-resource dedupe) but **balance** so one A5000 isn't saturated while the other idles. We
resolve by making locality a tie-first bias and load the ranking key — dense-but-not-hot.
**Cold rebalancing only:** a periodic (or admin-triggered) evaluator may, when one GPU is
chronically hot, mark idle VMs on it for **re-pin on next cold start** (VM stop/start or session
reconnect re-runs `place`) — never a live move. Boot storms (§7) are the main rebalancing lever:
staggered attach naturally spreads new VMs across both GPUs by the load key.

---

## 7. Graceful degradation under contention & login storms

**Never hard-fail an interactive desktop.** Under contention we spend *quality*, not
*availability*. A monotonic **degradation ladder**, applied worst-first to the least-latency-
sensitive VMs, driven by a contention signal `C = f(Σ gpu_busy, vram_pressure, encoder_util)`:

| Rung | Trigger | Action (target: background/designer first, foreground office last) |
|---|---|---|
| 0 | normal | full fps / native res / high encode quality |
| 1 | C rising | cap **background** desktops to 30 fps; designers → `global_priority=LOW` |
| 2 | C high | drop **encode bitrate/quality** on non-foreground; token budget `headroom<1` |
| 3 | VRAM high-water | page out idle VMs (§6); defer new large allocations |
| 4 | severe | lower **resolution** of non-foreground streams; frame-drop batch 3D |
| 5 | admission full | **queue** new GPU attaches (never OOM a resident VM) |

Foreground office desktops are the last thing touched and the first restored as `C` falls.
This mirrors cloud-gaming's "protect the interactive frame budget" posture and the vGPU
short-time-slice-for-latency guidance (shorter slices favor latency-sensitive graphics
[VxWorld](https://vxworld.co.uk/2025/06/30/understanding-nvidia-vgpu-time-slicing-policies-best-effort-vs-equal-share-vs-fixed-share/)).

**Login/boot storm control.** ~9am many VMs boot at once, each initializing its GPU context,
allocating a swapchain, and compositing a first frame — a synchronized VRAM + init spike. The
broker runs a **boot-storm admission gate**: a token-bucket-limited *attach* rate (e.g. ≤ N
concurrent GPU-init handshakes per GPU, the rest QUEUE), so context creation is **staggered**
over tens of seconds instead of stampeding. First-frame allocations get a brief priority boost
so the login screen paints fast, then settle to Idle priority. Because early-login desktops are
idle-heavy, staggering costs the user only sub-second extra paint latency while preventing a
transient over-commit that would page-thrash the whole host.

---

## 8. Mapping to Infinibay

**Prisma — department GPU policy** (new fields on `Department`; the `Machine` model already
carries `gpuPciAddress`, `departmentId`, `nodeId`, so per-VM attach state fits there):

```prisma
model Department {
  // ... existing fields ...
  gpuEnabled          Boolean @default(false)   // master gate; admission DENY if false
  vramReserveMB       Int     @default(0)       // headroom kept free per GPU for this dept
  vramCapMB           Int     @default(2048)    // hard per-VM cap (2B baseline); designers 8192
  priorityTier        String  @default("standard") // "interactive" | "standard" | "batch"
  maxConcurrentGpuVMs Int     @default(8)       // department concurrency cap (admission)
  gpuTimeWeight       Int     @default(100)     // fair-share weight (CFS-like divisor)
}
model Machine {
  // ... existing fields (gpuPciAddress, departmentId, nodeId) ...
  gpuAttached   Boolean @default(false)   // broker-owned; true while a replayer holds a context
  gpuId         Int?                      // which physical A5000 (0/1) it is pinned to
  vramResidentMB Int?                     // last telemetry sample (denormalized for UI)
}
```

Per-VM overrides (a specific designer VM needing 12 GB) can live in a small `GpuAttachment`
side table or a JSON column, resolved as **VM override > department policy > default** — the
same precedence pattern infinization already uses for config (doc CONFIGURATION.md).

**RBAC-gated attach.** GPU attach is a new backend GraphQL mutation
(`attachGpu(machineId)`), a thin `type-graphql` resolver that (1) checks the caller's
department role via the existing `DepartmentMembership`/authChecker path (MANAGER+ or global
ADMIN to attach; MEMBER may attach only their own desktop within `maxConcurrentGpuVMs`), then
(2) delegates to a new `GpuBrokerService` singleton (mirrors `InfinizationService`) that calls
the host broker's `admit()`. The resolver never touches the GPU — it validates + delegates, per
backend's strict layering. Detach on VM stop/delete flows through the existing lifecycle hooks.

**Live telemetry over the existing path.** The broker already produces `FleetView` at ~4 Hz.
It pushes deltas to the backend, which (a) persists coarse samples to Postgres (a
`GpuMetric`/`MachineHealth`-style table on the health-queue cadence, not at 4 Hz) and (b)
re-emits over **Socket.IO** on a `gpu:metrics` channel into the frontend health slice — the
identical `realTimeReduxService` bridge that live VM CPU/RAM metrics already use (MEMORY:
"Live VM metrics wiring"). The frontend Overview/department views render per-GPU VRAM bars,
per-VM GPU-time, and admission/queue status with zero new transport. Multi-node caveat (MEMORY:
"Multi-node control-plane realtime"): only the master runs Socket.IO/EventManager, so a
node-hosted broker reports up to the master over the existing node RPC channel before fan-out.

---

## 9. The control loop, at a glance

```
every 250 ms (per host broker):
  1. INGEST   : drain arbiter gpu_ns reports → vm.gpu_ns_window; poll NVML @1Hz (vram/util drift)
  2. ACCOUNT  : recompute FleetView.epoch (per-GPU vram_used/committed, busy_ns, active sets)
  3. RESIDENCY: LRU demote idle VMs while vram_used>HIGH_WATER; set Vulkan mem priorities
  4. SCHEDULE : per active VM → weight → token refill → global_priority; back-pressure tokens<=0
  5. DEGRADE  : compute contention C → apply/lift ladder rungs (fps/bitrate/res/defer)
  6. ADMIT    : service attach queue (retry placement); run boot-storm rate gate
  7. EMIT     : Socket.IO gpu:metrics + gpu:attached/evicted/queued events
```

Inputs consumed: arbiter Vulkan timestamps (authoritative GPU-time), NVML per-process +
device VRAM/SM/enc util, the broker's own VRAM commit ledger, per-VM foreground flag (from the
present/console path), and department policy from Postgres. Outputs: token budgets +
`global_priority` per replayer, Vulkan memory priorities, admission verdicts, degradation
settings, and telemetry. **Everything is cooperative and ownable** — no NVIDIA scheduler
internals touched, exactly per ADR 0003.

---

## Sources

- NVIDIA vGPU scheduling policies (Best-Effort / Equal-Share / Fixed-Share): https://docs.nvidia.com/ai-enterprise/release-8/latest/infra-software/vgpu/features/scheduling.html
- VxWorld — Understanding NVIDIA vGPU time-slicing policies (latency vs throughput, VDI guidance): https://vxworld.co.uk/2025/06/30/understanding-nvidia-vgpu-time-slicing-policies-best-effort-vs-equal-share-vs-fixed-share/
- NVIDIA vPC Sizing — profile framebuffer sizes (1B/2B/3B, KW baseline): https://docs.nvidia.com/vgpu/sizing/virtual-pc/latest/overview.html
- NVIDIA vGPU sizing FAQ (2 GB PoC start, 4/8 GB for demanding): https://docs.nvidia.com/vgpu/faq/latest/sizing.html
- NVIDIA RTX vWS sizing methodology (designer/CAD): https://docs.nvidia.com/vgpu/sizing/virtual-workstation/latest/methodology.html
- Kubernetes GPU sharing — Time-Slicing / MPS / MIG, VRAM-boundary OOM trap: https://www.kubenatives.com/p/mig-vs-time-slicing-vs-mps-which
- NVIDIA GPU Operator — Time-Slicing GPUs in Kubernetes (oversubscription): https://docs.nvidia.com/datacenter/cloud-native/gpu-operator/latest/gpu-sharing.html
- Spheron — MIG / Time-Slicing / MPS guide (per-client memory+compute fraction): https://www.spheron.network/blog/run-multiple-llms-one-gpu-mig-time-slicing-guide/
- Fair DRM Scheduler graduates out of RFC (CFS-like, vruntime, virtual GPU time) — Phoronix, Oct 2025: https://www.phoronix.com/news/Fair-DRM-Scheduler-Post-RFC
- LWN — Fair DRM scheduler: https://lwn.net/Articles/1026526/
- Cloud gaming backends explained — GeForce NOW/xCloud input-to-photon budget, 1:8–1:16 density: https://gsb.supercraft.host/blog/cloud-gaming-backends-geforce-now-xcloud/
- Counterpoint — GeForce NOW RTX 5080 / 48 GB framebuffer, low-latency streaming: https://counterpointresearch.com/en/insights/nvidia-geforce-now-rtx-5080-low-latency--gaming-pc-experience-in-the-cloud
- Khronos — VK_EXT_pageable_device_local_memory (pageable device-local, overcommit, vkSetDeviceMemoryPriorityEXT): https://registry.khronos.org/vulkan/specs/1.3-extensions/man/html/VK_EXT_pageable_device_local_memory.html
- Vulkan Memory Allocator — VK_EXT_memory_priority (0..1 eviction priority): https://gpuopen-librariesandsdks.github.io/VulkanMemoryAllocator/html/vk_ext_memory_priority.html
- NVIDIA — Vulkan Dos and Don'ts (use pageable memory + priority to avoid demoting critical resources): https://developer.nvidia.com/blog/vulkan-dos-donts/
- Khronos — VK_EXT_global_priority (LOW/MEDIUM/HIGH/REALTIME, skews HW allocation, precedence, no preemption guarantee, privilege): https://registry.khronos.org/vulkan/specs/1.3-extensions/man/html/VK_EXT_global_priority.html
- NVIDIA Management Library (NVML): https://developer.nvidia.com/management-library-nvml
- NVML GPM metrics for GPU monitoring (nvmlGpmMetricsGet, per-process sm/mem/enc/dec): https://dl.acm.org/doi/10.1145/3784828.3785156
- DigitalOcean — Monitoring GPU utilization in real time (nvidia-smi pmon / --loop): https://www.digitalocean.com/community/tutorials/monitoring-gpu-utilization-in-real-time
