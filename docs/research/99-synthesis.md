# 99 — Synthesis (post–Wave 1)

> Consolidates the six Wave-1 research docs (01–06) into one architecture picture.
> Wave 2 (docs 07–12) is verifying the open items flagged here. This is a **living**
> document. Date: 2026-07-16.

## 0. The one-line answer

To share one NVIDIA RTX A5000 (GA102) across many Windows+Linux VMs, license-free and
100% owned, there is **exactly one viable class**: **userspace API-remoting**. Each guest
runs *our* paravirtual GPU driver that **serializes its graphics API stream** onto a
command ring; a single **host arbiter process** decodes and **replays** it against a
**headless host Vulkan context** using the **ordinary free NVIDIA driver**, then presents a
**shared dma-buf** back to the guest scanout and to Infinibay's existing SPICE/VNC console.
Everything else (passthrough, MIG, SR-IOV/vGPU) is excluded by our hardware and licensing
constraints.

## 1. Why every other option is out (doc 02, 06)

| Option | Verdict for infinigpu |
|---|---|
| VFIO passthrough | 1 GPU → 1 VM. Doesn't share. A *host plumbing primitive* only. |
| MIG (hardware partition) | **Not on GA102** — datacenter A100/H100-class only. |
| SR-IOV / NVIDIA vGPU | A5000 exposes 24 VFs, but **dead without the proprietary `vgpu-kvm` host driver** + **per-VM DLS license**. Violates constraints 1 & 2. |
| `vgpu_unlock` | Still rides the proprietary blob; Ampere support WIP; fragile, un-ownable. Reject. |
| **API-remoting / paravirtualization** | ✅ Host owns the GPU with the **free** driver; guests get a synthetic device marshaled to a host multiplexer. **The only path satisfying all 5 constraints.** |

**Proof point that this works on NVIDIA without vGPU:** virglrenderer's *Venus* (Vulkan
remoting) path is explicitly tested against the **NVIDIA proprietary userspace driver** as a
host backend. The module flavor (nvidia-open vs proprietary) is irrelevant — the
Vulkan/CUDA userspace is byte-identical, and open modules also don't unlock vGPU (which we
don't need anyway).

## 2. The layered architecture

```
        GUEST VM (Windows or Linux)                         HOST (Linux, KVM)
  ┌───────────────────────────────────┐            ┌────────────────────────────────────────┐
  │ App → graphics API (Vulkan / D3D)  │            │  infinigpu ARBITER  (1 Rust process)     │
  │            │ intercept+serialize   │  cmd ring  │   • decode ring                          │
  │  our guest DRIVER  ───────────────────doorbell──▶   • ResourceTracker (guest→host handles) │
  │  (Linux DRM/KMS · Windows WDDM/IddCx)│◀──fence(dma)│   • replay on HEADLESS NVIDIA Vulkan    │
  │            ▲ present (blob dma-buf) │◀═zero-copy═▶│     (one host context per VM)           │
  └────────────┼──────────────────────┘  blob/udmabuf│   • scheduler: global_priority+quota    │
               │ PCI device (the "seam")              │   • scanout blob → import → encode      │
               │  ── vfio-user  OR  virtio-style ──────┤                    │                    │
        QEMU ◀─┘  (attached by infinization argv)     │                    ▼                    │
                                                      │        Infinibay SPICE/VNC relay (6100+) │
                                                      └────────────────────┼─────────────────────┘
                                                                           ▼  browser client
                                        2× RTX A5000 (free NVIDIA driver, headless Vulkan)
```

## 3. Component decisions & status

### 3.1 Host device seam — **DECIDED: vfio-user (ADR 0001)**
> Wave 2 (doc 07) verified against QEMU 10.1.1 that vfio-user honors direct BAR mmap, ioeventfd
> doorbells, and zero-copy memfd DMA, with a complete pure-Rust server crate — and refuted Option
> B's "reuse for free" claim. **Adopted vfio-user**, build against QEMU ≥ 10.1.1, subject to the
> Phase-0 spike. Details below kept for context.

How QEMU exposes our custom PCI device to the guest. Two candidates:

- **(A) vfio-user** (doc 01): `-device vfio-user-pci,socket=…`, merged upstream in **QEMU 10.1
  (Aug 2025)**; our device is a **separate Rust process**, no QEMU fork, fully custom ABI.
  Risk: MMIO perf (socket round-trip per register unless mmap-able BAR regions + ioeventfd
  doorbells are honored by the young client); rust-vmm vfio-user *server* maturity unproven.
- **(B) our own virtio-gpu-*style* device** (doc 06): our own device-ID + our own rust-vmm
  **vhost-user** backend, cribbing virtio-gpu's proven **blob/udmabuf/dma-fence** transport
  but writing our own protocol + guest drivers. Reuses mature zero-copy machinery; must stay
  clear of *adopting* upstream `virtio-gpu`/`vhost-user-gpu` (that would violate constraint 1).

> Decision deferred to a **1–2 week spike** (doc 07): verify QEMU 10.1+ honors server
> region-mmap + ioeventfd + memfd-DMA; if yes, vfio-user gives max ownership; if the perf
> escape hatches are missing, lean to (B). Fallback of last resort: custom in-QEMU C device =
> permanent fork (rejected unless forced).

### 3.2 Core sharing model — **DECIDED: API-remoting (Vulkan-first)**
Guest serializes Vulkan → host replays on headless NVIDIA Vulkan. Reference designs to crib
(not adopt): **Venus** (codegen'd Vulkan encoders/decoders, thin handle-mapping, needs blob
resources), **gfxstream** (1:1 encoder/decoder threads — the scalability fix over VirGL's
single decode thread), **rutabaga_gfx / vhost-device-gpu** (Rust templates for a
separate-process backend). **DRM native context** (forward low-level UAPI, near-native) is
**unavailable for NVIDIA** — no native context exists — so high-level API-remoting is the
only route.

### 3.3 Multiplexing / "intelligent sharing" — **DECIDED (cooperative)**
We do **not** schedule SMs (NVIDIA firmware time-slices host contexts; Ampere has HW
preemptive context switch). Our controllable knobs live in the arbiter:
- **`VK_EXT_global_priority`** per-VM context (lets the driver preempt between VMs).
- **Token-bucket / deficit throttle** metered by **GPU timestamps** → quotas + fairness.
- **VRAM admission control** — hard per-VM allocation caps (no HW VRAM isolation without MIG).
- Reference model for the fairness logic: the kernel **`drm_sched`** fair (CFS-like) scheduler.

### 3.4 Linux guest driver — **DECIDED: C KMS + Rust ABI crate (ADR 0005)**
> Wave 2 (doc 12) verified there is **no upstream Rust KMS** at kernel 7.2 (only render/buffer
> abstractions; RVKMS is an out-of-tree WIP RFC). The KMS/modeset driver is **C**; the Rust
> protocol crate is the ABI source-of-truth exported via a cbindgen header.

Paravirtual **DRM/KMS** driver modeled on virtio-gpu's 2D path
(`RESOURCE_CREATE_2D → ATTACH_BACKING → SET_SCANOUT → TRANSFER_TO_HOST_2D → RESOURCE_FLUSH`;
pageflip = alternate two resources). **3D lives in guest Mesa userspace** (a Venus-style
Vulkan ICD), **stays C**. Open point: how much of the guest *kernel* driver can be Rust —
GEM/scheduler/dma_fence Rust abstractions are upstream (kernel 7.0, Apr 2026), but **KMS
modeset Rust is not upstream** (Asahi/RVKMS out-of-tree). Doc 12 pins the exact line.

### 3.5 Windows guest driver — **DECIDED sequencing (doc 03, 08 verifying)**
- **M-early: IddCx display-only in Rust** (proven by `virtual-display-rs`): virtual monitor,
  capture composited frames, encode, stream. **Pixels only, ZERO in-guest 3D** (WARP/software
  compositing). User-mode → no BSOD, lighter signing, dodges April-2026 kernel-trust tightening.
- **M-late (the hard wall): a from-scratch WDDM UMD+KMD render pair** marshalling D3D/DXGI to
  the host. Microsoft's **GPU-PV does exactly this but is Hyper-V/VMBus-only** → unusable on
  KVM. **No open precedent**; multi-quarter; expect C/C++. Doc 08 pressure-tests how long we
  can ship VDI on IddCx-display + host-side 3D before this becomes mandatory.

### 3.6 Presentation — **DECIDED shape (doc 09 detailing)**
Blob-backed swapchain image = shared **dma-buf**; `SET_SCANOUT_BLOB + RESOURCE_FLUSH`, wait
on **dma-fence** (pin during `prepare_fb`, release on flush → no tearing). Host imports and
either composites or **H.264/HEVC-encodes** (Ampere; **AV1 encode needs Ada+ or RADV Vulkan-Video**,
negotiated per session — ADR 0009) → feeds **Infinibay's existing SPICE/VNC relay (ports 6100-6199)**.
Doc 09 settles NVENC licensing on the pro A5000 + the latency budget.

### 3.7 Rust/C split — **DECIDED (doc 05)**
| Component | Language |
|---|---|
| Host arbiter/renderer backend | **Rust** (`ash`/`vulkano`, `gbm.rs`/`drm-rs`; `cudarc` optional, off critical path) |
| Shared wire-protocol / command-ring / lifecycle crate | **Rust, `no_std`-clean from commit 1** (feature-gated for kernel/Windows/host-std) |
| vfio-user device server | **Rust** (rust-vmm `vfio-user`) — pending doc 07 maturity check |
| Linux guest kernel driver | **Rust on 7.2+** where abstractions allow, **C for KMS** if needed (doc 12) |
| Linux guest UMD (3D) | **C** (Mesa Gallium/Vulkan ICD) |
| Windows IddCx display driver | **Rust** (own UMDF/IddCx bindings, à la virtual-display-rs) |
| Windows WDDM render miniport + D3D UMD | **C/C++** (no Rust precedent) |

## 4. Hard problems / risk register (ranked)

1. **Windows in-guest 3D** — a from-scratch WDDM UMD+KMD render pair with no KVM precedent.
   Biggest unknown; deferred behind IddCx-display. *(doc 08)*
2. **Cross-VM fence/sync without deadlock** — real stacks (helix.ml) froze all contexts on a
   global block. Fence/queue design is make-or-break. *(doc 06, 11)*
3. **Resource lifetime tracking** — every guest handle needs an ordered host twin, reaped on
   guest crash. The bulk of the code. *(doc 06, 10, 11)*
4. **vfio-user MMIO performance & maturity** — needs mmap-able BARs + ioeventfd; young QEMU
   client. Could force seam (B). *(doc 07)*
5. **Multi-tenant isolation without hardware** — GPU fault/TDR from one guest must not sink
   all; VRAM starvation caps; sanitizing an untrusted command stream. *(doc 10)*
6. **Venus-style spec violation** — Mesa's own words: remoting "relies on
   implementation-defined behaviors" → couples host replay to specific NVIDIA driver versions.
   Product maintenance hazard. *(doc 06)*
7. **Presentation latency** staying zero-copy end-to-end. *(doc 09)*

## 5. Phased roadmap

- **Phase 0 — prove the loop (MVP):** *one* Linux guest → *one* host Vulkan context; forward
  **one** Vulkan workload (headless compute or a spinning triangle) through **one** command
  ring with **one** fence and **one** blob-backed image; present **once** into the SPICE
  console. This `serialize→transport→decode→replay→fence→present` round trip **is the entire
  risk.** Pick the seam per doc 07.
- **Phase 1 — share & schedule:** 2nd VM + arbiter scheduler (global_priority + token-bucket +
  VRAM caps); real desktop compositor over the Linux driver.
- **Phase 2 — Windows display:** IddCx display-only in Rust; encode/stream pipeline hardened.
- **Phase 3 — the hard core:** WDDM UMD+KMD render pair for in-guest D3D on Windows.
- **Cross-cutting:** security/isolation (doc 10) and the OS-neutral wire protocol (doc 11)
  are foundational and start in Phase 0.

## 6. Constraints check

| # | Constraint | Satisfied by |
|---|---|---|
| 1 | 100% our own (no existing experimental QEMU GPU driver) | Own device server + own protocol + own guest drivers; virtio-gpu/Venus/gfxstream as *reference only*. |
| 2 | No per-VM proprietary license | Host uses the **free** NVIDIA driver; no vGPU/DLS. |
| 3 | Windows **and** Linux guests | Linux DRM + Windows IddCx→WDDM, same host protocol. |
| 4 | Rust wherever feasible | Host + shared crate + vfio-user + IddCx in Rust; C only where the ecosystem forces it. |
| 5 | Share, don't dedicate | One GPU, N host contexts, cooperative time-slice + quotas. |

## 7. Wave 2 resolutions (all → ADRs)

- **07 → ADR 0001** vfio-user seam **confirmed** (BAR mmap + ioeventfd + memfd DMA real in QEMU
  10.1.1; pure-Rust server; Option B advantage refuted). Build against QEMU ≥ 10.1.1.
- **08 → ADR 0005** Windows render miniport unavoidable on KVM but **bounded** (~6–8mo, precedents
  exist off Hyper-V); 4-milestone sequence (IddCx → user-mode ICD → render miniport → D3D11/12).
- **09** NVENC is **license-free & uncapped on the pro A5000**; needs Vulkan→CUDA interop; Phase-0
  reuses the existing SPICE relay (ports **6100-6199**, CLAUDE.md's 6100-6119 is stale), Phase-1
  adds a browser WebCodecs/WebRTC stream.
- **10 → ADR 0003** **one jailed replay process per VM** + per-host broker, own Vulkan context
  (never MPS); irreducible **device-wide-reset residual risk** on GA102 documented + monitored.
- **11 → ADR 0004** **payload-agnostic multi-ring envelope** (1 control + N command rings, seqno
  completion); `no_std` Rust crate trio (`infinigpu-abi`/`-ring`/`-proto`); zerocopy+postcard hybrid.
- **12 → ADR 0005** **Linux guest KMS must be C** (no upstream Rust KMS at kernel 7.2; VGEM-in-Rust
  is render-only); the Rust ABI crate is exported to the C KMD via a cbindgen header.

See [`../decisions/`](../decisions/) for the ADRs (0001–0011, see ../decisions/README.md) and
[`../PHASE-0-PROTOTYPE.md`](../PHASE-0-PROTOTYPE.md) for the concrete first build.
