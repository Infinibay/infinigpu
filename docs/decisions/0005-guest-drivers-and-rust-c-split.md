# ADR 0005 — Guest driver strategy & Rust/C split

- **Status:** accepted
- **Date:** 2026-07-16
- **Feeds from:** research/03-windows-wddm-guest.md, research/04-linux-drm-guest.md, research/05-rust-driver-ecosystem.md, research/08-windows-wddm-render-deep.md, research/12-rust-linux-drm-verify.md

## Context

We write our own guest drivers for both OSes against our custom PCI device (ADR 0001), speaking our
protocol (ADR 0004). We want Rust where feasible (constraint 4), but the kernel-graphics ecosystems
constrain us. "Share the GPU" means both **display/remoting** (pixels out) and **3D acceleration**
(guest GPU work on the host GPU) — these have very different difficulty and should be sequenced apart.

## Decision — Linux guest

- **Display driver (milestone 1) = C.** At Linux 7.2-rc3, upstream Rust DRM has **only** render/buffer
  abstractions (device/driver/file/gem/gpuvm/ioctl); there is **no upstream Rust KMS/atomic-modeset**
  (only Lyude Paul's out-of-tree RVKMS WIP RFC). A display driver is ~entirely KMS, so it **cannot** be
  pure upstream Rust. (VGEM-in-Rust is render-only and does **not** prove KMS-in-Rust.)
- Model it on virtio-gpu's 2D path: `drm_simple_display_pipe` + `drm_gem_shmem` + dumb buffers +
  atomic helpers, targeting **current stable kernels** (the C DRM/KMS UAPI is version-independent).
- **3D (later) = C Mesa userspace** (a Venus-style Vulkan ICD / Gallium driver). No pure-Rust Mesa.
- **The Rust shared crate (ADR 0004) is the ABI source-of-truth**, exported to the C KMD via a
  **cbindgen-generated header** (structs/constants) + a round-trip conformance test. RfL modules are
  Rust-or-C; you cannot link an arbitrary Rust staticlib into a C kernel module, so no literal reuse.
- Defer any Rust in the guest kernel to a future render/3D node on kernel ≥ 7.2; even then KMS stays
  C until the KMS bindings merge upstream.

## Decision — Windows guest (four milestones)

Microsoft GPU-PV (guest UMD unchanged, `dxgkrnl` marshals the WDDM DDI to the host) is **VMBus-only**
and unusable on KVM, so we ship our own driver(s). Sequenced by difficulty and signing burden:

- **M1 — IddCx display-only, Rust (UMDF).** Virtual monitor, capture composited frames, encode,
  stream. Pixels only, **zero in-guest 3D** (WARP/software compositing). User-mode → no BSOD, lighter
  signing, dodges the April-2026 kernel-trust tightening. Proven by `virtual-display-rs`. **Ships a
  complete office/knowledge-worker VDI indefinitely.**
- **M2 — user-mode Vulkan/GL ICD, Rust, no kernel driver.** Remotes *native* Vulkan/GL guest apps to
  the host arbiter. Cannot accelerate D3D or the DWM desktop (only a WDDM adapter appears to the D3D
  runtime), but covers native GPU apps cheaply.
- **M3 — WDDM render miniport, C/C++ (mandatory only here).** When a persona needs hardware D3D or a
  GPU-composited desktop. Cheapest viable form: a thin **command-remoting** miniport whose DMA buffers
  are **opaque blobs** pushed to the host (no guest VidMm/MMU to model → ~a dozen DDI callbacks:
  `DxgkDdiCreateAllocation`/`SubmitCommand`/`BuildPagingBuffer`/`Render`/`Present`/`Patch`/
  `InterruptRoutine`), paired with a Mesa Gallium UMD over a virglrenderer/gfxstream-style pipe
  (crib max8rr8's viogpu3d + UTM "Neptune", ~6–8-month precedent), capped at ~D3D10/OpenGL. **No Rust
  precedent for a render miniport** → expect C/C++.
- **M4 — modern D3D11/12, via a guest DXVK/vkd3d UMD emitting Vulkan** over a Venus-style channel +
  the M3 adapter for DXGI enumeration. The real frontier; scheduled separately.

## Rust/C split (summary)

| Component | Language | Why |
|---|---|---|
| Host arbiter / replay / renderer | **Rust** | ecosystem ready (`ash`/`vulkano`, `gbm.rs`/`drm-rs`); hostile-input decoder in Rust |
| Shared ABI / ring / proto crate | **Rust `no_std`** | one source-of-truth for host + both guests |
| vfio-user device server | **Rust** | rust-vmm/vfio server crate |
| Linux guest KMS display driver | **C** | no upstream Rust KMS (ADR-0004 crate exported via cbindgen) |
| Linux guest 3D UMD | **C** | Mesa Gallium/Vulkan ICD |
| Windows IddCx display driver | **Rust** | UMDF, own bindings (virtual-display-rs) |
| Windows Vulkan/GL ICD | **Rust** | user-mode |
| Windows WDDM render miniport + Gallium UMD | **C/C++** | no Rust precedent; signing funnel |

## Consequences

- **Positive:** each guest ships a useful product early (Linux 2D desktop; Windows office VDI on M1)
  long before the hard 3D work; Rust is used everywhere the ecosystem permits.
- **Negative / accepted:** the guest KMS layer and all Windows 3D are C/C++; an EV cert + Partner
  Center attestation are required from M1 (Windows), plus full WHCP/HLK for the M3 kernel miniport.
- **Revisit if:** upstream Rust KMS bindings (RVKMS) merge (Linux driver can move to Rust); or a
  windows-drivers-rs render-miniport path with a working signing funnel appears.

## Corrections (review 2026-07-16)

- **M2 wording: "no WDDM render miniport", not "no kernel driver".** On Windows a pure user-mode ICD
  cannot map the ADR-0001 PCI device's BARs/rings/MSI — that needs a **minimal KMDF/WDM function driver**
  (still attestation-signed, but no `dxgkrnl` render-miniport surface). Reword M2 accordingly; the Linux
  side is correctly asymmetric (both display and 3D go through the C KMD conduit).
- **M1 (Windows IddCx) pixel path to the host encoder is explicit:** the IddCx driver writes swapchain
  frames into an ADR-0001 **shared memfd** exposed by that same minimal function driver, and the host
  arbiter NVENC-encodes + streams via infiniPixel — preserving ADR-0009's host-side encode (a driverless
  in-guest software-encode is an explicit exception, not the default).
- **M3 reintroduces in-guest kernel-crash (BSOD) risk** that M1/M2 avoid — a miniport code fault
  bugchecks that **one** VM (blast radius = one VM); keep the miniport minimal (payload-agnostic bucket A)
  and harden `DxgkDdiRestartFromTimeout`/`ResetEngine`; add host-side auto-restart of a bugchecked guest.
- **Host NVIDIA-driver ↔ guest Venus version skew** (unhandled): a rolling host driver update can
  invalidate the boot-time-negotiated Vulkan extension set and silently break DXVK/vkd3d. Add a
  **host↔guest Vulkan capability re-handshake on host-driver change** (or pin/stage host driver rollouts
  per fleet); on loss of an advertised extension raise a recoverable `VK_ERROR_DEVICE_LOST` (guest
  recreates) and quarantine until re-handshake. **M4** advertises to DXGI only the feature levels
  DXVK/vkd3d actually cover over remoted Vulkan (clamp + WARP fallback, don't fail hard).
- **CI-gate the cbindgen ABI** conformance (struct-layout round-trip) so the C KMD header can't drift
  from the Rust wire crate (mirrors infiniservice's cross-language HMAC test).

Full review log: [`../ERRATA.md`](../ERRATA.md). Failure-mode walkthroughs: [`../SCENARIOS.md`](../SCENARIOS.md).
