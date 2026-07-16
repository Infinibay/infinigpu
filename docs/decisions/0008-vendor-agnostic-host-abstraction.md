# ADR 0008 — Vendor-agnostic host abstraction (GpuBackend + MediaCodec)

- **Status:** accepted
- **Date:** 2026-07-16
- **Feeds from:** research/20-vendor-agnostic-host-hal.md, research/21-cross-vendor-media-codec.md

## Context

Owner directive: the architecture **and code** must support NVIDIA, AMD, and Intel GPUs (host side).
We test now on 2× RTX A5000 (NVIDIA GA102) but must leave the design ready for AMD/Intel without a
rearchitecture. Not ultra-old GPUs.

## Decision

**API-remoting already makes infinigpu nearly vendor-free by construction** — the guest talks Vulkan/
D3D to our device and the host replays on whatever host Vulkan driver, so the guest never sees the
physical GPU and there is **no per-vendor guest driver**. Headless Vulkan 1.3 render/compute, device
enumeration (`VK_EXT_physical_device_drm`), timeline-semaphore fences, and memory budget
(`VK_EXT_memory_budget`) are one cross-vendor code path (NVIDIA Turing+, AMD RDNA2+, Intel Arc/Xe;
Mesa 25.0 ships Vulkan 1.4 on RADV/ANV/NVK).

The per-vendor surface collapses to **four seams behind a capability-flag `GpuBackend` trait** — the
arbiter branches on runtime `BackendCaps` data, never a hard-coded vendor `match`:

| Seam | NVIDIA (GA102) | AMD (RADV) | Intel (ANV) | Cap flag |
|---|---|---|---|---|
| **Submission priority** | capped at **Medium** (driver regression) → lean on token-bucket | High/Realtime (CAP_SYS_NICE) | High/Realtime | `max_global_priority` |
| **dma-buf / DRM-modifier export** | weak/late (~driver 545+, needs modeset + render-node, PTE-kind coupling) → dedicated-alloc + NVENC/CUDA fallback | native, mature | native, mature | `supports_drm_modifiers`, `requires_render_node`, `requires_dedicated_alloc` |
| **Fault/reset scope** | **DeviceWide** on severe Xid (downs all tenants; no MIG) | **per-Queue** reset (best isolation) | **per-Engine** GuC reset | `finest_reset_scope` |
| **Optional SR-IOV** | impossible without licensed vGPU | GIM/MxGPU (license-free, Instinct-first) | SR-IOV Gen12+ (Flex/Battlemage) | `optional_sriov` |

- **Media codec:** a `MediaCodec` trait with a **startup capability probe** and a priority table:
  **Vulkan Video (`VK_KHR_video_encode/decode_queue`) is the cross-vendor default** (native to
  `VkImage`, zero interop with our renderer, no CUDA), **VA-API** the broad fallback, **NVENC/AMF/
  oneVPL** opt-in optimized backends. (RADV already does H.264/H.265/**AV1** encode; Ampere NVENC
  cannot AV1-encode — so codec is negotiated per session, ADR 0009.)
- **Unify all faults on `VK_ERROR_DEVICE_LOST`** → kill the per-VM replay process → broker re-admit
  (identical everywhere); drive **quarantine blast-radius off the `finest_reset_scope` cap**
  (Queue/Engine = one tenant; DeviceWide = whole GPU).
- **SR-IOV is a separate PRODUCT MODE**, not just another backend: it changes the *guest* model (guest
  gets a real VF driver, not our device), so API-remoting stays the universal default and SR-IOV is an
  opt-in accelerated tier on capable license-free hosts.

## Consequences

- **Positive:** adding a vendor = a backend impl + correct caps flags, not a rearchitecture; the
  **AMD/Intel per-context reset scope materially shrinks the ADR-0003 / RISKS.md S1 residual** on
  non-NVIDIA hosts (a real reason multi-vendor matters beyond portability); AMD/Intel also give
  hardware submission priority NVIDIA currently denies us.
- **Negative / accepted:** the NVIDIA backend is the *most* constrained (priority cap + dma-buf
  fragility) yet it's our launch hardware — its token-bucket + NVENC/CUDA-interop fallback must be
  first-class, not bolted on. GPU floor excludes pre-Turing / pre-RDNA2 / pre-Gen12 (Vega, Iris Xe =
  render-only degraded tier).
- **NEEDS VERIFICATION:** exact NVIDIA driver where headless-server GA102 dma-buf export is robust;
  the amdgpu/Xe reset uevent payloads and whether they reliably identify the affected tenant.
