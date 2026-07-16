# ADR 0002 — Core sharing model: userspace API-remoting (Vulkan-first)

- **Status:** accepted
- **Date:** 2026-07-16
- **Feeds from:** research/02-gpu-sharing-taxonomy.md, research/06-data-plane-and-host-gpu.md

## Context

The product must share **one** physical NVIDIA RTX A5000 (GA102/Ampere) across **many** VMs, with
**no per-VM license** and **full ownership**. The choice of *how* the GPU is shared is the most
fundamental decision; everything else (protocol, drivers, scheduling) follows from it.

## Options considered

- **Hardware partitioning (MIG):** not available on GA102 (A100/H100-class only). ❌
- **SR-IOV / NVIDIA vGPU (mdev):** A5000 exposes 24 VFs but they are dead without the proprietary
  `vgpu-kvm` host driver **and** a per-VM DLS license. Violates constraints 1 & 2. ❌
- **`vgpu_unlock`:** still rides the proprietary blob; Ampere WIP; fragile; un-ownable. ❌
- **API-remoting / paravirtualization:** host owns the GPU with the **free** driver; each guest
  gets a synthetic device whose graphics API stream is serialized to a host multiplexer that
  replays it on a real host context. ✅ The only class that satisfies all five constraints.

## Decision

**Adopt userspace API-remoting, Vulkan-first.** Each guest serializes its Vulkan stream over our
command ring; a host **replay** process decodes it and executes it against a **headless host Vulkan
context** using the ordinary free NVIDIA driver; results are presented via a shared dma-buf.

- **Reference designs to crib (not adopt):** Venus (codegen'd Vulkan encoders, thin handle-mapping,
  blob resources), gfxstream (1:1 encoder/decoder threads), rutabaga_gfx / vhost-device-gpu (Rust
  separate-process backend templates).
- **Proof point:** virglrenderer's Venus path is tested against the NVIDIA proprietary userspace
  driver → API-remoting onto an NVIDIA host works with no vGPU licensing.
- **NVIDIA host execution:** headless Vulkan (cleaner than EGL/GL on NVIDIA), dma-buf export via
  `VK_KHR_external_memory_fd`. The kernel module flavor (nvidia-open vs proprietary) is irrelevant —
  the Vulkan/CUDA userspace is byte-identical; neither unlocks vGPU (which we don't need).
- **Why not DRM native context** (forward low-level UAPI, near-native): **no NVIDIA native context
  exists** (only Freedreno/AMDGPU/Intel/Asahi), so high-level API-remoting is the only NVIDIA route.

## Consequences

- **Positive:** license-free; runs on the GPUs we have; one host owns the device and multiplexes it;
  Vulkan is the single host execution API for both Linux (native) and Windows (later, via DXVK/vkd3d).
- **Negative / accepted:** we do **not** schedule the SMs (firmware time-slices contexts) — our
  fairness is cooperative (ADR 0003); Venus-style remoting "relies on implementation-defined
  behaviors" (Mesa) → host replay is coupled to specific NVIDIA driver versions (a maintenance/CI
  burden — pin & test host driver versions); double CPU work vs native (encode + decode).
- **Follow-up:** the wire protocol (ADR 0004), isolation (ADR 0003), and guest drivers (ADR 0005).
- **Revisit if:** NVIDIA ships an open, license-free mediation/SR-IOV path for GA102-class cards, or
  a usable NVIDIA native context lands upstream.
