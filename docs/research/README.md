# infinigpu — research corpus

Deep-research phase (2026-07-16). Each doc is written by a focused researcher against primary
sources and ends with a `## Sources` section; Wave-2/3 agents are adversarial.

**Read order:** [`99-synthesis.md`](99-synthesis.md) → [`../decisions/`](../decisions/) (ADR 0001–0011)
→ [`../RISKS.md`](../RISKS.md) → [`../ROADMAP.md`](../ROADMAP.md) → [`../PHASE-0-PROTOTYPE.md`](../PHASE-0-PROTOTYPE.md).

## Wave 1 — foundational survey (🟢 6/6)
| # | Doc | Outcome |
|---|-----|---------|
| 01 | [01-qemu-device-model.md](01-qemu-device-model.md) | seam → **vfio-user** |
| 02 | [02-gpu-sharing-taxonomy.md](02-gpu-sharing-taxonomy.md) | only viable class → **API-remoting** |
| 03 | [03-windows-wddm-guest.md](03-windows-wddm-guest.md) | Windows → **IddCx display-first** |
| 04 | [04-linux-drm-guest.md](04-linux-drm-guest.md) | Linux → **DRM/KMS** (virtio-gpu 2D model) |
| 05 | [05-rust-driver-ecosystem.md](05-rust-driver-ecosystem.md) | **Rust/C split** + toolchain |
| 06 | [06-data-plane-and-host-gpu.md](06-data-plane-and-host-gpu.md) | transport, replay, multiplexing |

## Wave 2 — adversarial verification + deep dives (🟢 6/6)
| # | Doc | Outcome |
|---|-----|---------|
| 07 | [07-verify-device-seam.md](07-verify-device-seam.md) | **vfio-user confirmed** (QEMU 10.1.1) |
| 08 | [08-windows-wddm-render-deep.md](08-windows-wddm-render-deep.md) | render miniport bounded (~6–8mo) |
| 09 | [09-presentation-latency.md](09-presentation-latency.md) | NVENC uncapped on A5000; latency budget |
| 10 | [10-security-isolation.md](10-security-isolation.md) | per-VM jailed process; reset residual |
| 11 | [11-wire-protocol-design.md](11-wire-protocol-design.md) | multi-ring envelope; no_std crate |
| 12 | [12-rust-linux-drm-verify.md](12-rust-linux-drm-verify.md) | Linux KMS **must be C** |

## Wave 3 — red-team + deepest-risk deep dives (🟢 3/3)
| # | Doc | Outcome |
|---|-----|---------|
| 13 | [13-redteam-showstoppers.md](13-redteam-showstoppers.md) | **NO-GO as commodity SLA / GO on principle** (→ RISKS.md) |
| 14 | [14-fence-sync-design.md](14-fence-sync-design.md) | deadlock **solvable** (seqno+timeline, 8 rules) → ADR 0006 |
| 15 | [15-windows-m3-ddi-concrete.md](15-windows-m3-ddi-concrete.md) | Windows 3D **precedented** (fork viogpu3d + DXVK) |

## VDI specialization (🟢 2/2)
| # | Doc | Outcome |
|---|-----|---------|
| 16 | [16-vdi-workload-and-host-scheduler.md](16-vdi-workload-and-host-scheduler.md) | host brain: capacity/scheduler → ADR 0007 |
| 17 | [17-guest-intelligence-and-video-offload.md](17-guest-intelligence-and-video-offload.md) | cooperative guest + NVDEC video offload |

## Custom remote protocol "infiniPixel" (🟢 2/2)
| # | Doc | Outcome |
|---|-----|---------|
| 18 | [18-remote-protocol-display-datapath.md](18-remote-protocol-display-datapath.md) | NVENC-HEVC/QUIC/WebCodecs, ~14–22ms → ADR 0009 |
| 19 | [19-remote-protocol-io-and-integration.md](19-remote-protocol-io-and-integration.md) | input/audio/clipboard/USB + SPICE migration |

## Perceptual / HVS layer (🟢 2/2)
| # | Doc | Outcome |
|---|-----|---------|
| 22 | [22-perceptual-hvs-compression.md](22-perceptual-hvs-compression.md) | temporal+structural first, then foveated QP-map → ADR 0009 |
| 23 | [23-perceived-latency-and-adaptive-control.md](23-perceived-latency-and-adaptive-control.md) | local cursor, prediction, adaptive loop |

## Vendor-agnostic (NVIDIA/AMD/Intel) (🟢 2/2)
| # | Doc | Outcome |
|---|-----|---------|
| 20 | [20-vendor-agnostic-host-hal.md](20-vendor-agnostic-host-hal.md) | `GpuBackend`, 4 seams → ADR 0008 |
| 21 | [21-cross-vendor-media-codec.md](21-cross-vendor-media-codec.md) | `MediaCodec`, Vulkan Video default → ADR 0008 |

## QEMU device implementation spec (🟢 2/2)
| # | Doc | Outcome |
|---|-----|---------|
| 24 | [24-qemu-device-implementation-spec.md](24-qemu-device-implementation-spec.md) | **register-level codeable spec** (config space, BAR map, vfio-user msg handling, DMA, MSI-X, argv, lifecycle) → the PHASE-0 device |
| 25 | [25-device-mechanics-book-grounding.md](25-device-mechanics-book-grounding.md) | archive-grounded PCI/DMA/MSI-X/mmap + KVM device-model mechanics (page-cited from the driver books) |

## Client-side GPU offload / split-rendering (🟢 2/2)
| # | Doc | Outcome |
|---|-----|---------|
| 26 | [26-client-offload-split-rendering.md](26-client-offload-split-rendering.md) | delegate compose/super-res/cached-tiles/interp to the client GPU → ADR 0010 |
| 27 | [27-client-capability-negotiation-and-resilience.md](27-client-capability-negotiation-and-resilience.md) | capability negotiation + adaptive split + thin-client fallback + loss resilience |

## Client-delegation execution protocol (🟢 2/2)
| # | Doc | Outcome |
|---|-----|---------|
| 30 | [30-client-delegation-instruction-set-and-wire.md](30-client-delegation-instruction-set-and-wire.md) | 1-byte-opcode instruction set (Guacamole/RDP-EGFX) + control+per-frame-sidecar wire + epoch frame-binding → ADR 0011 |
| 31 | [31-client-delegation-negotiation-latency-bandwidth.md](31-client-delegation-negotiation-latency-bandwidth.md) | 8-state negotiation + per-task latency + signaling bandwidth (0.3–6%) + AIMD re-delegation → ADR 0011 |

## First-hand archive reads (hand-read by me, page-cited)
| # | Doc | Outcome |
|---|-----|---------|
| 28 | [28-guest-pci-driver-handread.md](28-guest-pci-driver-handread.md) | guest PCI driver bring-up verified vs Madieu Ch11 (p535–553); refines doc 24 (`pci_set_master`, `pci_iomap_range`) |
| 29 | [29-dma-and-kvm-device-model-handread.md](29-dma-and-kvm-device-model-handread.md) | DMA ring model + KVM device model verified vs Madieu (p554–566) + Mastering KVM (p64–73) |

## Synthesis & reference
- [99-synthesis.md](99-synthesis.md) — consolidated architecture.
- [../reference/books-catalog.md](../reference/books-catalog.md) — 82-book corpus mapped by relevance.
- [../reference/books-deep-notes.md](../reference/books-deep-notes.md) — impl detail mined from the high-value books (page-cited).
