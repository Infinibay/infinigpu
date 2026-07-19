# 3D-submit performance audit

Audit of the own-remoting 3D hot path
(`guest ICD → DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED → device server (vfio-user) → NVIDIA Vulkan → DMA writeback`).

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
| 5 | Two CPU copies of the frame (`map`+`to_vec`, then `dma.write`); dma-buf export unused | `render_triangle_inner`; `submit_vulkan` | **D** | ½ done (Fix B host removes the per-frame re-map); full zero-copy (import guest memfd) is larger |
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
| `INFINIGPU_SCRATCH_CACHE=1` | B (host) | Reuse per-(w,h) image/memory/framebuffer/readback (persistent map); needs pipeline cache on |
| `INFINIGPU_NUMA_NODE=<n>` (infinization) | E | Bind guest RAM + device-server CPU/mem to the GPU's NUMA node + prealloc |

Land each remaining fix (B KMD/ICD, D-full, F) **only** once its own before/after p99 justifies it (golden
rule). The gated fixes above are implemented but need A5000 render-validation + a measured win before their
flags become the default.
