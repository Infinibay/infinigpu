# infinigpu — Roadmap

> Ties the research (docs/research/), the decisions (docs/decisions/ ADR 0001–0011), the risk
> burndown (RISKS.md), and the MVP (PHASE-0-PROTOTYPE.md) into a build sequence. Date: 2026-07-16.

## Positioning (what we are building and why)

A **100%-owned, vendor-agnostic, license-free GPU-sharing + remote-display stack** for Infinibay VDI.
The value is **ownership + vendor independence (NVIDIA/AMD/Intel) + a purpose-built perceptual
low-latency protocol** — *not* "cheaper than NVIDIA vGPU." (vGPU is NVIDIA-only, per-VM-licensed, and
gives no owned protocol; see RISKS.md S2.) Proceed **only** with that framing accepted.

## Stage −1 — Risk burndown (BEFORE committing quarters) — see RISKS.md

Cheapest-to-kill first. Do not skip; each can save a quarter.
1. `~2d` vGPU-vs-build TCO one-pager (real quote) → confirm the principle-not-cost framing. *(S2)*
2. `~1wk` host-driver-skew matrix: one Vulkan workload × 3 pinned NVIDIA drivers → pinning policy. *(S4)*
3. `1–2wk` expand the vfio-user spike: 8+ MSI-X vectors + Windows class-code bind + hot-unplug. *(S5)*
4. `~1wk` **reproduce the helix.ml N=4–8 concurrency wall + the reset blast radius** with the ADR-0006
   design — **the true go/no-go gate.** *(S1)*
5. `~3d` measure KSM loss + p95 latency at 4–8 guests. *(S6)*

**Gate:** if 1–5 clear → Phase 0. If S1's reset blast radius or S4's skew are intolerable for the
target deployment → de-scope (single-GPU / few-tenant / Linux-only best-effort) or buy vGPU for a
plain NVIDIA-only licensed-VDI need.

## Phase 0 — prove the loop (MVP) — see PHASE-0-PROTOTYPE.md

One Linux guest → one host Vulkan context: one Vulkan workload through one command ring, one fence,
one blob image, presented once via QEMU's **existing** SPICE path (no new client code).
- vfio-user device server (ADR 0001) · `infinigpu-abi`/`-ring` no_std crates (ADR 0004) · C DRM/KMS
  guest driver (ADR 0005) · thin guest Vulkan encoder · jailed Rust replay process on headless Vulkan
  (ADR 0002/0003) · seqno→MSI→sync_file completion (ADR 0006, single ring instance).
- **Done =** a triangle/compute result rendered on a physical A5000 via the replay process appears in
  the console, guest RAM zero-copy (memfd), doorbell = eventfd, fence = MSI-X. No QEMU fork.

## Phase 1 — share, schedule, and the real protocol

- **2nd VM + the host brain** (ADR 0007): capacity accounting → admission + VRAM ledger + residency →
  weighted fair-share + foreground boost + degradation ladder. Land the 6 `Department` Prisma fields +
  RBAC `attachGpu` + `GpuBrokerService` early.
- **Multi-ring scale-out** (ADR 0004/0006): N command rings, 1:1 decode/poller threads, `ring_idx`
  timelines. Validate the anti-deadlock 8-rule set at N=4–8.
- **infiniPixel v1** (ADR 0009): NVENC HEVC/H.264, intra-refresh, damage-aware hybrid, WebTransport/
  QUIC, WebCodecs browser client + local cursor; the `encoded-console-stream` service beside
  `SpiceProxyService`. SPICE stays as the fallback rung.
- **Guest intelligence + video offload** (ADR 0007/doc 17): idle/damage suppression, adaptive pacing,
  **video decode→NVDEC** (off the 3D SMs).
- **Perceptual layer** (ADR 0009): temporal frame-skip + content routing first; then the attention/QP
  map + psy-RC; VMAF/SSIMULACRA2 with a text edge gate.
- **Ships:** a genuinely usable **Linux VDI desktop** with accelerated 2D/video + a low-latency browser
  protocol, multi-tenant with quotas.

## Phase 2 — Windows desktop (display-first)

- **M1 IddCx display-only (Rust)** (ADR 0005): virtual monitor → capture → infiniPixel stream. WARP/
  software composition, **no in-guest 3D** — but a complete **office/knowledge-worker Windows VDI**.
- **M2 user-mode Vulkan/GL ICD (Rust):** remotes *native* Vulkan/GL guest apps to the arbiter, no
  kernel driver.
- **Ships:** Windows office VDI + native Vulkan/GL apps. Budget an EV cert + Partner Center attestation.

## Phase 3 — Windows hardware 3D (the frontier)

- **M3 WDDM render miniport (C/C++)** (ADR 0005, doc 15): fork max8rr8/viogpu3d, reseam its ring push
  to our vfio-user ring, payload = Venus-encoded Vulkan from M2's ICD, run **DXVK + vkd3d-proton in the
  guest** → D3D renders on our existing Vulkan arbiter, no bespoke D3D UMD. First cut: single-in-flight,
  no preemption, D3D11-via-DXVK. `~3–5 quarters` on top of M2.
- **M4:** D3D12 via vkd3d-proton + queue depth.
- **Ships:** hardware-accelerated D3D for CAD/3D Windows guests.

## Cross-cutting (start in Phase 0, mature throughout)

- **Vendor HAL** (ADR 0008): keep the `GpuBackend`/`MediaCodec` capability-flag traits from day one so
  the NVIDIA-specific bits (token-bucket QoS, NVENC/CUDA dma-buf fallback) are *backends*, not the
  architecture. AMD/Intel bring hardware submission priority **and better per-context reset** (shrinks
  the S1 residual). Vulkan Video is the cross-vendor codec default.
- **Security/isolation** (ADR 0003): jailed per-VM process, hostile-command-stream validation (Rust
  decoder, forced host robustness), fail-closed admission, fault quarantine by reset scope.
- **Residual, documented, monitored** (RISKS.md): GA102 device-wide reset (better on AMD/Intel); the
  host-driver/guest-protocol compat matrix (mitigated by appliance-side pinning).

## Milestone → persona → GPU vendor matrix

| | Linux office/2D+video | Linux 3D/CAD | Windows office | Windows 3D/CAD |
|---|---|---|---|---|
| **Phase 1** | ✅ | ✅ (Vulkan) | — | — |
| **Phase 2** | ✅ | ✅ | ✅ (display+native GL/VK) | — |
| **Phase 3** | ✅ | ✅ | ✅ | ✅ (D3D via DXVK/vkd3d) |
| **Vendor** | NVIDIA now; AMD/Intel via ADR-0008 caps (AMD/Intel improve availability) | | | |
