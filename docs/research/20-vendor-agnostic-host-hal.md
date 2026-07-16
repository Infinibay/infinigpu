# 20 — Vendor-Agnostic Host GPU Abstraction Layer (the `GpuBackend` HAL)

**Scope:** design the Rust `GpuBackend` trait that lets infinigpu's host replay/arbiter run on
**NVIDIA, AMD, and Intel** GPUs, and draw the honest line between what is genuinely cross-vendor
and what needs per-vendor code. We test now on 2× **NVIDIA RTX A5000 (GA102, Ampere)**, but the
architecture must accept AMD (RADV/AMDVLK) and Intel (ANV/Arc) host drivers as *backend impls*,
not a rearchitecture.

## Bottom line up front

The core design decision — **API-remoting onto a headless host Vulkan context** (ADR-0002) — is
what makes vendor-agnosticism nearly free. The guest talks Vulkan/D3D to *our* vfio-user device;
the host replays on whatever host Vulkan driver is present. **The guest never sees the physical
GPU**, so there is no per-vendor guest driver and no vendor passthrough. Vulkan is cross-vendor by
construction (NVIDIA proprietary, AMD RADV/AMDVLK, Intel ANV are all Vulkan 1.3/1.4 conformant).

The vendor-specific residue is small and *concentrated at four seams*: (1) **dma-buf/DRM-modifier
export** (NVIDIA is the weak, late mover; Mesa is native), (2) **submission priority** (NVIDIA's
Linux driver caps global priority at *medium* — a real regression — while RADV/ANV honor
realtime), (3) **fault/reset granularity** (AMD/Intel now do per-queue/per-engine reset; GA102
still escalates severe faults to a device-wide reset), and (4) **optional SR-IOV hardware
partitioning** (license-free on AMD GIM and Intel Xe, impossible on NVIDIA without licensed vGPU).
The `GpuBackend` trait isolates exactly these four. Everything else — enumeration, context/queue
creation, memory budget, timeline sync — is one code path.

---

## 1. Cross-vendor render/compute + dma-buf export

**Headless Vulkan render/compute is fully cross-vendor.** All three vendors expose an offscreen
Vulkan device with no window-system surface: NVIDIA proprietary (documented headless offscreen +
external-memory export, per research/06), RADV and ANV via Mesa (no display server needed).
Vulkan 1.3 is conformant on NVIDIA Maxwell-and-newer, AMD Vega + all RDNA, and Intel Iris Xe +
Arc; Mesa 25.0 brought **Vulkan 1.4 to RADV, ANV, and NVK** in lockstep
([9to5Linux — Mesa 25.0](https://9to5linux.com/mesa-25-0-linux-graphics-stack-brings-vulkan-1-4-support-on-radv-anv-and-nvk),
[AMD RDNA3 Vulkan 1.3](https://www.kitguru.net/gaming/joao-silva/amd-rdna-3-gpus-support-vulkan-1-3/),
[Intel Arc Vulkan 1.3](https://videocardz.com/newz/intel-arc-alchemist-mobile-is-officially-vulkan-1-3-compatible)).
So the *render* half of the HAL is a single Vulkan code path with no vendor branches.

**dma-buf export is where the vendors diverge — this is a genuine per-vendor seam.** The frame
must leave the replay context as an on-GPU blob dma-buf (`VK_KHR_external_memory_fd` +
`VK_EXT_external_memory_dma_buf` + `VK_EXT_image_drm_format_modifier`) so the encoder (NVENC /
Vulkan-Video) consumes it zero-copy.

- **AMD (RADV) and Intel (ANV):** DRM format modifiers and dma-buf export/import are **native and
  long-standing** — memory is neutral storage, an image's layout is fully described by the 64-bit
  modifier, and both drivers have shipped `VK_EXT_image_drm_format_modifier` for years
  ([Vulkan-Docs — VK_EXT_external_memory_dma_buf](https://github.com/KhronosGroup/Vulkan-Docs/blob/main/appendices/VK_EXT_external_memory_dma_buf.adoc),
  [VK_EXT_image_drm_format_modifier ref](https://registry.khronos.org/vulkan/specs/latest/man/html/VK_EXT_image_drm_format_modifier.html)).
  These are the "easy" backends for the presentation path.
- **NVIDIA is the historically weak mover, exactly as the task anticipated.** The proprietary
  driver only gained `VK_EXT_external_memory_dma_buf` around driver **545**, and it works *only*
  with `nvidia-drm.modeset=1` plus render-node (`/dev/dri/renderD*`, `render` group) access
  ([NVIDIA forum — dma_buf in 545](https://forums.developer.nvidia.com/t/vk-ext-external-memory-dma-buf-missing-in-545/275834)).
  The deeper reason NVIDIA lags on modifiers is architectural: **NVIDIA encodes image layout into
  the page tables** — each format/sample-count/compression combination maps to an 8-bit "PTE kind",
  so memory is *not* neutral and modifier interop needs per-image VA ranges + `VM_BIND`, unlike
  Intel/AMD ([Collabora — DRM format modifiers in NVK](https://www.collabora.com/news-and-blog/news-and-events/implementing-drm-format-modifiers-in-nvk.html)).

**Design consequence:** the HAL must *not* assume a modifier-tagged dma-buf handshake works
identically everywhere. It exposes `export_dmabuf`/`import_dmabuf` with an explicit modifier list
and a per-backend **capability flag** (`supports_drm_modifiers`, `requires_render_node`,
`requires_dedicated_alloc_for_modifier_images`). On NVIDIA the backend forces
`prefersDedicatedAllocation` for modifier images (the same workaround NVK uses) and, on Ampere,
may fall back to an **NVENC-in-context capture** (Vulkan→CUDA interop, research/09) instead of a
cross-process dma-buf when the modifier negotiation fails — the encode still happens on-GPU, we
just skip the external handle. **NEEDS VERIFICATION:** exact minimum NVIDIA driver where
`VK_EXT_image_drm_format_modifier` + dma-buf export is robust for a *server/Quadro* GA102 headless
(the 545 thread is desktop-centric).

## 2. The `GpuBackend` trait

The trait abstracts **eight** operation groups. Ownership mirrors ADR-0003: one long-lived
`GpuBackend` per physical device inside the privileged broker, which hands each per-VM jailed
replay process a `RenderContext` (its own Vulkan device/context, never shared à la MPS).

```rust
/// One physical GPU, one vendor backend. Lives in the broker; `Send + Sync`.
pub trait GpuBackend: Send + Sync {
    type Context: RenderContext;

    // (1) Enumeration & selection — vendor-neutral (VkPhysicalDevice + PCI/DRM match)
    fn enumerate() -> Vec<GpuDeviceInfo>;            // vendor, PCI BDF, DRM render node, VRAM, gen
    fn open(selector: &DeviceSelector) -> Result<Self> where Self: Sized;
    fn capabilities(&self) -> BackendCaps;           // the seam-map flags (see below)

    // (2) Per-VM context/queue creation — one isolated Vulkan context per guest
    fn create_context(&self, vm: VmId, req: ContextRequest) -> Result<Self::Context>;

    // (3) Memory allocation + residency/budget — VK_EXT_memory_budget, cross-vendor
    fn query_budget(&self) -> MemoryBudget;          // heapBudget/heapUsage per heap
    // admission is enforced above this by the broker (per-VM vramCapMB, ADR-0003)

    // (8) Fault / reset — the DEVICE_LOST unifier (see §3)
    fn poll_faults(&self) -> Vec<GpuFault>;          // drains vendor Xid/ring/engine events
    fn reset_scope(&self) -> ResetScope;             // Context | Queue | Engine | DeviceWide
}

/// Per-VM render context — the untrusted guest stream is replayed against this.
pub trait RenderContext: Send {
    fn submit(&mut self, batch: CommandBatch, prio: Priority) -> Result<TimelinePoint>;
    fn signal_timeline(&mut self, v: u64) -> Result<()>;      // VK timeline semaphore, universal
    fn wait_timeline(&mut self, v: u64, ns: u64) -> Result<()>;

    // dma-buf export/import — modifier-aware, per-vendor caveats live here (§1)
    fn export_dmabuf(&mut self, img: ImageId, mods: &[DrmModifier]) -> Result<DmabufHandle>;
    fn import_dmabuf(&mut self, fd: DmabufHandle, desc: &ImageDesc) -> Result<ImageId>;

    /// Vendor-neutral fatal signal: maps VK_ERROR_DEVICE_LOST + vendor reset behavior.
    fn is_lost(&self) -> Option<DeviceLost>;
}

/// The seam map, as runtime capability flags the arbiter branches on — NOT scattered ifs.
pub struct BackendCaps {
    pub vendor: Vendor,                              // Nvidia | Amd | Intel
    pub max_global_priority: Priority,               // Nvidia => Medium; Amd/Intel => Realtime
    pub priority_needs_cap_sys_nice: bool,           // true on amdgpu + anv for High/Realtime
    pub supports_drm_modifiers: bool,
    pub requires_render_node: bool,                  // NVIDIA needs nvidia-drm.modeset + render grp
    pub requires_dedicated_alloc_for_modifier_images: bool, // NVIDIA PTE-kind workaround
    pub finest_reset_scope: ResetScope,              // Amd/Intel: Queue/Engine; GA102: DeviceWide
    pub vulkan_video_encode: VideoCodecs,            // H264/H265/AV1 availability differs
    pub optional_sriov: bool,                        // §5 accelerated backend available
}

pub enum Priority { Low, Medium, High, Realtime }
pub enum ResetScope { Context, Queue, Engine, DeviceWide }
```

Notes on the operation groups:

- **(1) Enumeration/selection** is vendor-neutral: `vkEnumeratePhysicalDevices` +
  `VK_EXT_physical_device_drm` (PCI bus/device + DRM primary/render minor) lets the broker bind a
  Vulkan device to the exact `/dev/dri/renderD*` node and PCI BDF the VMM assigned — one code path.
- **(3) Memory budget/residency** is cross-vendor: **`VK_EXT_memory_budget` works on NVIDIA, AMD,
  and Intel** and reports `heapBudget`/`heapUsage` including driver-internal allocations; VMA uses
  it automatically ([VMA — staying within budget](https://gpuopen-librariesandsdks.github.io/VulkanMemoryAllocator/html/staying_within_budget.html),
  [VK_EXT_memory_budget ref](https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_memory_budget.html)).
  Our per-VM VRAM admission cap (ADR-0003) is enforced *above* the budget query, so the arbiter
  logic is identical on all vendors; only the numbers differ. (Caveat: NVIDIA has had bugs where
  `heapBudget` is over-reported and `heapUsage` lags after `vkFreeMemory`
  ([NVIDIA forum — budget too high](https://forums.developer.nvidia.com/t/vk-ext-memory-budget-returns-too-high-budget/73424)) —
  treat the number as a hint, not a hard fence; the hard fence is our own accounting.)
- **Timeline semaphores** (Vulkan 1.2 core) are universal on every target generation, so the whole
  guest↔host fence bridge (research/14) is one implementation.
- **(2)/(submit) submission priority is a real seam.** `VK_EXT_global_priority`/`VK_KHR_global_priority`
  (core in Vulkan 1.4) is the QoS knob ADR-0003 leans on. **RADV and ANV honor High/Realtime** but
  require `CAP_SYS_NICE` (they log "No CAP_SYS_NICE, falling back to regular-priority")
  ([VK_KHR_global_priority ref](https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_global_priority.html)).
  **NVIDIA's Linux driver currently caps at Medium** — realtime compute queues advertised in the
  470 changelog "disappeared", and on 550 "the highest priority available is now simply medium…
  all other mesa drivers function as expected"
  ([NVIDIA forum — no realtime queue on 550](https://forums.developer.nvidia.com/t/nvidia-binary-driver-unable-to-acquire-realtime-vulkan-queue-on-latest-550-driver-release/292555)).
  **Consequence:** on NVIDIA the arbiter cannot rely on hardware priority tiers and must lean
  harder on its **token-bucket / deficit submission throttle** (the software knob we fully own);
  on AMD/Intel it can *also* use global priority as a second lever. `max_global_priority` in
  `BackendCaps` drives this fallback automatically instead of hard-coding a vendor check.

## 3. Per-vendor fault/reset semantics (the hardest seam) and its abstraction

This is where the vendors differ most, and where the task's premise holds: **AMD and Intel give
*better* per-context fault isolation than GA102's device-wide reset.**

- **NVIDIA (GA102 / Ampere).** **Robust Channels** contain the *common* fault: the faulting kernel
  is terminated and its CUDA/Vulkan context destroyed while the GPU stays operational — "other
  kernels running on different SMs may continue"
  ([arXiv — GPU fault resilience in MPS](https://arxiv.org/pdf/2605.26461)). Error reporting is the
  **Xid** stream out of the KernelRc (Robust Channels) engine object
  ([DeepWiki — Xid error reporting](https://deepwiki.com/eunomia-bpf/gpu_ext-kernel-modules/6.2-error-reporting-(xid))).
  **But the severe classes still escalate to a device-wide reset that downs all tenants** — e.g.
  Xid 79 is "instant atomic GPU death" with no precursor
  ([open-gpu-kernel-modules #1151](https://github.com/NVIDIA/open-gpu-kernel-modules/issues/1151)),
  and uncorrectable ECC triggers driver-level error recovery that terminates the affected app
  ([NVIDIA GPU Memory Error Management r575](https://docs.nvidia.com/deploy/pdf/NVIDIA-GPU-Memory-Error-Management.pdf)).
  GA102 has no MIG, so this residual is irreducible in software — exactly ADR-0003's accepted
  negative. **Finest reset scope on NVIDIA: `DeviceWide` for the severe Xid class.**
- **AMD (amdgpu, RDNA2/RDNA3+).** amdgpu has moved aggressively toward **granular recovery**:
  **soft recovery** (wave kill), **ring reset** (re-emit unprocessed state after resetting the
  ring), and, landing across 2025, **per-user-queue reset + recovery** for gfx/compute/SDMA queues
  that recovers a single hung queue "without immediately requiring full GPU reset", plus **enforce
  isolation** that scrubs GPU state between processes
  ([amd-gfx — user queue reset & recovery](https://www.mail-archive.com/amd-gfx@lists.freedesktop.org/msg126780.html),
  [amd-gfx — re-emit state on ring reset](https://www.mail-archive.com/amd-gfx@lists.freedesktop.org/msg125511.html),
  [amd-gfx — adjust ring reset behavior](https://www.mail-archive.com/amd-gfx@lists.freedesktop.org/msg123448.html)).
  **Finest reset scope: `Queue`** — strictly better tenant isolation than GA102.
- **Intel (i915 / Xe).** The Xe driver resets **per-engine** via the **GuC** microcontroller
  ("GT0: Engine reset: engine_class=rcs…"), banning the offending context and returning
  `VK_ERROR_DEVICE_LOST` to just that context
  ([Dota-2-Vulkan #461 — xe engine reset](https://github.com/ValveSoftware/Dota-2-Vulkan/issues/461)).
  **Finest reset scope: `Engine`.** The caveat: engine reset can *fail* ("GuC engine reset request
  failed") and escalate to a full GT reset, so isolation is good-but-not-guaranteed.

**The abstraction.** Every vendor ultimately surfaces the fatal condition to Vulkan as
**`VK_ERROR_DEVICE_LOST`** on submit/wait. The HAL therefore unifies on that single signal and
layers a vendor-supplied **`ResetScope` hint** on top:

1. `RenderContext::is_lost()` returns `DeviceLost` the moment any submit/wait yields
   `VK_ERROR_DEVICE_LOST`. The per-VM replay process treats this as a **per-VM fault** (ADR-0003):
   `kill(process)` reaps all GPU state; the broker re-admits a fresh context. This path is
   **identical on all three vendors** — it is the load-bearing invariant.
2. `GpuBackend::poll_faults()` drains the vendor event source (NVIDIA Xid via NVML/kernel log; AMD
   amdgpu GPU-reset uevents; Intel Xe reset dmesg/uevents) into a normalized `GpuFault { scope,
   severity, tenant_hint }`. The broker uses `reset_scope()`/`finest_reset_scope` to decide blast
   radius: `Context`/`Queue`/`Engine` → quarantine **one tenant**; `DeviceWide` → **quarantine the
   whole GPU** and evict/migrate all its VMs (the ADR-0003 "full-reset-class Xid" quarantine).

So the isolation *model* is one abstraction (`DEVICE_LOST` → per-VM kill → broker re-admit); the
*blast-radius policy* is data-driven off `ResetScope`. Porting to AMD/Intel doesn't change the
model — it just makes the common case land in the `Queue`/`Engine` branch instead of `DeviceWide`,
which **improves** multi-tenant safety on non-NVIDIA hosts for free. **NEEDS VERIFICATION:** the
exact amdgpu/Xe uevent payloads and whether they carry a reliable per-VF/per-context tenant hint,
and whether Ampere ever recovers a "device-wide" Xid without a full driver reload.

## 4. The GPU generation floor

The gating feature set is **Vulkan 1.3 + timeline semaphores (VK 1.2) + external-memory/dma-buf +
(ideally) Vulkan Video encode** for the presentation path. Concrete floor per vendor:

| Vendor | **Floor** | Why (gating feature) | Notes |
|---|---|---|---|
| **NVIDIA** | **Turing (RTX 20 / Quadro RTX)** | Vulkan 1.3 exists back to Maxwell, but Turing is the clean floor for modern **NVENC/NVDEC + Vulkan Video** and `nvidia-open` (Turing+). Our A5000/GA102 is well above it. | Maxwell/Pascal: render-capable but old media; treat as unsupported. |
| **AMD** | **RDNA / RDNA2** (Navi 1x/2x) | Vega has Vulkan 1.3 but weak VCN media; **RDNA2 is the practical floor for Vulkan-Video decode + AV1 decode**; RADV video *encode* (H.264/H.265) needs modern VCN. | Vega = render-only fallback, not a video floor. |
| **Intel** | **Xe / Arc Alchemist (Gen12+)** | Iris Xe (Tiger Lake, Gen12) and Arc (Xe-HPG) are Vulkan 1.3 conformant with ANV video decode; **Gen9.5 (Skylake) is below the floor** (no Vulkan Video, aging). | Discrete **Arc** preferred for encode-class media. |

**Vulkan Video reality (2026), the encode gate for browser streaming (research/09):**
- **RADV (AMD):** decode H.264/H.265/**AV1** (+ VP9 decode landed mid-2025); **encode H.264/H.265**,
  with **AV1 encode merged ahead of Mesa 25.2**
  ([Phoronix — RADV AV1 encode](https://www.phoronix.com/news/RADV-Merges-AV1-Encode),
  [Khronos — Vulkan Video AV1 + H.264/265 encode](https://www.khronos.org/blog/khronos-releases-vulkan-video-av1-decode-extension-vulkan-sdk-now-supports-h.264-h.265-encode)).
- **ANV (Intel):** `VK_KHR_video_decode_h264/h265` since Mesa 24.0.4; encode maturing.
- **NVIDIA:** full Vulkan-Video encode/decode (NVIDIA drove the H.264/H.265/AV1 encode extensions),
  and on Ampere we can bypass Vulkan Video entirely and use **NVENC via CUDA interop** (research/09).

**Practical statement:** target **NVIDIA Turing+, AMD RDNA2+, Intel Arc (Xe-HPG)+** as the
first-class floor for the *full* render+encode pipeline; allow **AMD Vega / Intel Iris Xe** as a
render-only degraded tier (compute/render works, host-side encode falls back to CPU or a lower
codec). The `BackendCaps.vulkan_video_encode` flag drives that degradation.

## 5. SR-IOV / vendor-native sharing as an *optional* accelerated backend

API-remoting stays the **universal default** — it is the only path that works on NVIDIA without a
licensed vGPU (research/02). But where a vendor offers a **license-free hardware partition**, the
HAL can expose it as an *alternate accelerated `GpuBackend` impl* behind the same trait, for lower
CPU overhead when hardware isolation is available:

- **AMD MxGPU (SR-IOV) — open and license-free.** AMD publishes the **GIM (GPU-IOV Module)** kernel
  driver open-source (VF config, world-switch scheduling, hang detection + FLR reset, PF/VF
  handshake); it's Instinct-first today with **Radeon on the public roadmap**, and consumer Radeon
  Pro (e.g. W7900) does **not** yet expose SR-IOV
  ([Phoronix — AMD open-source GIM](https://www.phoronix.com/news/AMD-GIM-Open-Source),
  [amd/MxGPU-Virtualization](https://github.com/amd/MxGPU-Virtualization)).
- **Intel SR-IOV — upstream, license-free, the strongest client path.** SR-IOV **replaces GVT-g**
  for Xe/Gen12+; **Flex 140/170** expose VFs, and **Battlemage (B50/B60) SR-IOV is landing in Linux
  6.17** (2025) — no per-VM license, mainline kernel
  ([Phoronix — Intel BMG SR-IOV in 6.17](https://www.phoronix.com/news/Intel-Enables-BMG-SR-IOV-Linux),
  [Intel — graphics virtualization support matrix](https://www.intel.com/content/www/us/en/support/articles/000093216/graphics/processor-graphics.html)).
- **NVIDIA:** SR-IOV VFs exist on Ampere silicon but only come alive under the **licensed
  `vgpu-kvm` host driver** — off-limits (research/02). NVIDIA has **no** license-free hardware
  partition; API-remoting is mandatory there.

**Design placement:** SR-IOV is a *second* backend family (`SriovBackend: GpuBackend`) that, when
present and license-free, hands each VM a VF as a near-native device — but it changes the *guest*
model (the guest gets a real VF driver, not our vfio-user device), so it is a **parallel product
mode**, not a drop-in swap of the replay path. The universal, guest-uniform, Windows-and-Linux
default remains API-remoting. Treat SR-IOV as an opt-in "accelerated on capable AMD/Intel hosts"
tier, gated by `BackendCaps.optional_sriov`.

## Seam map — cross-vendor vs per-vendor, at a glance

| Capability | Cross-vendor (one code path) | Per-vendor seam (isolate in backend) |
|---|---|---|
| Headless render/compute | ✅ Vulkan 1.3/1.4 all three | — |
| Device enum/selection | ✅ VkPhysicalDevice + `VK_EXT_physical_device_drm` | — |
| Timeline sync / fences | ✅ VK 1.2 timeline semaphores | — |
| Memory budget/residency | ✅ `VK_EXT_memory_budget` all three | NVIDIA over-reports; treat as hint |
| Submission priority | tier plumbing shared | **NVIDIA caps at Medium**; AMD/Intel Realtime (+CAP_SYS_NICE) |
| dma-buf / DRM modifiers | ✅ RADV/ANV native | **NVIDIA late (drv 545+), PTE-kind quirks, render-node gated** |
| Fault → `DEVICE_LOST` → kill | ✅ one model all three | **reset scope**: GA102 DeviceWide vs AMD Queue vs Intel Engine |
| Vulkan Video encode | extension plumbing shared | codec availability differs (RADV AV1; ANV maturing; NVIDIA full/NVENC) |
| Hardware partition | — (not the default) | **SR-IOV: AMD GIM / Intel Xe license-free; NVIDIA licensed-only** |

**Verdict:** adding a vendor is a `GpuBackend` impl plus the right `BackendCaps` flags — not a
rearchitecture. The four seams (priority cap, dma-buf/modifier export, reset scope, optional
SR-IOV) are the entire vendor-specific surface; the arbiter branches on runtime capability flags,
never on a hard-coded vendor `match`.

## Sources

- Mesa 25.0 — Vulkan 1.4 on RADV/ANV/NVK: https://9to5linux.com/mesa-25-0-linux-graphics-stack-brings-vulkan-1-4-support-on-radv-anv-and-nvk
- AMD RDNA3 GPUs support Vulkan 1.3 (KitGuru): https://www.kitguru.net/gaming/joao-silva/amd-rdna-3-gpus-support-vulkan-1-3/
- Intel Arc Alchemist Vulkan 1.3 compatible (VideoCardz): https://videocardz.com/newz/intel-arc-alchemist-mobile-is-officially-vulkan-1-3-compatible
- VK_EXT_external_memory_dma_buf appendix (Khronos): https://github.com/KhronosGroup/Vulkan-Docs/blob/main/appendices/VK_EXT_external_memory_dma_buf.adoc
- VK_EXT_image_drm_format_modifier (Khronos ref): https://registry.khronos.org/vulkan/specs/latest/man/html/VK_EXT_image_drm_format_modifier.html
- NVIDIA forum — VK_EXT_external_memory_dma_buf missing in 545 (fix = modeset + render group): https://forums.developer.nvidia.com/t/vk-ext-external-memory-dma-buf-missing-in-545/275834
- Collabora — Implementing DRM format modifiers in NVK (PTE-kind quirk): https://www.collabora.com/news-and-blog/news-and-events/implementing-drm-format-modifiers-in-nvk.html
- VK_KHR_global_priority ref (privilege / NOT_PERMITTED): https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_global_priority.html
- NVIDIA forum — no realtime Vulkan queue on 550 driver (caps at medium; mesa fine): https://forums.developer.nvidia.com/t/nvidia-binary-driver-unable-to-acquire-realtime-vulkan-queue-on-latest-550-driver-release/292555
- VK_EXT_memory_budget ref: https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_memory_budget.html
- VMA — staying within budget (cross-vendor): https://gpuopen-librariesandsdks.github.io/VulkanMemoryAllocator/html/staying_within_budget.html
- NVIDIA forum — VK_EXT_memory_budget over-reports: https://forums.developer.nvidia.com/t/vk-ext-memory-budget-returns-too-high-budget/73424
- arXiv — Characterization-Guided GPU Fault Resilience in NVIDIA MPS (per-context fault behavior): https://arxiv.org/pdf/2605.26461
- DeepWiki — NVIDIA Xid error reporting (Robust Channels / KernelRc): https://deepwiki.com/eunomia-bpf/gpu_ext-kernel-modules/6.2-error-reporting-(xid)
- open-gpu-kernel-modules #1151 — Xid 79 instant atomic GPU death: https://github.com/NVIDIA/open-gpu-kernel-modules/issues/1151
- NVIDIA GPU Memory Error Management (r575, ECC recovery): https://docs.nvidia.com/deploy/pdf/NVIDIA-GPU-Memory-Error-Management.pdf
- amd-gfx — user queue reset & recovery (per-queue, no full GPU reset): https://www.mail-archive.com/amd-gfx@lists.freedesktop.org/msg126780.html
- amd-gfx — re-emit unprocessed state on ring reset: https://www.mail-archive.com/amd-gfx@lists.freedesktop.org/msg125511.html
- amd-gfx — adjust ring reset behavior (enforce isolation vs soft recovery): https://www.mail-archive.com/amd-gfx@lists.freedesktop.org/msg123448.html
- ValveSoftware/Dota-2-Vulkan #461 — Intel Xe per-engine reset → DEVICE_LOST: https://github.com/ValveSoftware/Dota-2-Vulkan/issues/461
- Phoronix — RADV merges AV1 Vulkan Video encode (Mesa 25.2): https://www.phoronix.com/news/RADV-Merges-AV1-Encode
- Khronos — Vulkan Video AV1 decode + H.264/H.265 encode SDK: https://www.khronos.org/blog/khronos-releases-vulkan-video-av1-decode-extension-vulkan-sdk-now-supports-h.264-h.265-encode
- Phoronix — AMD publishes open-source GIM (MxGPU) driver, Radeon on roadmap: https://www.phoronix.com/news/AMD-GIM-Open-Source
- amd/MxGPU-Virtualization (GIM source): https://github.com/amd/MxGPU-Virtualization
- Phoronix — Intel enables Battlemage SR-IOV in Linux 6.17: https://www.phoronix.com/news/Intel-Enables-BMG-SR-IOV-Linux
- Intel — graphics virtualization support (SR-IOV replaces GVT-g for Gen12+): https://www.intel.com/content/www/us/en/support/articles/000093216/graphics/processor-graphics.html
