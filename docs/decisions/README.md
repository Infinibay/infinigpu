# Architecture Decision Records (ADRs)

One file per significant, hard-to-reverse decision. Format: context → options considered →
decision → consequences. Numbered sequentially. Superseded ADRs stay in place and link forward.

| # | Decision | Status |
|---|----------|--------|
| [0001](0001-host-device-seam-vfio-user.md) | Host device seam → **vfio-user** (own Rust device server, no QEMU fork) | ✅ accepted (pending Phase-0 spike) |
| [0002](0002-core-sharing-model-api-remoting.md) | Core sharing model → **userspace API-remoting** (Vulkan-first) | ✅ accepted |
| [0003](0003-process-topology-and-isolation.md) | Process topology → **one jailed replay process per VM** + per-host broker | ✅ accepted |
| [0004](0004-wire-protocol-and-shared-crate.md) | Wire protocol → **payload-agnostic multi-ring envelope**, `no_std` Rust ABI crate | ✅ accepted |
| [0005](0005-guest-drivers-and-rust-c-split.md) | Guest drivers & **Rust/C split** (Linux C-KMS; Windows 4-milestone) | ✅ accepted |
| [0006](0006-cross-vm-fence-sync-design.md) | **Cross-VM fence/sync** — deadlock-free (seqno+timeline+8 rules) | ✅ accepted |
| [0007](0007-vdi-capacity-manager-and-scheduler.md) | **VDI capacity manager & scheduler** (the intelligent "brain") | ✅ accepted |
| [0008](0008-vendor-agnostic-host-abstraction.md) | **Vendor-agnostic** host abstraction (NVIDIA/AMD/Intel) | ✅ accepted |
| [0009](0009-infinipixel-remote-protocol.md) | **infiniPixel** custom low-latency remote protocol + perceptual layer | ✅ accepted |
| [0010](0010-client-side-offload-split-rendering.md) | **Client-side GPU offload / split-rendering** (exploit the client PC's GPU) | ✅ accepted |
| [0011](0011-client-delegation-execution-protocol.md) | **Client-delegation execution protocol** (instruction set, wire, frame-binding, negotiation) | ✅ accepted |

Supporting capstones: [`../RISKS.md`](../RISKS.md) (go/no-go + risk burndown),
[`../ROADMAP.md`](../ROADMAP.md) (build sequence), and
[`../PHASE-0-PROTOTYPE.md`](../PHASE-0-PROTOTYPE.md) (the MVP that validates ADR 0001–0006).

Template: [`0000-template.md`](0000-template.md).
