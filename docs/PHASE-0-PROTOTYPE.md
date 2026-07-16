# Phase 0 — the prototype that proves the whole loop

> Derived from ADRs 0001–0006 and research docs 06/07/09/10/11. **The concrete, register-level device
> to build in Step 1/5 is fully specified in [`research/24-qemu-device-implementation-spec.md`](research/24-qemu-device-implementation-spec.md)**
> (config space, BAR/register map, vfio-user message handling, DMA, MSI-X, argv, lifecycle), grounded
> in [`research/25-device-mechanics-book-grounding.md`](research/25-device-mechanics-book-grounding.md).
> Goal: prove the *entire* round trip end-to-end on the smallest possible slice, because that round trip
> **is the whole risk**. Everything else (2nd VM, scheduler, Windows, 3D, client-offload) is deferred
> until this is solid.

## The one sentence

**One Linux guest → one host Vulkan context: forward ONE Vulkan workload through ONE command
ring with ONE fence and ONE blob-backed image, and present it ONCE into the console.**

```
Linux guest                                   Host (Linux, KVM)
┌──────────────────────────┐   vfio-user PCI  ┌───────────────────────────────────────┐
│ test app → our Vulkan     │  BAR mmap ring   │ replay process (Rust, jailed)          │
│ encoder (thin ICD/layer)  │──doorbell(eventfd)──▶ decode ring → ResourceTracker        │
│  ↕ our DRM/KMS driver (C)  │◀── MSI-X fence ──│  replay on headless NVIDIA Vulkan       │
│  scanout blob (dma-buf)    │◀═ memfd zero-copy═▶  render → blob dma-buf                 │
└──────────────────────────┘                   │  present blob → QEMU SPICE → relay      │
                                               └───────────────────────────────────────┘
```

## Explicit non-goals for Phase 0

No 2nd VM. No scheduler/quotas (rely on the driver's own time-slicing). No Windows. No 3D app
(a spinning triangle or a headless compute dispatch is enough). No encoding (uncompressed RGBA
over localhost is fine). No live migration. No full Vulkan coverage — only the handful of
entrypoints the one workload needs.

## Build order (each step is independently testable)

### Step 1 — the seam spike (validates ADR 0001) — do this FIRST, standalone
A throwaway custom PCI device using rust-vmm `vfio-user` `Server`/`ServerBackend`:
BAR0 = doorbell + status page, 1 MSI-X vector, `DMA_MAP` mmap of a memfd. Launch under
**QEMU ≥ 10.1.1** with `-object memory-backend-memfd,share=on … -device vfio-user-pci,socket=…`.
**Prove:** (a) doorbell is an eventfd kick, not a socket msg; (b) a 256 MB buffer is
server-visible zero-copy via mmap, not `DMA_READ`; (c) MSI-X reaches the guest; (d) reset on
reboot; (e) a running-VM qcow2 disk snapshot still succeeds with the device attached; (f)
`savevm` fails cleanly. **If (a) or (b) fail → invoke the ADR-0001 fallback before going further.**

### Step 2 — the shared ABI crate (validates ADR 0004, minimal)
`infinigpu-abi` (`no_std`, `zerocopy` wire structs) + `infinigpu-ring` (`no_std` SPSC + seqno,
loom-tested). Only the messages Phase 0 needs: `NEGOTIATE`, `CTX_CREATE`, `RESOURCE_CREATE_BLOB`,
`MAP_BLOB`, `SUBMIT_CMD` (opaque Vulkan payload), a fence, `SET_SCANOUT_BLOB`, `RESOURCE_FLUSH`.
Export a cbindgen header for the C guest driver + a round-trip conformance test (Rust encodes ↔ C
decodes the same bytes).

### Step 3 — the Linux guest driver (C) (validates ADR 0005 Linux)
A minimal DRM/KMS driver: `drm_simple_display_pipe` + `drm_gem_shmem` + dumb buffers + one CRTC/
plane/encoder/connector + pageflip. Binds to our vfio-user PCI device (class `0x030000`). Maps the
BAR ring, rings the doorbell, waits on MSI-X. Uses the cbindgen ABI header. First success = a
`RESOURCE_FLUSH`ed dumb framebuffer shows up on the host (pure 2D, no Vulkan yet).

### Step 4 — the guest Vulkan encoder (thin)
A minimal Vulkan layer/ICD that encodes just the entrypoints the test workload calls
(`vkCreateInstance/Device`, one queue, `vkAllocateMemory` on a blob, one command buffer,
`vkQueueSubmit`, one fence) into `SUBMIT_CMD` payloads on a command ring. Study Venus's codegen but
hand-roll this subset for Phase 0.

### Step 5 — the host replay process (Rust, jailed) (validates ADR 0002/0003)
Decode the ring; maintain a `ResourceTracker` (guest handle → host Vulkan handle); replay the
Vulkan subset against a **headless NVIDIA Vulkan** context (`ash`/`vulkano`); render into a
blob-backed `VkImage`; signal the fence (MSI-X). Run it **jailed** (namespaces + seccomp) as the
per-VM process from day one — validate at least the "own context, never MPS" and "process kill =
reap" properties. Force host-side `robustBufferAccess` ON; validate every guest handle.

### Step 6 — present (validates ADR 0009 path, Phase-0 variant)
Present the blob dma-buf as a scanout into QEMU's own display/SPICE path; reuse the **unchanged**
`SpiceProxyService` TCP relay + the existing `.vv` native viewer. Uncompressed RGBA on localhost is
acceptable. First success = the triangle/compute result is visible in the console. **This closes the
loop:** `serialize → transport → decode → replay → fence → present`.

## Definition of done

A test app in one Linux guest renders one Vulkan workload that executes on a physical A5000 via the
host replay process and appears in the Infinibay console — with the guest RAM zero-copy shared
(memfd), the doorbell an eventfd, and the fence an MSI-X. No copies on the hot path, no QEMU fork,
all our own code.

## Infinibay touch-points introduced (minimal)

- `infinization/src/core/QemuCommandBuilder.ts` — add `-object memory-backend-memfd,share=on` +
  `-device vfio-user-pci,socket=${INFINIZATION_SOCKET_DIR}/<vmId>.gpu.sock`.
- A new host-side lifecycle hook to spawn/reap the per-VM replay process alongside VM start/stop.
- Nothing in `backend`/`frontend` yet (RBAC/quota/console-stream come in Phase 1).

## What Phase 0 deliberately leaves unproven (tracked, not forgotten)

- Multi-VM fairness/scheduling and VRAM admission caps (Phase 1).
- The device-wide-reset residual risk on GA102 (ADR 0003) — monitor, don't solve.
- Windows anything (Phase 2+).
- Encoded low-latency browser streaming (Phase 1, new console service).
- Full Vulkan coverage + the D3D sub-protocol (later).
