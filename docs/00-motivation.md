# infinigpu — Motivation & fit in Infinibay

> Status: living document. Written at project kickoff (2026-07-16).

## What infinigpu is

`infinigpu` is a **100% custom, from-scratch GPU virtualization stack written in Rust** whose
job is to let a single Linux KVM/QEMU host **share its physical GPU(s) among many guest VMs**
running Windows and Linux desktops. It is the graphics counterpart to the rest of the Infinibay
hypervisor stack.

Two halves, both ours:

- **Host side** — a virtual GPU device presented to each guest by QEMU, plus a host-side backend
  that multiplexes the real GPUs (here: 2× NVIDIA RTX A5000) across all running VMs, time-slicing
  and isolating GPU work per VM / per tenant.
- **Guest side** — our own guest GPU driver: a **WDDM** display driver on Windows and a
  **DRM/KMS** driver on Linux, shipped into guests the same way the agent is today.

## Why Infinibay needs it

Infinibay is a self-hostable **VDI** platform running *real* QEMU/KVM VMs as user desktops.
A usable modern desktop needs GPU acceleration (desktop compositor, browser, video decode,
3D/CAD, increasingly local AI). Every off-the-shelf way to give a VM a GPU fails at least one
of our hard constraints:

| Approach | Shares 1 GPU across N VMs? | License-free? | Works on our NVIDIA GPUs? | Cross-platform guests? | We own it? |
|---|---|---|---|---|---|
| **VFIO passthrough** | ❌ 1 GPU → 1 VM | ✅ | ✅ | ✅ | partial |
| **NVIDIA vGPU / GRID** (mdev/SR-IOV) | ✅ | ❌ per-VM subscription | ✅ (gated SKUs) | ✅ | ❌ |
| **virtio-gpu / VirGL / Venus** | ✅ (API remoting) | ✅ | ✅ | ⚠️ weak Windows, version ceilings | ❌ upstream, experimental |
| **infinigpu (this project)** | ✅ (goal) | ✅ | ✅ | ✅ | ✅ 100% |

- **Passthrough** gives a whole physical GPU to a single VM → with 2 A5000s only 2 VMs get a GPU.
  That destroys density, which is the entire point of VDI.
- **NVIDIA vGPU** does share a GPU, but it is proprietary, **license-gated per VM**, tied to
  specific GPU SKUs and NVIDIA's stack — incompatible with "self-hostable" and "100% own".
- **virtio-gpu / VirGL / Venus** are exactly the **existing experimental QEMU GPU drivers we are
  explicitly not adopting as our solution.** We study them as reference architectures only.

So Infinibay needs a GPU-sharing path that is **owned end-to-end, free of per-VM licensing,
runs on the GPUs we already have, and serves both Windows and Linux guests.** That is infinigpu.

## How it plugs into the existing stack

```
frontend ──GraphQL──▶ backend ──in-proc──▶ infinization ──builds QEMU argv──▶ qemu/KVM
                         │                      │  (+ our virtual GPU device: -device infinigpu-vgpu ...)
                         │                      ▼
                         │              infinigpu host backend  ◀── shares/time-slices ──▶ 2× RTX A5000
                         │                      ▲
                         │           virtio-serial / shared-mem data plane
                         ▼                      │
                     guest VM ─── infinigpu guest driver (WDDM / DRM-KMS) + infiniservice agent
```

- **infinization** gains a new virtual device it appends to the QEMU command line (peer to how it
  wires TAP/nftables today) and a lifecycle hook to the infinigpu host backend.
- **backend** owns provisioning + per-VM / per-department GPU policy (quota, priority, isolation)
  and telemetry, matching the existing multi-tenant RBAC + per-VM firewall model.
- **infiniservice / guest packaging** is the delivery mechanism for the guest driver, mirroring how
  the Rust agent binary is already served to guests.

## Non-negotiable constraints (the design must satisfy all)

1. **100% our own stack** — no existing experimental QEMU GPU driver as the solution (reference only).
2. **No per-VM proprietary licensing** — NVIDIA vGPU/GRID licensing is a non-starter for the core.
3. **Both Windows and Linux guests** are first-class.
4. **Rust** for everything we can (host backend, guest data-plane/protocol; kernel-mode parts where
   the ecosystem allows, C/C++ shims only where unavoidable).
5. **Share, don't dedicate** — the win condition is *density*: many VMs per physical GPU.
6. **Vendor-agnostic** — architecture and code support NVIDIA, AMD, and Intel (host side); test on the
   A5000s now, no rearchitecture to add a vendor. Floor: NVIDIA Turing+ / AMD RDNA2+ / Intel Arc-Gen12+.
   (API-remoting makes this nearly free — the guest never sees the physical GPU. See ADR 0008.)
7. **Specialized, not generic** — the driver's intelligence (scheduling, capacity, protocol) is designed
   *for this VDI workload*: capacity-aware (live VRAM + GPU-time), persona-aware, cooperative, and
   graceful under contention (ADR 0007), with an owned perceptual low-latency remote protocol (ADR 0009).

## Who the VMs are (the use cases the design is tuned to)

| Persona | GPU demand | VRAM | Latency | Design implication |
|---|---|---|---|---|
| **Office / task worker** | 2D desktop + browser + **video** (Teams/Zoom/YouTube); mostly idle | ~1.5 GB | **critical** | wins the scheduler (cheap+urgent); video → NVDEC; damage-skip → ~0 bits idle |
| **Knowledge / power user** | multi-monitor, more video, light 3D | ~2–3 GB | high | as above + more scanouts |
| **Designer / CAD / 3D / AI** | bursty heavy 3D or CUDA | 4–12 GB+ | tolerant | the natural throttle target (expensive+patient); best-effort |

Temporal reality: **mostly-idle desktops with bursts**, interactive input→photon-critical, diurnal load,
**login/boot storms** (~9am). Density is the whole point → the scheduler, the cooperative guest, and the
perceptual protocol all exist to *do less work per idle desktop* so one GPU fans out to many.

## Honest positioning (read RISKS.md)

infinigpu's value is **ownership + vendor-independence + an owned perceptual protocol** — **not** "cheaper
than NVIDIA vGPU" (vGPU runs on the A5000 and is cheap per seat, but is NVIDIA-only, per-VM-licensed, and
gives no owned protocol). As a *commodity multi-tenant SLA product on NVIDIA GA102* the red-team says
**NO-GO** (one guest's severe fault can reset the shared GPU and down all tenants — no MIG on GA102). It is
a **GO** as a principle-driven, multi-vendor (AMD/Intel improve that residual), owned platform. Proceed with
that framing and the RISKS.md burndown.

## Open architectural questions (resolved by the research phase)

- What is the cleanest way to present an **owned** device to QEMU without forking QEMU?
  (candidates under study: `vfio-user`/`libvfio-user` out-of-process device in Rust, `vhost-user`,
  a custom PCI device, `ivshmem` shared memory.)
- What does "share the GPU" concretely mean on **NVIDIA consumer/pro (non-vGPU) silicon** — API
  remoting vs. mediated time-slicing — and what's actually reachable without NVIDIA's proprietary
  mediation layer?
- What is the **minimal viable** guest driver on each OS (modeset + framebuffer first, 3D later)?
- How much of the guest kernel-mode driver can realistically be **Rust** today (Rust-for-Linux DRM,
  windows-drivers-rs)?

See `docs/research/` for the evidence behind each answer.
