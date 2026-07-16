# infinigpu — Implementation log

> The running record of the design→code transition. Design corpus is in `docs/`
> (research/, decisions/, ROADMAP, PHASE-0-PROTOTYPE). This file tracks what is
> actually built and what the code taught us that the docs didn't know.

## 2026-07-16 — Kickoff: foundation crates + host GPU datapath proven

### What exists now (Rust workspace)

```
Cargo.toml                     # workspace: abi, ring, replay
crates/infinigpu-abi/          # no_std, no-alloc wire ABI (zerocopy)   — DONE, tested
crates/infinigpu-ring/         # no_std SPSC ring + seqno               — DONE, tested + loom-verified
crates/infinigpu-replay/       # host Vulkan backend (ash)              — renders on the A5000
scripts/build-qemu-vfio-user.sh
```

- **`infinigpu-abi`** — PCI identity, BAR0 register map, and all Phase-0 wire
  structs as `#[repr(C)]` + zerocopy `FromBytes/IntoBytes/Immutable/KnownLayout`,
  with compile-time layout assertions. `#![forbid(unsafe_code)]` (zerocopy's derives
  are compatible). 7 tests green.
- **`infinigpu-ring`** — SPSC descriptor ring viewed over caller memory, seqno
  completion, `Release`/`Acquire` publish protocol. 5 unit/stress tests green **plus
  a `loom` model check** (`RUSTFLAGS="--cfg loom" cargo test --test loom_ring`) that
  exhaustively proves lossless, race-free ordering — the ADR-0004 requirement.
- **`infinigpu-device`** — the vfio-user PCI device `ServerBackend` (config space,
  BAR0 control registers, an mmap'd IOVA→HVA DMA table with fail-closed bounds checks,
  MSI-X). Validated **without QEMU** by `tests/loopback.rs`, which drives it with the
  real `vfio_user` `Client` (the same protocol QEMU speaks) and proves: PCI identity +
  display class, BAR0 `MAGIC`/`ABI`/`CAPS`/`GLOBAL_CTRL`, **zero-copy DMA read+write
  through a shared memfd**, and **MSI-X delivery** via eventfds. 1 integration test green.
- **`infinigpu-replay`** — headless Vulkan on the physical GPU via `ash` (prefers the
  NVIDIA proprietary driver → Vulkan for free, no vGPU license). `HostGpu::render_clear`
  runs a real graphics render pass and DMA-reads the result back. The smoke binary
  verified pixel-exact readback on an **RTX A5000** (render ~10 ms). This closes the
  **GPU-facing half of the Phase-0 loop** with no QEMU involved.

### Two ground-truth findings that changed the design (verified against source)

1. **The `vfio-user` Rust crate (v0.1.3) has NO ioeventfd doorbells.**
   `GET_REGION_IO_FDS` is hard-rejected, so a BAR write is always a synchronous
   socket round-trip — the "doorbell = eventfd" hot path in research/24 is not
   available with the stock crate. This is exactly the ADR-0001 fallback (ERRATA #5),
   now **confirmed mandatory**. Resolution baked into the ABI: the device advertises
   `caps::POLL_SUBMIT` (host polls the sparse-mmap'd shared index page SQPOLL-style;
   the trapped doorbell only *wakes* an idle poller) and does **not** advertise
   `IOEVENTFD_DOORBELL`. Zero-copy guest RAM (memfd via `dma_map`) and MSI-X (hand-
   rolled cap, per-vector eventfds) both work as designed.
2. **`vfio-user-pci` is upstream in QEMU since 10.1** (no oracle fork). Build ≥ 10.1.1
   to a private prefix via `scripts/build-qemu-vfio-user.sh`. Property is `socket=`
   (SocketAddress); `share=on` on the RAM backend is mandatory for DMA; there is no
   live-migration knob (savevm fails cleanly — acceptable).

### 2026-07-16 (later) — real QEMU integration verified

Built QEMU **10.1.5** with the upstream `vfio-user-pci` client into `/opt/qemu-vfio-user`
(via `scripts/build-qemu-vfio-user.sh`). `scripts/smoke-qemu-device.sh` boots it headless
with our `infinigpu-device` attached and **no guest OS**, and the device server log proves
the seam works against the *actual* QEMU vfio-user client (not just the loopback):

- `config read @0x00 (PCI enumeration): 0x1b36:0x0110` — SeaBIOS enumerated our device;
- `DMA_MAP iova=0x0 size=0x40000000 (guest RAM mapped zero-copy)` — QEMU shared the full
  1 GB guest-RAM memfd into our device process.

Device fix from this run: a `DMA_MAP` **without** a shared fd (BIOS/ROM shadow, MMIO holes)
is now a silent no-op (the region simply stays unmapped → guest DMA into it fails closed),
instead of erroring. Notes: `socket` is a `SocketAddress` union so the **JSON `-device`
form is required** (flat `socket.type=` is rejected); `x-pci-class-code` takes a number
(`229376` = `0x038000`). Both recorded in the smoke script.

### 2026-07-16 (later still) — full host pipeline fused end-to-end

`infinigpu-device` now depends on `infinigpu-replay`, and a doorbell write runs a
**submit engine**: it decodes the `SUBMIT_CMD` at the command-ring base from guest RAM
(via the DMA table + zerocopy), and for a Phase-0 `DISPLAY_CLEAR` payload renders on the
GPU and DMA-writes the frame back to the guest scanout address, raising the completion
MSI-X. The `infinigpu-pipeline-demo` binary drives the real backend in-process and verifies
the whole chain on the **A5000**:

```
guest rings command-ring-0 doorbell (submits DISPLAY_CLEAR)…
replay GPU: NVIDIA RTX A5000 (NVIDIA_PROPRIETARY)
seqno 1: rendered 256x256 on the GPU → scanout 0x80100000
completion MSI-X fired: true
scanout[0,0] in guest RAM = [0, 153, 204, 255]  (expected [0, 153, 204, 255])
OK — the guest's ring submission rendered on the GPU and the frame was DMA-written back
```

This fuses **abi (wire format) + device (DMA/decode/MSI-X) + replay (physical GPU)** into one
working datapath — the entire Phase-0 host side, minus the guest OS. What remains for a true
guest→GPU loop is the guest driver.

### 2026-07-16 (guest side) — real guest kernel enumerates the device; driver built

- **Guest enumeration verified.** `scripts/guest-enumerate.sh` direct-kernel-boots a real
  Linux kernel under our QEMU with the device attached (host kernel + a busybox initramfs,
  no distro image needed) and the guest kernel reports
  `0000:00:03.0 vendor=0x1b36 device=0x0110 class=0x038000` — our device, correct display
  class, on the guest PCI bus.
- **Guest driver written + compiled.** `guest/linux/infinigpu.c` (+ `Makefile`) is a plain
  PCI driver that binds `1b36:0110` and runs an in-kernel **self-test** in `probe()`: map
  BAR0, check `DEV_MAGIC`, build a one-entry command ring in coherent DMA memory, submit a
  `DISPLAY_CLEAR`, and verify the host rendered it on the GPU and DMA-wrote the frame back.
  Builds cleanly to `infinigpu.ko` against the 6.14 headers. Added a pollable
  `CMD_RING0_RETIRED` register so this first test syncs without needing MSI-X in the guest.
  `scripts/guest-driver-test.sh` boots it and checks `dmesg` for `SELFTEST: PASS` — ready to
  run; needs a readable copy of the matching host kernel (one `sudo install` — see the script).
- **cbindgen ABI header (Step 2 tail).** `scripts/gen-abi-header.sh` regenerates
  `guest/include/infinigpu_abi.h` (the wire structs) from `infinigpu-abi` and compiles
  `guest/include/abi_conformance.c`, whose `_Static_assert`s pin the C layout to the Rust
  ABI — the cross-language drift guard (mirrors infiniservice's HMAC cross-lang test).

### 2026-07-16 — 🎯 FULL GUEST→GPU LOOP CLOSED (Phase-0 objective met)

`scripts/guest-driver-test.sh` boots a real Linux guest, loads `infinigpu.ko`, and its
in-kernel self-test passes end-to-end:

```
[guest] infinigpu 0000:00:03.0: magic=0x49475055 abi=0x1 caps=0x1c
[host]  replay GPU: NVIDIA RTX A5000 (NVIDIA_PROPRIETARY)
[host]  seqno 1: rendered 256x256 on the GPU → scanout 0x28c0000
[guest] INFINIGPU-SELFTEST: PASS retired=1 scanout[0]=[0,153,204,255]
```

A real guest-kernel driver submits a command through our device; the host decodes it,
renders on the **physical A5000**, DMA-writes the frame back into guest RAM, and the guest
verifies the pixels — the whole point of the project, working through our own stack.

**Load-bearing fix — `x-no-posted-writes=true` is mandatory.** Without it the guest's BAR
MMIO writes desync the protocol (QEMU "unexpected reply"/"bad header size" → read timeout →
broken pipe): QEMU posts MMIO writes by default (no reply expected) but the `vfio_user`
v0.1.3 server always replies to REGION_WRITE. Enumeration (SeaBIOS) doesn't hit it; a guest
driver does immediately. **`infinization`'s `QemuCommandBuilder.addInfinigpuDevice()` must
include `x-no-posted-writes`** (until the crate honors the posted-write flag). Recorded in
`ERRATA`-style project memory.

### Immediate next steps

- **Step 1 (device):** write the `infinigpu-device` vfio-user `ServerBackend` against
  v0.1.3 (config space, BAR0 regs, sparse-mmap index page, `dma_map` interval table,
  MSI-X). Testable **without QEMU** first via an in-process `Client`↔`Server` loopback,
  then against the real QEMU once built.
- **Step 5+ (replay):** add a shader triangle (SM execution) and export the rendered
  blob as a **dma-buf**; then wire the ring decoder so a `SUBMIT_CMD` payload drives it.
- **Step 3 (guest):** minimal C DRM/KMS driver, tested in a Fedora/Ubuntu Infinibay VM.
