# ADR 0001 — Host device seam: vfio-user

- **Status:** accepted (pending Phase-0 spike validation)
- **Date:** 2026-07-16
- **Feeds from:** research/01-qemu-device-model.md, research/07-verify-device-seam.md

## Context

We must present *our own* GPU as a PCI device to each guest **without forking QEMU** (constraint
1) and without adopting an existing experimental QEMU GPU driver. The guest must see a normal PCI
device (so a WDDM/DRM driver can bind to it), and the hot path (command-ring doorbell, register
page, large DMA transfers) must not cost a UNIX-socket round-trip per access.

## Options considered

### A — vfio-user (`-device vfio-user-pci,socket=…`)
Out-of-process PCI device implemented as a separate Rust process; client merged upstream in QEMU
**10.1 (Aug 2025)**. Verified against QEMU 10.1.1 source (doc 07):
- **Direct BAR mmap** honored (`VFIO_REGION_INFO_FLAG_MMAP` → `memory_region_init_ram_device_ptr`) → register/doorbell page touched at memory speed, no socket round-trip.
- **ioeventfd doorbells** (`VFIO_USER_DEVICE_GET_REGION_IO_FDS`) → a doorbell write is a bare eventfd kick via KVM.
- **Zero-copy DMA** (`VFIO_USER_DMA_MAP` passes guest RAM as an SCM_RIGHTS memfd the server mmaps) → requires `-object memory-backend-memfd,share=on`.
- **Reset** covered (`VFIO_USER_DEVICE_RESET`). Live device-state **migration/savevm is NOT** in the minimal client.
- Pure-Rust **server** exists: rust-vmm/vfio `vfio-user` crate (`Server`, `ServerBackend`, BARs, MSI-X, DMA, region mmap), no C `libvfio-user` dependency.
- Pro: models "we are a real GPU PCI device"; production-intended; fully owned in Rust; no fork.
- Con: young client (MSI-X/hotplug edge cases unproven for a complex device); forces memfd-backed guest RAM; live migration would need us to implement VFIO migration v2.

### B — our own virtio-*style* device + vhost-user backend
Own device-ID + own rust-vmm vhost backend, hoping to reuse virtio-gpu's blob/udmabuf/dma-fence.
- **Refuted advantage** (doc 07): that machinery lives in the virtio-gpu *device model* +
  rutabaga/virglrenderer, **not** generic vhost-user — a custom virtio device gets none of it for
  free; we reimplement resource management either way. QEMU's generic `vhost-user-device-pci` is
  documented "not recommended for production." Guest needs a custom virtio driver anyway.

### C — custom in-QEMU C device
Most mature APIs, in-process speed — but a **permanent QEMU fork** in C. Rejected; violates
constraint 1. Kept only as an absolute last resort.

## Decision

**Adopt (A) vfio-user** as the host device seam, and build our command-ring/blob/fence protocol as
our own shared `no_std` Rust crate *on top* of it. Build against **QEMU ≥ 10.1.1** (the
`x-pci-class-code` regression fix, mandatory for advertising GPU class `0x030000`). The seam does
not solve the guest driver — that remains ours to write under either option.

## Consequences

- **Positive:** no QEMU fork; fully-owned Rust device server; zero-copy submit/DMA path; clean fit
  with `infinization` (add one `-device vfio-user-pci` line + a memfd memory-backend to
  `QemuCommandBuilder`; run one server process per VM with its socket in the shared sockets dir).
- **Negative / accepted:** hard product dependency on QEMU ≥ 10.1.1; guest RAM must be
  memfd/shared-backed (interacts with hugepages/NUMA/ballooning — verify); **no live migration /
  device savevm** for GPU VMs (acceptable: Infinibay uses cold migration + qcow2 disk snapshots).
- **De-risking spike (1–2 weeks) — must pass before committing:** a throwaway custom PCI device via
  rust-vmm `Server`/`ServerBackend` (BAR0 doorbell+status, 1 MSI-X vector, `DMA_MAP` mmap of a
  memfd), launched under QEMU 10.1.1 with `memory-backend-memfd,share=on` + `vfio-user-pci`, proving:
  (a) doorbell is an eventfd kick not a socket msg; (b) a 256 MB buffer is server-visible zero-copy
  via mmap not `DMA_READ`; (c) MSI-X round-trips to the guest; (d) reset on reboot; (e) a running-VM
  qcow2 disk snapshot still succeeds with the device attached; (f) `savevm` fails cleanly.
- **Fallback:** re-home the same Rust `ServerBackend` onto vhost-user behind
  `vhost-user-device-pci` (accepting dev-only status + a custom guest virtio driver) only if the
  young vfio-user client bites.
- **Revisit if:** the QEMU client proves too immature for GPU MSI-X/hotplug, or a hard live-migration
  requirement appears.

## Corrections (review 2026-07-16)

- **⚠️ ioeventfd doorbell is NOT free with the chosen crate.** The QEMU 10.1 *client* supports
  `GET_REGION_IO_FDS`, but the rust-vmm/vfio `vfio-user` **server** crate (v0.1.x) returns
  `UnsupportedCommand` for it (and for `DmaRead/DmaWrite`) — so a doorbell would trap as a
  `REGION_WRITE` socket round-trip. **Decide among:** (1) contribute `GET_REGION_IO_FDS` emission to the
  crate; (2) use C `libvfio-user` (SPDK's ioeventfd path) via FFI; or (3) the fallback: **one *batched*
  doorbell per submission** served as a trapped `REGION_WRITE` (keeps the socket trip off the
  per-command path). The Phase-0 spike must prove which. (Earlier text/doc 07 wrongly called this
  "confirmed shipping".)
- **PCI ID must change.** The placeholder `1B36:0100` **collides with QXL** (the in-tree `qxl` DRM
  driver binds `PCI_DEVICE(0x1b36,0x0100)`), and `1B36:0001` is the QEMU PCI-PCI bridge. Pick a DEV id
  unallocated in QEMU `docs/specs/pci-ids.rst`; obtain a real PCI-SIG vendor ID before GA. Binding is by
  exact VEN/DEV, so a colliding DEV is not benign (docs 24/25/28 to update).
- **No socket-DMA fallback exists** with the Rust crate (it rejects `DmaRead/DmaWrite`): any IOVA not
  covered by an active `DMA_MAP` is **fail-closed** (drop command + ring error), not a slow read. Every
  ring base + `ATTACH_BACKING` page list must be fully `DMA_MAP`-covered. The consume loop must
  **bounds-check** TAIL and every descriptor IOVA/len against the interval map before dereference (hostile
  guest → OOB read otherwise).
- **Memory: GPU-VMs are non-overcommittable.** All guest RAM is `DMA_MAP`-pinned/shared, so
  **virtio-balloon must be disabled** and overcommit is unsafe (a ballooned page is still server-mapped
  and can race DMA) — not just KSM loss (RISKS S6). `hugetlb=on` on the memfd is compatible.
- **Fail-fast QEMU gate.** `VMLifecycle`/`QemuCommandBuilder` must preflight `qemu --version ≥ 10.1.1`
  and probe `-device vfio-user-pci,help` for `x-pci-class-code`, aborting with a clear error if unmet.
- **MSI-X: use few vectors, not 64.** MSI carries no payload (completion is the shared seqno word), so
  the MVP uses 1 vector + a per-ring "pending" bitmap the ISR scans; scale to per-ring vectors only if
  measured, and verify the crate's SET_IRQS fd-buffer capacity first (RISKS S5).

Full review log: [`../ERRATA.md`](../ERRATA.md). Failure-mode walkthroughs: [`../SCENARIOS.md`](../SCENARIOS.md).
