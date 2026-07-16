# ADR 0004 — Wire protocol & shared Rust crate

- **Status:** accepted
- **Date:** 2026-07-16
- **Feeds from:** research/11-wire-protocol-design.md, research/06-data-plane-and-host-gpu.md, research/05-rust-driver-ecosystem.md

## Context

The same host device must serve a Linux guest (Vulkan) and a Windows guest (IddCx display + later
D3D/WDDM). We want one protocol/codebase where honest, a `no_std`-clean shared Rust crate reusable
across the Linux kernel module, the Windows driver, and the std host backend. The protocol layers
**above** the host device seam (ADR 0001) so it is seam-agnostic.

## Decision

**A payload-agnostic *envelope* protocol, multi-ring, with a shared `no_std` Rust ABI crate.**

- **"One protocol" is honest only at the envelope layer.** Framing/resource/fence/scanout is
  OS- and API-neutral (virtio-gpu is the existence proof). `SUBMIT_CMD` carries an **opaque,
  encoding-tagged** payload (`VULKAN_VENUSLIKE | D3D12_DDI | DXGI_PRESENT`); framing never parses it.
  The Vulkan and D3D command sub-protocols **share zero marshalling code** — two codegen pipelines.
- **Ring topology:** **one control ring per device** (ordered lifecycle/negotiation) **+ N
  per-context command rings** (high-freq opaque submission). This corrects Wave-1's "one command
  ring": gfxstream's 1:1 encoder/decoder threading was *the* scalability fix over VirGL's single
  decode thread — so the ABI is multi-ring from day one even if the MVP runs one.
- **Completion = seqno, not a second ring:** each command ring publishes a monotonic submission
  seqno + a host-written retired-seqno word (Venus `vn_ring` model); fences compare against it.
- **Message classes** (our enum values, our device, cribbed from virtio-gpu):
  `NEGOTIATE`/`GET_CAPSETS` (capset bitmap `CAP_VULKAN`/`CAP_D3D12`/`CAP_DISPLAY_ONLY`),
  `CTX_CREATE`/`ATTACH_RING`/`DESTROY`, `RESOURCE_CREATE_BLOB` (mem GUEST/HOST3D/HOST3D_GUEST) +
  `MAP_BLOB`/`ATTACH_BACKING`, `SUBMIT_CMD`, fences, `SET_SCANOUT_BLOB`/`RESOURCE_FLUSH`/`CURSOR`,
  `RESET`/`EVENT`.
- **Serialization = justified hybrid:** `zerocopy` (Miri/Kani-verified, `no_std`, compile-time
  layout validation) for fixed framing in shared memory; codegen'd opaque payloads memcpy'd through;
  `postcard` (`no_std` serde, stable 1.0 wire) only for low-frequency variable-shape control/
  negotiation. Reject bincode (wire churn) and postcard on the descriptor hot path.
- **Shared crate layout:**
  - `infinigpu-abi` — `#![no_std]`, no alloc, `repr(C)` + `zerocopy` wire types.
  - `infinigpu-ring` — `#![no_std]`, SPSC + seqno, loom-tested memory ordering.
  - `infinigpu-proto` — `#![no_std]` + alloc, `postcard` control messages.
  - codegen'd sibling encoders/decoders (registry-driven: `vk.xml` for Vulkan, model Venus's
    venus-protocol generator / gfxstream Cereal).
  - feature-gated glue: `kernel` (MMIO `writel` + kernel alloc), `windows` (`WRITE_REGISTER` +
    `wdk-alloc`), `std` (host backend).
- **Transport trait** (`ring_memory`/`doorbell`/`completion_signal`) abstracts vfio-user (sparse
  mmap BAR + ioeventfd + MSI-X) vs a virtio-style device, so the crate survives an ADR-0001 fallback.
- **Forward-compat:** ABI major.minor in the ring header; capset negotiation; reserved trailing
  padding; TLV `{type,length}` message headers for skip-unknown; new commands gated behind negotiated
  capset/feature bits.

## Consequences

- **Positive:** one ABI source-of-truth across host + both guests; safe reinterpretation of hostile
  guest bytes via `zerocopy` (not ad-hoc `transmute`); multi-ring scalability baked into the ABI;
  adding API coverage is a registry edit.
- **Negative / accepted:** two full per-API codegen pipelines (Vulkan now, D3D later) — "one
  protocol" is an envelope claim, not a command-layer claim; the Windows D3D-over-KVM-ring payload is
  **unproven** (GPU-PV is VMBus-only) and is scheduled separately (ADR 0005 M3/M4).
- **Follow-up:** SPSC ring memory-ordering must be loom/property-tested (a wrong fence is a silent
  data race). The Rust crate cannot literally link into a C Linux KMD (ADR 0005) — it is exported to
  C via a cbindgen header + round-trip conformance test.
