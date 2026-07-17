# infinigpu

**A 100%-owned, from-scratch GPU-virtualization + remote-display stack in Rust** that lets one
Linux KVM/QEMU host share its physical GPU(s) among many guest VM desktops — cooperatively
time-sliced, **license-free**, vendor-agnostic, and owned end to end. The graphics peer to
Infinibay's `infinization` (hypervisor) and `infiniservice` (in-guest agent).

> **Status: working Phase-0 + core Phase-1** (kickoff 2026-07-16). A real Linux guest renders
> through our own DRM/KMS driver, the host executes the work on a physical **NVIDIA RTX A5000**,
> multiple VMs share that one GPU under a capacity-aware scheduler, and the guest desktop streams
> to a browser over our own **infiniPixel** protocol. See [`docs/IMPLEMENTATION-LOG.md`](docs/IMPLEMENTATION-LOG.md)
> for the blow-by-blow and [`docs/`](docs/) for the design corpus (23 research docs + 11 ADRs).

## The problem it solves

Infinibay runs real QEMU/KVM VMs as user desktops (VDI). Modern desktops need a GPU, but every
existing option fails one of our constraints:

- **VFIO passthrough** dedicates a whole GPU to one VM (2 GPUs → 2 VMs; no density).
- **NVIDIA vGPU/GRID** shares a GPU but is proprietary and **per-VM licensed**.
- **virtio-gpu / VirGL / Venus** — weak Windows support, version ceilings, not ours.

infinigpu is the owned alternative: **one physical GPU, many VMs, no per-VM license, Rust.**

## What works today

A real Linux guest boots, loads our driver, and its console renders on the physical GPU and streams
to a browser — through our own stack, no libvirt, no vGPU license:

```
┌─ guest VM ───────────────┐   vfio-user (UNIX socket)   ┌─ host ────────────────────────────────┐
│ fbcon / app              │                             │ infinigpu-device (vfio-user server)    │
│   ↓ DRM/KMS (infinigpu.ko)│  BAR0 regs + doorbell ────► │   ├─ DMA table (zero-copy memfd)       │
│   ↓ framebuffer (DMA)     │◄── MSI-X + retired ──────── │   ├─ GpuBroker  (admission + fair-share)│
│                          │◄══ zero-copy guest RAM ═════►│   ├─ replay → Vulkan render on the GPU │
└──────────────────────────┘                             │   └─ present → NVENC → infiniPixel ─────┼──► browser
                                                          └────────────────────────────────────────┘   (WebCodecs)
```

- **Phase-0 loop** — a guest ring submission is decoded on the host, rendered on the A5000, and
  DMA-written back; verified against real **QEMU 10.1.5** with the upstream `vfio-user-pci` client.
- **Real DRM/KMS display** — `/dev/dri/card0`, fbcon on our framebuffer, continuous presents.
- **Multi-VM sharing (the VDI differentiator)** — two VMs share one A5000 under a fail-closed
  admission + VRAM-ledger + weighted-fair-share scheduler (ADR-0007). No MPS, no license.
- **infiniPixel** — an owned low-latency remote-display protocol: NVENC H.264 → owned framing →
  WebSocket → browser WebCodecs, with damage-aware idle-skip (idle ⇒ ~0 bits). Replaces SPICE's
  GPU path.
- **Multi-ring** device (8 contexts), a **vendor HAL** (capabilities, not vendor names), and a
  Rust↔C ABI conformance guard.

## Crates & components

| Crate / dir | Lang | Role |
|---|---|---|
| `crates/infinigpu-abi` | Rust (`no_std`) | Wire ABI: PCI identity, BAR0 register map, zerocopy framing. Single source of truth (→ C header via cbindgen). |
| `crates/infinigpu-ring` | Rust (`no_std`) | SPSC command ring + seqno completion; `loom`-verified ordering. |
| `crates/infinigpu-hal` | Rust (pure) | Vendor HAL (ADR-0008): `GpuBackend`/`MediaEncoder` capability traits. |
| `crates/infinigpu-replay` | Rust (`ash`) | Headless Vulkan render backend — runs on the physical GPU: fixed-function clear, a **shader-executed** triangle (our SPIR-V), and **dma-buf/opaque-fd export** for zero-copy hand-off. |
| `crates/infinigpu-sched` | Rust | The GPU broker "brain" (ADR-0007): admission, VRAM ledger, token-bucket weighted fair-share, watchdog. |
| `crates/infinigpu-pixel` | Rust | infiniPixel (ADR-0009): NVENC/H.264 encode, owned protocol, WebSocket, idle-skip. |
| `crates/infinigpu-device` | Rust | The vfio-user PCI device server (ADR-0001) — config space, BAR0, DMA, MSI-X; ties broker + replay + pixel together. |
| `crates/infinigpu-viewer` | Rust | **Native desktop client** (the virt-viewer replacement, **no GTK/Qt**): `winit` (Wayland/Win32) + Vulkan (`ash`, swapchain + blit) + `openh264` decode + WebSocket. |
| `guest/linux/infinigpu.c` | C | The in-guest **DRM/KMS** display driver (ADR-0005; dual MIT/GPL — the DRM stack is `EXPORT_SYMBOL_GPL`). |
| `client/infinipixel.html` | JS | Browser WebCodecs viewer for the infiniPixel stream. |

## Build & test

```bash
cargo build            # all host crates
cargo test             # 34 unit/integration tests (no GPU/QEMU needed for most)
make -C guest/linux    # the guest DRM/KMS kernel module (against the running kernel's headers)

# QEMU with the upstream vfio-user-pci client (one-time, needs sudo for install):
./scripts/build-qemu-vfio-user.sh          # → /opt/qemu-vfio-user
```

### Demos & end-to-end tests (need the A5000 + built QEMU)

```bash
# host-only proofs (no QEMU):
cargo run -p infinigpu-device --bin infinigpu-pipeline-demo   # guest ring → A5000 render → DMA back
cargo run -p infinigpu-device --bin infinigpu-broker-demo     # 2 VMs share the A5000, weighted fair-share
cargo run -p infinigpu-replay --bin infinigpu-replay-triangle # shader-executed triangle + dma-buf/opaque-fd export
cargo run -p infinigpu-pixel  --bin infinigpu-pixel-demo      # NVENC → infiniPixel (open client/infinipixel.html)

# full-stack, boot a real guest under QEMU:
./scripts/guest-kms-test.sh          # DRM/KMS: /dev/dri/card0 + fbcon on our framebuffer
./scripts/guest-kms-pixel-test.sh    # the whole path: guest console → NVENC → infiniPixel → decoded H.264
./scripts/infinipixel-test.sh        # headless infiniPixel round-trip (Node client + ffmpeg decode)
./scripts/viewer-headless-test.sh    # the NATIVE client decodes the stream headless (winit+Vulkan window needs a display)

# native desktop client (needs a Wayland/Win32 display for the window):
cargo run -p infinigpu-viewer -- --port 8090            # connect + show in a window
cargo run -p infinigpu-viewer -- --headless --frames 60 # decode-only, no display (CI/dev)
```

A one-time readable-kernel copy is needed for the guest tests:
`mkdir -p ~/.cache/infinigpu && sudo install -m0644 /boot/vmlinuz-$(uname -r) ~/.cache/infinigpu/vmlinuz`.

## How it plugs into Infinibay

infinigpu is the graphics peer to `infinization`/`infiniservice`. The concrete wiring (QEMU argv,
the per-VM device-server lifecycle, the `Department` GPU policy fields + `GpuBrokerService`, and the
`encoded-console-stream` service beside `SpiceProxyService`) is specified in
[`docs/INTEGRATION.md`](docs/INTEGRATION.md) — ready to implement as a gated, opt-in feature.

## Status vs. roadmap

| | Status |
|---|---|
| Phase-0 loop, DRM/KMS display, cbindgen ABI guard | ✅ working |
| Multi-VM broker (admission + weighted fair-share), multi-ring, vendor HAL | ✅ working |
| infiniPixel v0 (NVENC H.264 + owned protocol + WebCodecs) + idle-skip + device wiring | ✅ working |
| infiniPixel v1 (damage-rect hybrid, intra-refresh, HEVC/AV1, WebTransport, perceptual/foveation) | ⏳ next |
| Per-VM jailed replay *process* + NVML attribution (ADR-0003) | ⏳ next |
| Infinibay backend/infinization wiring (per [`docs/INTEGRATION.md`](docs/INTEGRATION.md)) | ⏳ blueprint ready |
| Windows guest (IddCx → WDDM, DXVK/vkd3d) | ⏳ Phase 2–3 |

**Honest risk** (see [`docs/RISKS.md`](docs/RISKS.md)): as a *commodity multi-tenant SLA product on
GA102* this is a **NO-GO** (a severe Xid forces a device-wide GPU reset with no MIG). It is a **GO**
as a *principle-driven, owned, multi-vendor* platform — AMD/Intel's per-queue/engine reset shrinks
that residual, which is exactly why the architecture is capability-first (the vendor HAL).

## License

**MIT** — Copyright (c) 2026 Infinibay LLC `<andres@infinibay.net>` (see [`LICENSE`](LICENSE)).
The Linux guest kernel driver (`guest/linux/infinigpu.c`) is **dual `MIT/GPL`**: the kernel's
DRM/KMS stack exports its symbols `EXPORT_SYMBOL_GPL`, so a pure-MIT module would be refused those
symbols and fail to load — `MODULE_LICENSE("Dual MIT/GPL")` keeps it MIT while remaining loadable.
