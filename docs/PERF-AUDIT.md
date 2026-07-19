# 3D-submit performance audit

Audit of the own-remoting 3D hot path
(`guest ICD → DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED → device server (vfio-user) → NVIDIA Vulkan → DMA writeback`).

> For the **reusable method** behind this audit — how to measure, the traps, the hardware truths, the
> technique menu — see [`MICRO-OPT-PLAYBOOK.md`](./MICRO-OPT-PLAYBOOK.md).

**Metric that matters:** the *queue tail* (p99/p999) under **multi-VM** load, as a share of the frame
budget — not a single-VM µs mean. **Golden rule:** the data plane is already zero-copy (guest RAM is a
`memory-backend-memfd,share=on` mapped once; the vfio-user socket carries only control; completion is an
8-byte store in the RingIndices page) — do **not** touch it. The dominant cost is per-submit work.

## Load-bearing reframe: the multi-VM mediation layer is inert in production

infinization spawns **one `infinigpu-device` process per GPU VM** (`InfinigpuDeviceServer`), and
`device.rs::main` calls **`serve()`** (not `serve_with_broker()`), which builds a `GpuBroker` +
`SharedGpu` (its own `VkDevice`) + `run_lock` + `VmConfig("vm",1,4096)` **per process**. `serve_with_broker`
(the shared-broker path) is only used by `broker_demo`.

Consequences:
- The `run_lock` (`infinigpu-sched`) is **per-process**, not fleet-global — near-uncontended in prod
  (one ring thread per VM). Findings **6** and **9** are **latent** (only bite once a shared broker is
  adopted); Fix C has ~0 multi-VM benefit as shipped.
- Admission fail-closed + fair-share + VRAM ledger + concurrency caps are **inert** — each process thinks
  it owns the whole GPU at weight 1, so **VRAM overcommit is never rejected** and there is no cross-tenant QoS.
- Real cross-VM interference flows through: N independent `VkDevice`s hammering the one physical GPU with no
  cooperative serialization, NUMA misplacement, CPU contention, and **N× redundant identical pipeline compiles**.

## Findings (all confirmed), ranked by tail impact in the shipped topology

| # | Finding | Symbol | Fix | Status |
|---|---------|--------|-----|--------|
| 1 | Pipeline+shader compile every submit (`PipelineCache::null`, 2× `create_shader_module`) | `HostGpu::render_triangle_inner` (`infinigpu-replay`) | **A** | ✅ done (`INFINIGPU_PIPELINE_CACHE`) |
| — | *(bug)* broker per-process → admission/fair-share/ledger inert | `serve` / `InfinigpuDeviceServer` | shared host-broker | ⏳ owner decision |
| 7 | Doorbell is a synchronous trapped write → inline replay on the socket thread; vCPU parked the whole submit; ioeventfd rejected; guest retires via `udelay` busy-wait, no MSI-X | `Device::bar0_write_u32`/`submit_vulkan`; `infinigpu.c` | **F** (ioeventfd + IRQ) | ⏳ gated on measurement |
| 8 | NUMA not enforced: device server, memfd, CPU-pinning are GPU-node-oblivious | `InfinigpuDeviceServer.ts`, `QemuCommandBuilder.ts`, `CpuPinningAdapter.ts` | **E** | ⏳ gated on measurement |
| 2 | Per-frame Vulkan alloc churn (`RenderScratch` built+dropped each render) | `render_triangle_inner` | **B** (host) | ✅ done (`INFINIGPU_SCRATCH_CACHE`, off) |
| 8 | NUMA (memfd bind + prealloc, device-server membind) | `QemuCommandBuilder.ts`, `InfinigpuDeviceServer.ts` | **E** | ✅ done (`INFINIGPU_NUMA_NODE`, off) |
| 3 | `dma_alloc_coherent(total_len, GFP_KERNEL)` per submit (fragmentation) | `igpu_ioctl_submit_forwarded` (`guest/linux/infinigpu.c`) | **B** (KMD pool) | 📐 designed — needs a guest build/test env (KMD DMA/concurrency) |
| 4 | ICD re-serializes full SPIR-V + malloc/free per submit | `guest/icd/infinigpu_sync.c`, `infinigpu_forwarded.c` | **B** (ICD payload cache) | 📐 designed — guest build/test env |
| 5 | Two CPU copies of the frame (`map`+`to_vec`, then `dma.write`) + a per-frame heap alloc of the whole frame; dma-buf export unused | `render_forwarded_present` / `render_forwarded_zerocopy`; `submit_vulkan` | **D** | ✅ one-copy (dma-write from the readback mapping) **and** ✅ zero-copy (Fix D: GPU DMAs straight into the imported guest scanout — no CPU copy). Zero-copy gated `INFINIGPU_ZEROCOPY_SCANOUT`, GPU core validated on A5000 (p99 −60%, p999 −86% @1080p); needs full-stack validation — see [`FIX-D-ZEROCOPY.md`](./FIX-D-ZEROCOPY.md) |
| 6 | run_lock over the whole compile+render, inline on the trap thread | `GpuBroker::run` (`infinigpu-sched:590`) | **C** | latent (per-process lock) |
| 9 | Token-bucket `thread::sleep` up to 50ms on the inline thread (before the lock) | `GpuBroker::run` (`infinigpu-sched:583`) | yield/async | latent |

### Correctness bugs fixed (2D/3D), on `main`
- `render_clear` / `convert_present_inner` leaked every Vulkan object on error paths → RAII guards.
- ICD `BindBufferMemory2` dereferenced null `mem->map` → guarded.
- KMD `igpu_resource_register` fbcache key omitted `w` → wrong scanout binding on collision → added `w`.
- `present_scanout_damaged` left a partial (torn) update on a mid-rect DMA failure → read-all-then-write.
- 3D render pass lacked the external `SubpassDependency` (color-write → transfer-read) → added (in Fix A).

Still open: **bind-offset ABI bug** — the render writeback ignores the image's bind offset while the readback
applies it (breaks suballocators/VMA). The real fix adds `bo_offset` to `drm_infinigpu_submit_forwarded` and
threads it host↔guest; needs a GPU test.

## Measured impact (RTX A5000, `bench_forwarded`)

`render_forwarded` (builtin triangle, 256×256) latency. Reproduce with
`cargo run --release -p infinigpu-replay --bin bench_forwarded` (see the bin's header for env).

**Single VkDevice** (2000 iters):

| Config | p50 | p99 | submit/s | cache hit |
|--------|----:|----:|---------:|----------:|
| baseline (`INFINIGPU_PIPELINE_CACHE=0`) | 1764µs | 2268µs | 556 | 0% |
| Fix A (pipeline cache) | 1195µs | 1814µs | 818 | 100% |
| Fix A + cached readback | 784µs | 1226µs | 1211 | 100% |
| **Fix A + Fix B + cached readback** | **119µs** | **185µs (−92%)** | **7396 (13×)** | 100% |

**4 concurrent VkDevices on one GPU = the multi-VM tail** (1500 iters each), fleet **worst** p99:

| Config | worst-VM p99 | % of a 60 Hz frame |
|--------|-------------:|-------------------:|
| baseline (cache off) | 3931µs | 23.6% |
| Fix A + cached readback | 3396µs | 20.4% |
| **Fix A + Fix B + cached readback** | **660µs** | **4.0% (−83%)** |

Key findings, confirmed by a per-phase breakdown (`INFINIGPU_BREAKDOWN=1`):
1. Under multi-VM contention the per-submit **allocation** churn (finding #2) dominates — Fix A alone barely
   moves the fleet worst-p99 (the N processes still contend on the driver allocator); Fix B (allocation-free
   hot path) collapses it. Deploy **both**.
2. The frame **readback copy** was ~72% of a small single-VM frame (221µs for 256 KB) because the readback
   buffer was HOST_COHERENT → write-combined/**uncached** on NVIDIA. Switching it to **HOST_CACHED** (+
   invalidate) cut the copy to ~32µs (−86%) and roughly halves the multi-VM tail again.
3. After those, the remaining single-VM cost is `submit + fence-wait` (~78µs GPU round-trip). Most of that
   is the **sleep+wakeup context switch** of a blocking `vkWaitForFences`, not GPU work — a short spin on
   `vkGetFenceStatus` before blocking removes it (fence-spin, below) without the larger async-submit change (Fix F).

Render output validated identical to the reference (`render_forwarded_matches_builtin`) with all of the above.

### Fence-spin (context-switch elimination)

`HostGpu::wait_fence` spin-polls `vkGetFenceStatus` for up to `INFINIGPU_FENCE_SPIN_US` µs before falling
back to a blocking `vkWaitForFences`. When the GPU finishes within the window (the common case for a small
frame), it skips the ~50–80µs sleep/wakeup entirely. All three readback paths (default 3D, `render_clear`,
`convert_present`) and the Fix-B cached path route through it. Measured on the A5000 (`bench_forwarded`,
256×256, 3000 iters), single VkDevice:

| Path | spin | p50 | p99 | submit/s |
|------|-----:|----:|----:|---------:|
| default (scratch off, alloc-bound) | 0 | 750µs | 1225µs | 1273 |
| default | 100µs | 739µs | **870µs (−29%)** | 1345 |
| **Fix B (scratch on)** | 0 | 145µs | 160µs | 7806 |
| **Fix B** | **100µs** | **78µs (−46%)** | **84µs (−48%)** | **12502 (+60%)** |

The win is largest on the cheap Fix-B path, where the fence-wait is the dominant remaining cost — p99 falls
to **84µs = 0.5% of a 60 Hz frame**. Multi-VM: at 4 VMs the win narrows (worst p99 637→588µs) because the
shared GPU is genuinely busy so the spin usually times out and blocks; at 12 VMs oversubscribed (48-CPU host)
it still helps slightly (2145→1979µs) with no regression. **Caveat:** the spin busy-waits, so on a
CPU-oversubscribed host (VMs > cores) it steals cycles a vCPU could use — hence **default 0 (off)**; enable a
small value (50–100µs) on well-provisioned hosts. Render validated identical with spin on.

### One-copy present (Fix D, host half)

The frame used to be copied **twice** on the CPU: `render_forwarded` `map`+`to_vec`'d the GPU readback into
an owned `Frame` Vec (copy 1 + a per-frame heap alloc of the whole frame), then the device `dma.write`'d that
Vec into the guest scanout (copy 2). `HostGpu::render_forwarded_present` takes a `present: FnOnce(&[u8])`
closure and calls it **once, on the persistent readback mapping, while the cache mutex is held**, so the
device copies straight readback→guest — **one copy, zero per-frame allocation**. It rides the Fix-B scratch
cache (which owns the persistent mapping); with the cache off it falls back to the old render-then-present
(unchanged 2 copies). The win scales with frame size — negligible at 256×256 (fixed overheads dominate,
both copies are cache-hot), decisive at desktop resolutions:

| Res | Path | p50 | p99 | p999 | submit/s |
|-----|------|----:|----:|-----:|---------:|
| 1080p (8 MB) | 2-copy | 3041µs | 3453µs | 9147µs | 324 |
| 1080p | **1-copy** | **2077µs (−32%)** | **2489µs (−28%)** | **3528µs (−61%)** | **471 (+45%)** |
| 720p (3.5 MB) | 2-copy | 1365µs | 1655µs | 3427µs | 725 |
| 720p | **1-copy** | **1110µs (−19%)** | **1180µs (−29%)** | 3124µs | **943 (+30%)** |

**4 VkDevices @1080p (the multi-VM tail):** worst-VM **p99 11816→7658µs (−35%)**, worst **p999 14475→11439µs
(−21%)**, and the per-VM latency distribution **collapses from a wild 5.4–11.8 ms spread to a tight, fair
~7.35 ms** across all VMs. The 2-copy path `mmap`/`munmap`s an 8 MB Vec every frame → page-fault tail spikes
**and** unfairness (one VM raced ahead while the others starved); removing the alloc makes the tail both lower
and predictable. Render validated identical (`render_forwarded_matches_builtin`).

### Where the time is now (phase attribution — the micro-opt surface is exhausted)

After Fix A + Fix B + one-copy present + fence-spin, a per-phase breakdown at **1080p** (`INFINIGPU_BREAKDOWN=1`)
shows only two phases matter; the rest are noise:

| Phase | Solo | 4-VM | Notes |
|-------|-----:|-----:|-------|
| setup | ~8µs | ~8µs | mutex + SPIR-V hash + lookups — **0.4%**, flat under load |
| record | ~22µs | ~23µs | ~11 command-buffer calls — **~0.5%**, flat under load |
| **gpu(submit+wait)** | **~1.29ms** | **~3.92ms (3×)** | dominated by the **8 MB device→host readback DMA over PCIe** + GPU exec; the 3× growth is shared-GPU contention |
| **copy (present)** | **~1.0–1.15ms** | **~0.88ms** | **99.98% CPU memcpy** (8 MB, cache-cold ≈ 7 GB/s); the per-frame `vkInvalidateMappedMemoryRanges` is **216 ns** (a noop on NVIDIA); per-process, does not grow under contention |

So every cheap lever is spent — allocation churn (Fix B), the second CPU copy (one-copy present), the
fence-wait context switch (fence-spin), and the compile (Fix A) are all gone; `setup`+`record`+`invalidate`
are together **<1.5%** and not worth touching. The two remaining costs are **structural**, and each maps to
one of the already-designed architectural fixes:

- **`gpu` (1.3–3.9 ms)** → **Fix F (async submit)**: overlap frame N+1's GPU work with N's readback+copy so the
  PCIe DMA and the copy are hidden behind the next submit. Under multi-VM it is ultimately GPU-contention-bound
  (a shared broker + real serialization is the QoS lever, not a micro-opt).
- **`copy` (~1 ms pure memcpy)** → **Fix D (zero-copy)** — **IMPLEMENTED + core-validated.** Import the guest
  scanout host pointer via `VK_EXT_external_memory_host` and `cmd_copy_image_to_buffer` **straight into guest
  RAM**, removing both the CPU memcpy *and* the intermediate readback buffer. Shell-validated on the A5000
  (byte-identical render + p99 −60%, p999 −86%, ×2.4 throughput @1080p); gated `INFINIGPU_ZEROCOPY_SCANOUT`,
  pending only full-stack (real guest) validation. See [`FIX-D-ZEROCOPY.md`](./FIX-D-ZEROCOPY.md).

`gpu` still needs the owner's host + a decision (async submit / shared broker). This was the honest stop line
for shell-*measurable* micro-opts; Fix D turned out to be shell-*validatable* (import a page-aligned host
buffer standing in for guest RAM), so it got implemented and measured rather than just designed.

## Post-audit fixes (multi-agent 3D-accel audit)

A follow-up 5-lens audit (completeness / host correctness / micro-opts / guest / ABI) surfaced a **live**
throttle that no `bench_forwarded` run could see (the bench drives `HostGpu` directly, bypassing the broker):

- **Token-bucket throttle capped every production VM (fixed).** `GpuBroker::run_timed` debits the render's
  **wall-clock** and refills only 200 ms of GPU-time per wall-second (`infinigpu-sched:185`), so a GPU-bound VM
  was hard-capped at **~20% GPU duty** and slept up to 50 ms **on the vfio-user doorbell-drain thread** (parking
  the vCPU). The bucket enforces *cross-tenant* share, but the shipped topology is **one device process per VM**,
  so it only ever throttled the lone VM it was meant to protect — and under multi-VM it debited
  contention-inflated wall-clock as "usage", amplifying the very tail this audit targets. Fixed: a new
  `BrokerConfig.throttle_enabled` (default on for the shared broker + tests) is set **off** in the per-process
  `broker_config_from_nvml`, so a single-tenant broker never back-pressures its VM (`throttle_disabled_never_blocks_a_lone_vm`).
- **Fix B (`INFINIGPU_SCRATCH_CACHE`) promoted to default-on** — the audit's largest measured win; render
  validated identical on the A5000 with it default-on and with `=0`.
- **Zero-copy fallback + alignment (fixed).** `submit_vulkan` now falls back to the one-copy present (and latches
  zero-copy off) if an import/zero-copy render fails, instead of dropping the frame forever; and
  `supports_zerocopy_scanout` requires `minImportedHostPointerAlignment ≤ 4096` so an import can't reference a
  page past the device-validated region.
- **`memoryOffset` writeback landmine (interim fix).** The guest ICD now advertises `requiresDedicatedAllocation=true`
  so images bind at offset 0 — the host writeback ignores `memoryOffset` while the read paths honor it, which
  would silently corrupt any sub-allocated (VMA) image. Full fix (thread `memoryOffset` through the ioctl) tracked
  in [`3D-COMPLETENESS-ROADMAP.md`](./3D-COMPLETENESS-ROADMAP.md).

3D-accel **completeness** (vertex/index buffers, descriptors/UBO/textures, multi-draw, depth, formats) is a
separate feature track — see [`3D-COMPLETENESS-ROADMAP.md`](./3D-COMPLETENESS-ROADMAP.md).

### Creative micro-opts (both opt-in, both measured)

Two ideas beyond the standard fixes, attacking metrics *adjacent* to the steady-state tail (which is now
architecture-bound: shared broker + async submit):

- **Idle-frame elision** (`INFINIGPU_ELIDE_UNCHANGED`) — skip the whole submit for a byte-identical repeat frame
  (own-remoting forwards the complete input, so identical payload ⇒ the buffer already holds the pixels). Under
  mixed multi-VM load, eliding idle VMs' redundant frames frees the shared GPU and lowers the *active* VMs' tail.
  Hash cost ~0.5–3µs for typical payloads vs a ~1.3ms submit; keyed per scanout_addr (double-buffer safe),
  fail-safe, invalidated on any other scanout write. End-to-end win is workload-dependent (idle-heavy = high).
- **Shared on-disk `VkPipelineCache`** (`INFINIGPU_PIPELINE_CACHE_FILE`) — one blob shared across VM processes,
  warm-starting pipeline creation from other VMs / prior boots (the N× redundant-compile cost). Measured on the
  A5000 (first-render latency, single pipeline): **~6.3× faster (10ms → 1.6ms) even with the NVIDIA driver disk
  cache on** — the driver cache warms the shader *compile*, this warms the *pipeline-object* creation the driver
  cache doesn't cover (they're complementary); ~8.5× (22ms → 2.7ms) with the driver cache off. A real app
  (hundreds of DXVK pipelines) compounds this into the boot-storm cost. Always correct; helps cold-start /
  first-frame, not the steady-state tail.

## Fix A (done)

`HostGpu` owns a real `VkPipelineCache` + a SPIR-V-hash-keyed memo (`GpuObjCache`) of shader modules, render
passes, and pipelines, reused across submits. The pipeline uses **dynamic viewport+scissor** so one entry
serves every resolution. Bounded fail-closed. Gated by `INFINIGPU_PIPELINE_CACHE` (default on; `=0` restores
the per-submit compile path). **Needs render-validation on the A5000** (GPU tests are `#[ignore]`d).

## How to measure (Phase 2)

Instrumentation is opt-in and zero-cost when off. Set on the **device-server env** (inherited from the backend):

- `INFINIGPU_PROFILE=1` — log p50/p99/p999 per hop (`decode`, `runlock_wait`, `render`, `dma_write`, `total`)
  every `INFINIGPU_PROFILE_EVERY` submits (default 300), plus p99 as a share of `INFINIGPU_FRAME_BUDGET_US`
  (default 16667 = one 60 Hz frame). Also logged on VM teardown.
- Pipeline-cache hit rate is logged every 120 submits (`pipeline cache: N hit / M miss (X% hit)`).

**Before/after Fix A on ONE binary, under N concurrent GPU VMs:**

```
INFINIGPU_PROFILE=1 INFINIGPU_PIPELINE_CACHE=0   # baseline: compile every submit
INFINIGPU_PROFILE=1 INFINIGPU_PIPELINE_CACHE=1   # Fix A
```

Compare the `render` and `total` **p99** (and its % of frame). Expect `render` p99 to collapse in steady
state and the cache hit rate to approach 100%.

### All perf flags (each independent; default off unless noted; A/B on one binary)

| Flag | Fix | Effect |
|------|-----|--------|
| `INFINIGPU_PIPELINE_CACHE` (default **on**) | A | Cache pipelines/shaders across submits; `=0` restores per-submit compile |
| `INFINIGPU_SCRATCH_CACHE` (default **on**) | B (host) | Reuse per-(w,h) image/memory/framebuffer/readback (persistent map); `=0` restores the per-frame-alloc path. Needs pipeline cache on |
| `INFINIGPU_NUMA_NODE=<n>` (infinization) | E | Bind guest RAM + device-server CPU/mem to the GPU's NUMA node + prealloc |
| `INFINIGPU_FENCE_SPIN_US=<n>` (default **0**) | — | Spin-poll the fence up to `n` µs before blocking; skips the sleep/wakeup for fast frames. 50–100µs on well-provisioned hosts; leave 0 if VMs > CPU cores |
| `INFINIGPU_ZEROCOPY_SCANOUT=1` (default off) | D | GPU DMAs the frame straight into the imported guest scanout (`VK_EXT_external_memory_host`) — no CPU copy. Needs the scratch cache + a page-aligned mapped scanout; falls back to one-copy present otherwise. Core-validated on A5000; needs full-stack validation |
| `INFINIGPU_ELIDE_UNCHANGED=1` (default off) | — | Skip the whole GPU submit + DMA when a forwarded frame is byte-identical to the last one rendered into the same scanout buffer (own-remoting forwards the complete input, so identical payload ⇒ identical pixels already in the buffer). Frees the shared GPU for busy VMs under mixed multi-VM load. Keyed per scanout_addr (double-buffer safe); invalidated on any non-forwarded scanout write / DMA remap / reset. Fail-safe (a hash collision repaints a stale frame, never UB) |
| `INFINIGPU_PIPELINE_CACHE_FILE=<path>` (default unset) | — | Load/merge a **shared on-disk `VkPipelineCache`** across all VM device processes: warm-start pipeline creation from other VMs (and prior boots), cutting first-frame/boot-storm latency (the N× redundant-compile cross-VM cost). Always correct (the driver ignores a mismatched blob); merged+saved atomically on drop so concurrent writers never lose entries. The device server should point this at a shared host path |

Land each remaining fix (B KMD/ICD, D-full, F) **only** once its own before/after p99 justifies it (golden
rule). The gated fixes above are implemented but need A5000 render-validation + a measured win before their
flags become the default.
