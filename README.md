# infinigpu

**A 100% custom, from-scratch GPU virtualization stack in Rust that lets one Linux
KVM/QEMU host share its physical GPU(s) among many Windows and Linux guest VMs — intelligently
time-sliced, license-free, and owned end-to-end.**

> Status: **research & design phase** (kickoff 2026-07-16). No code yet — we are doing a deep,
> evidence-based feasibility and architecture study first. See `docs/`.

## The problem it solves

Infinibay runs real QEMU/KVM VMs as user desktops (VDI). Modern desktops need a GPU, but every
existing way to give a VM a GPU fails one of our constraints:

- **VFIO passthrough** dedicates a whole GPU to one VM (2 GPUs → 2 VMs; no density).
- **NVIDIA vGPU/GRID** shares a GPU but is proprietary and **per-VM licensed**.
- **virtio-gpu / VirGL / Venus** are the existing experimental drivers we deliberately **do not**
  build on (weak Windows support, version ceilings, not ours).

infinigpu is the owned alternative: **one physical GPU, many VMs, no per-VM license, Windows +
Linux guests, all Rust.** See [`docs/00-motivation.md`](docs/00-motivation.md).

## Shape of the system (target)

| Layer | What | Where it runs |
|---|---|---|
| **Guest driver** | Our WDDM (Windows) / DRM-KMS (Linux) GPU driver | inside each guest VM |
| **Virtual device** | The vGPU device QEMU presents to the guest | host, attached to QEMU |
| **Host backend** | Multiplexes/schedules the real GPU(s) across VMs; renders/executes guest work | host userspace |

It plugs into the Infinibay stack: `infinization` attaches the virtual device to the QEMU command
line; the `backend` owns per-VM/per-department GPU policy; guest drivers are delivered like the
`infiniservice` agent.

## Repository layout

```
docs/
  00-motivation.md         # why this exists and how it fits Infinibay
  research/                # the deep-research corpus (one doc per domain, cited)
  decisions/               # Architecture Decision Records (ADRs)
  reference/               # book catalog + external reference notes
```

Rust workspace crates will be added once the architecture is decided (see `docs/decisions/`).

## Hard constraints (design must satisfy all)

1. 100% our own stack — no existing experimental QEMU GPU driver as the solution.
2. No per-VM proprietary licensing.
3. Windows **and** Linux guests are first-class.
4. Rust wherever feasible.
5. Share, don't dedicate — the win condition is density (many VMs per physical GPU).

## Where we are

Research phase in progress. Findings land in `docs/research/`; the running synthesis and the
architecture decision will land in `docs/decisions/`.
