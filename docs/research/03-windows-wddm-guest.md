# 03 — Building Our Own Windows Guest GPU Driver (WDDM / IddCx)

**Scope:** what we concretely have to implement inside a Windows guest to (a) put a
desktop on screen, (b) expose a framebuffer, and (c) eventually get 3D — and which of
those is the sane *first* milestone for infinigpu. Blunt version up front, evidence below.

## TL;DR / recommendation

- A Windows GPU driver is **two cooperating pieces**: a kernel-mode display **miniport (KMD)**
  implementing `DxgkDdi*` callbacks, and a user-mode display driver **(UMD)** implementing the
  Direct3D DDI. `dxgkrnl.sys` (the DirectX Graphics Kernel) sits between them and owns the
  video-memory manager (`VidMm`) and GPU scheduler (`VidSch`).
  [[MS: WDDM architecture]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/windows-vista-and-later-display-driver-model-architecture)
- **There are three ascending tiers of difficulty**, and they are *not* the same driver:
  display-only (scanout), 2D framebuffer accel, and full 3D. The jump to tier 3 is the whole ballgame.
- **The single most important finding for infinigpu:** Microsoft's **Indirect Display Driver
  (IddCx)** lets us ship a *user-mode* virtual-monitor driver that presents a real monitor to
  the guest and hands us every composited desktop frame as a DirectX surface to encode/stream —
  **with no kernel miniport at all**. It is dramatically simpler and safer than a WDDM miniport,
  and it is exactly the shape a remote-rendered VDI wants for the *display/pixel-delivery* half.
  [[MS: IDD overview]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview)
- **But be honest about what IddCx does NOT do:** it delivers *already-composited* frames. It
  gives guest applications **zero 3D acceleration**. Time-slicing the A5000 for in-guest D3D is a
  *separate, much larger* project (a full paravirtual WDDM render driver). IddCx is milestone 1,
  not the finish line.
- **Recommended first milestone: IddCx display-only driver + host/remote streaming.** Rust is
  viable for it (real precedent exists), and UMDF isolation means a driver bug can't BSOD the guest.

---

## 1. WDDM architecture: what a Windows GPU driver actually is

WDDM splits every graphics driver into a **user-mode display driver (UMD)** — a DLL the Direct3D
runtime loads in-process, implementing the D3D user-mode DDI — and a **kernel-mode display
miniport driver (KMD)** that talks to `dxgkrnl` and the hardware. A hardware vendor **must supply
both**.
[[MS: WDDM architecture]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/windows-vista-and-later-display-driver-model-architecture)

`Dxgkrnl` is the kernel graphics core. It brokers all communication and contains the **display
port driver**, the **video memory manager `VidMm`**, and the **GPU scheduler `VidSch`** (shipped
in `dxgmms2.sys` for WDDM 2.0+, `dxgkrnl.sys` for everything else — D3DKMT calls, modes, GPU
virtualization, power).
[[MS: WDDM architecture]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/windows-vista-and-later-display-driver-model-architecture)
The KMD advertises its callbacks by filling a `DRIVER_INITIALIZATION_DATA` (or the display-only
variant) with `DxgkDdi*` function pointers at `DriverEntry`.
[[MS: DriverEntry of display miniport]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/driverentry-of-display-miniport-driver)

The three ascending tiers we care about:

**(a) Display + modeset (the display-only miniport, "KMDOD").** The minimum to light up a screen.
Microsoft ships a reference **Kernel-Mode Display-Only Miniport Driver (KMDOD)** sample that
"implements most of the DDIs that a display-only miniport driver should provide." It fills
`KMDDOD_INITIALIZATION_DATA` and calls `DxgkInitializeDisplayOnlyDriver`. It **cannot do 3D
rendering and cannot do GPU scheduling**; it assumes a **linear framebuffer** (VESA or a UEFI GOP
framebuffer) and presents by **blitting into that framebuffer**. It requires WDDM 1.2.
[[MS: KMDOD sample]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/driverentry-of-display-miniport-driver)
[[MS: KMDOD README]](https://github.com/microsoft/Windows-driver-samples/blob/main/video/KMDOD/README.md)
This is still a **kernel driver** — a bug here BSODs the guest, and it needs kernel-grade signing.

**(b) 2D framebuffer with a paravirtual command channel.** Same KMDOD skeleton, but instead of a
dumb VESA buffer you wire the present/blt path to a virtual device's command ring (e.g. virtio-gpu
transfer/flush commands). Still no 3D. This is what the shipping virtio-gpu Windows driver is
(see §2).

**(c) Full 3D / DirectX.** A full WDDM miniport that participates in `VidMm`/`VidSch`: it must
implement GPU memory management (allocations, paging, GPU virtual address translation), DMA buffer
submission, command scheduling, fences/synchronization, and pair with a **D3D UMD** that
translates DXGI/D3D11/D3D12 into hardware (or paravirtual) command buffers. This is an
order-of-magnitude larger effort than (a)/(b) and is where every real GPU vendor spends its
engineering. There is no cheap version of tier (c).

## 2. How existing paravirtual GPUs expose themselves to Windows (references)

**virtio-gpu (viogpu / virtio-gpu-wddm-dod).** The production Windows driver in
`virtio-win/kvm-guest-drivers-windows` is a **KMDOD (Display-Only) driver** — it implements
Microsoft's DOD model with `VioGpuDod` DDI callbacks, a `VioGpuAdapter` hardware-abstraction
layer, VidPN (mode) management, and a present pipeline; it binds to PCI `VEN_1AF4&DEV_1050`.
[[DeepWiki: VioGPU]](https://deepwiki.com/virtio-win/kvm-guest-drivers-windows/7-graphics-driver-(viogpu))
Crucially, **the upstream Windows virtio-gpu driver is display-only — full 3D/DirectX is an open
feature request, not shipped.**
[[virtio-win #773: add full 3D driver]](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/773)
A separate `utmapp/virtio-gpu-wddm-dod` fork exists, and community 3D-accel builds (VirGL-based)
are experimental and fragile.
[[utmapp/virtio-gpu-wddm-dod]](https://github.com/utmapp/virtio-gpu-wddm-dod/blob/main/viogpu.sln)
Takeaway: even the flagship open paravirtual GPU only gives Windows a **display-only** driver in
practice. That is a strong signal about where the achievable bar is.

**QXL/Spice** is a 2D-oriented paravirtual card: it offloads **2D** operations to the Spice client
and has no modern 3D story; it's increasingly considered legacy versus virtio-gpu.
[[kraxel: display devices in QEMU]](https://www.kraxel.org/blog/2019/09/display-devices-in-qemu/)
**VMware SVGA-II** (`vmsvga`) is a more capable virtual card but requires VMware Tools' guest
driver and is prone to version-mismatch crashes under QEMU.
[[Phoronix: QEMU VGA/GPU options]](https://www.phoronix.com/news/QEMU-VGA-GPU-Options)

**The DOD (Display-Only Driver) minimal path** is therefore the well-trodden road: a kernel
miniport that does modeset + framebuffer present and nothing else. It's the reference for tier
(a)/(b) above and is exactly what virtio-gpu's Windows driver is.

## 3. IddCx — the Indirect Display Driver shortcut (the key finding)

The **Indirect Display Driver (IDD)** model is "a **simple user-mode driver model** to support
monitors that aren't connected to traditional GPU display outputs," and its **first-named scenario
is "streaming the display output over a network to a remote client (remote display)"** and
"creating virtual monitors for virtual desktop environments." That is *literally* our VDI use
case.
[[MS: IDD overview]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview)

What it is and does:

- An IDD is a **UMDF (user-mode) driver** using the **IddCx** class extension. It **doesn't
  support kernel-mode components** and **runs in Session 0** as an isolated host process, so *"any
  driver instability doesn't affect the stability of the system as a whole"* — a bug can't BSOD
  the guest.
  [[MS: IDD overview]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview)
- It creates a graphics adapter, reports **real monitors** connecting/disconnecting, supplies
  EDID/mode descriptions, and can support **hardware mouse cursor, gamma, I2C, and protected
  content (HDCP)**.
  [[MS: IDD overview]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview)
- The OS composes the desktop and hands the IDD frames through an **`IDDCX_SWAPCHAIN`** (multi-
  buffered: OS composes into one buffer while the driver reads another). Each frame arrives as a
  **DirectX surface** the driver can process with **any DirectX API** — perfect for feeding a
  hardware H.264/H.265/AV1 encoder and shipping pixels to a remote client.
  [[MS: IddCx objects]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/iddcx-objects)
- `IddCxAdapterSetRenderAdapter` lets the driver pick, by **DXGI LUID**, *which* GPU the OS uses to
  **compose** the swapchains; the actual adapter used is reported back in
  `EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN`.
  [[MS: IddCxAdapterSetRenderAdapter]](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/nf-iddcx-iddcxadaptersetrenderadapter)

What it explicitly **cannot** do — read this twice:

- The driver **must not call GDI, windowing APIs, OpenGL, or Vulkan**.
  [[MS: IDD overview]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview)
- **IddCx provides NO 3D acceleration to guest applications.** It delivers frames that have
  *already been composited* by DWM. The "render adapter" it selects is whatever GPU (or the
  software **WARP** rasterizer, if the guest has no GPU) DWM/apps use to draw — IddCx does not
  create that GPU. So in a guest with no other display device, an IddCx desktop is composed **on
  the CPU (WARP)**: fine for 2D/desktop/office/browser workloads, useless for accelerated 3D.

**Is IddCx the right first milestone for a remote-rendered VDI where 3D happens host-side?**
For the **display/pixel-delivery half — yes, unambiguously.** It is the simplest possible way to
(1) get a real, correctly-EDID'd virtual monitor into a Windows guest with *our own* driver, (2)
receive every desktop frame as a GPU surface, and (3) encode/stream it — all in user mode, all
signable as a UMDF package, no BSOD risk. But note the architectural subtlety of "3D happens
host-side": guest *applications* still run **in the guest** and call D3D **in the guest**. To make
those calls hit the host A5000, you need a **render path into the guest** (a real GPU visible to
DWM/apps). IddCx alone gives you a software-composited desktop. To reach the project's actual goal
— time-sliced hardware 3D — you additionally need either a paravirtual **WDDM render driver** that
marshals D3D to the host, or a mediated device. **IddCx solves display; it does not solve GPU
sharing.** Treat it as the plumbing milestone that de-risks everything downstream.

## 4. WDDM version targets and the driver-signing reality

**Version target.** KMDOD needs WDDM ≥ 1.2; IddCx needs Windows 10 1903+ for
`SetRenderAdapter`, and modern IddCx features track newer Windows 10/11 builds. Targeting
**Windows 10 21H2+ / Windows 11** as the guest baseline is realistic.
[[MS: IddCxAdapterSetRenderAdapter]](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/nf-iddcx-iddcxadaptersetrenderadapter)

**Signing is the real gate for shipping to customers, not the code.** On 64-bit Windows,
driver packages must be signed to install cleanly. The paths:

- **Test-signing / self-signed:** works on machines with test-signing enabled (or signature
  enforcement disabled). Fine for our own dev guests; **not** shippable to customers without them
  toggling boot security. [[MS: attestation signing]](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation)
- **Attestation signing:** requires a **Partner Center / Hardware Developer Program** account and
  an **EV code-signing certificate** (~$250–$500/yr from DigiCert/Sectigo/GlobalSign). You submit
  an EV-signed CAB; **Microsoft runs automated checks and counter-signs** it — **no HLK/WHQL
  testing**. This is the pragmatic route for a self-hosted product driver.
  [[MS: attestation signing]](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation)
- **WHQL / WHCP:** full HLK test pass; only this can go to Windows Update. Overkill for us.

Two 2025/2026 realities to plan around: **(1)** Microsoft is **removing trust for the legacy
cross-signed driver program** and tightening a new **kernel trust policy** (Win11 24H2/25H2/26H1,
Server 2025) landing in the **April 2026** update — kernel-mode drivers increasingly *must* go
through the Microsoft-signing funnel.
[[MS: removing cross-signed trust]](https://techcommunity.microsoft.com/blog/windows-itpro-blog/advancing-windows-driver-security-removing-trust-for-the-cross-signed-driver-pro/4504818)
**(2)** This is *another* reason to prefer **IddCx (UMDF)** over a **KMDOD (kernel)** first: a
user-mode driver package is lower-risk and simpler to get through attestation than a kernel
miniport, and it dodges the kernel-trust tightening for the pixel-delivery layer. (We'll still
need EV + attestation to ship either one without customers disabling signature enforcement —
budget for the EV cert regardless.)

## 5. Can we write these drivers in Rust?

**Yes for IddCx/UMDF, with caveats; harder for a full kernel miniport.** Microsoft's official
`microsoft/windows-drivers-rs` provides `wdk-build`, `wdk-sys` (raw FFI), `wdk` (safe bindings)
and `wdk-panic`; `cargo-wdk` reached crates.io in Nov 2025. But it is **explicitly early / not
recommended for production**, crates.io currently ships only **KMDF v1.33** bindings (others must
be generated from source), it **still requires substantial `unsafe`**, and a **WHCP/CodeQL version
mismatch currently blocks production signing** of Rust drivers through the official path.
[[MS: Towards Rust in Windows drivers]](https://techcommunity.microsoft.com/blog/windowsdriverdev/towards-rust-in-windows-drivers/4449718)
[[The Register, 2025-09]](https://www.theregister.com/2025/09/04/rust_windows_drivers/)
[[microsoft/windows-drivers-rs]](https://github.com/microsoft/windows-drivers-rs)

The decisive precedent: **`MolotovCherry/virtual-display-rs` is a working IddCx virtual-display
driver written in Rust** — its core UMDF driver is Rust, with hand-written `wdf-umdf` and
`iddcx` Rust bindings (`rust/wdf-umdf/src/iddcx.rs`).
[[DeepWiki: virtual-display-rs]](https://deepwiki.com/MolotovCherry/virtual-display-rs)
So a Rust IddCx driver is not theoretical — it exists in the wild. Expect to **write our own
IddCx FFI bindings** (the official crates don't cover IddCx yet) and a fair amount of `unsafe`,
but the memory-safety and tooling payoff is real. A full **WDDM 3D render miniport in Rust**,
by contrast, would be pioneering work with essentially no precedent — defer it.

## Recommended minimal FIRST-milestone Windows driver shape

**Ship an IddCx display-only driver (UMDF, in Rust) that presents a virtual monitor and streams
composited frames — not a full WDDM 3D miniport, and not even a KMDOD.**

Why:

1. **It matches the VDI use case exactly** — "remote display" is IddCx's headline scenario, and it
   hands us GPU-surface frames ready to encode.
   [[MS: IDD overview]](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview)
2. **User-mode = no BSOD, easier signing.** A UMDF package sidesteps the kernel-trust tightening
   and is the cheapest thing to push through EV + attestation.
3. **Rust is proven here** (`virtual-display-rs`), so we honor the Rust-first constraint on day one.
4. **It de-risks the whole stack** — virtual-monitor lifecycle, EDID/modes, swapchain capture,
   encode, and transport to the Infinibay client — *before* we touch the genuinely hard part.
5. **It is a stepping stone, and we must say so out loud:** IddCx gives a **software-composited**
   desktop (WARP) with **no in-guest 3D**. Delivering the project's real goal — **time-sliced
   hardware 3D on the shared A5000** — requires a *later, separate* milestone: a **paravirtual
   WDDM render driver** (D3D UMD + thin KMD marshalling command buffers to a host-side renderer,
   à la virtio-gpu Venus/D3D or a GPU-PV-style paravirtual adapter). That tier-(c) work, not
   IddCx, is where the multi-quarter engineering lives.

**Sequencing:** (M1) IddCx display-only + encode/stream, software-composed desktop, our own
signed Rust driver. (M2) Optionally pair IddCx with a real render adapter in the guest so DWM/2D
apps get acceleration. (M3) The paravirtual 3D WDDM render path that actually time-slices the
A5000 — the hard, unavoidable core, to be specified separately.

## Sources

- MS — WDDM Architecture: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/windows-vista-and-later-display-driver-model-architecture
- MS — DriverEntry of Display Miniport Driver: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/driverentry-of-display-miniport-driver
- MS — KMDOD sample (samples index): https://learn.microsoft.com/en-us/samples/microsoft/windows-driver-samples/kernel-mode-display-only-miniport-driver-kmdod-sample/
- MS — KMDOD README (GitHub): https://github.com/microsoft/Windows-driver-samples/blob/main/video/KMDOD/README.md
- MS — Indirect Display Driver Model Overview: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview
- MS — IddCx Objects: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/iddcx-objects
- MS — IddCxAdapterSetRenderAdapter: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/nf-iddcx-iddcxadaptersetrenderadapter
- MS — Attestation Sign Windows Drivers: https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation
- MS — Advancing Windows driver security (removing cross-signed trust): https://techcommunity.microsoft.com/blog/windows-itpro-blog/advancing-windows-driver-security-removing-trust-for-the-cross-signed-driver-pro/4504818
- MS — Towards Rust in Windows Drivers: https://techcommunity.microsoft.com/blog/windowsdriverdev/towards-rust-in-windows-drivers/4449718
- microsoft/windows-drivers-rs (GitHub): https://github.com/microsoft/windows-drivers-rs
- The Register — Microsoft slow progress on Rust for Windows drivers (2025-09-04): https://www.theregister.com/2025/09/04/rust_windows_drivers/
- DeepWiki — MolotovCherry/virtual-display-rs (Rust IddCx driver): https://deepwiki.com/MolotovCherry/virtual-display-rs
- DeepWiki — virtio-win VioGPU graphics driver (KMDOD): https://deepwiki.com/virtio-win/kvm-guest-drivers-windows/7-graphics-driver-(viogpu)
- virtio-win #773 — add full VirtIO-GPU 3D driver (open): https://github.com/virtio-win/kvm-guest-drivers-windows/issues/773
- utmapp/virtio-gpu-wddm-dod (GitHub): https://github.com/utmapp/virtio-gpu-wddm-dod/blob/main/viogpu.sln
- kraxel — display devices in QEMU: https://www.kraxel.org/blog/2019/09/display-devices-in-qemu/
- Phoronix — QEMU VGA/GPU options for desktop virtualization: https://www.phoronix.com/news/QEMU-VGA-GPU-Options
