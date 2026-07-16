# 08 — Windows in-guest 3D: how unavoidable and how large is the WDDM render driver?

**Scope:** pressure-test the Wave-1 claim that *"in-guest hardware 3D on Windows requires a
from-scratch WDDM UMD+KMD render pair and is the biggest unknown."* This doc goes deep on
precedent, the concrete DDI surface, what IddCx+WARP actually covers, effort/language/signing, and
recommends a Windows sequencing.

## Verdict

**PARTIALLY-CONFIRMED.** The *render pair is genuinely unavoidable* — on KVM/QEMU you cannot borrow
Microsoft's cheap GPU-PV path (it is welded to Hyper-V VMBus), so accelerating in-guest D3D *does*
require our own WDDM render miniport (KMD) + D3D/Gallium UMD. But the *"biggest unknown"* framing is
too pessimistic: it is a **known-hard, precedented, bounded** problem with at least four reference
implementations on non-Hyper-V hypervisors, one (**UTM Neptune**) actively built *for QEMU in 2026*
with a stated 6–8-month estimate — and the Mesa/Gallium + `virglrenderer` reuse path removes most of
the from-scratch burden. The residual true unknown is narrower than "a WDDM driver": it is *modern
D3D11/12 over our NVIDIA-Vulkan-remoting host*.

---

## 1. Precedent: paravirtual WDDM **render** drivers exist off Hyper-V (this refutes "no precedent")

Wave 1 implied the only render-capable Windows paravirtual GPU is Microsoft's Hyper-V-only GPU-PV.
That is wrong. Concrete render-miniport precedents on non-Hyper-V hypervisors:

- **`max8rr8`'s viogpu3d (virtio-win PR #943, 2023→still open in 2026).** A **full render miniport**
  (not display-only) for the **KVM/QEMU** virtio-gpu device: kernel-mode WDDM driver plus user-mode
  `viogpu_d3d10.dll` (a **Gallium-based D3D10 UMD**) and `viogpu_wgl.dll` (OpenGL/WGL), backed by a
  patched **`virglrenderer`/VirGL** host. It self-describes as WDDM 1.3+, "has rendering glitches and
  might crash," has **no preemption**, and breaks on Electron/VSCode (missing
  `PIPE_QUERY_TIMESTAMP_DISJOINT`). Experimental, unmerged — but it is a working existence proof that a
  render miniport runs on plain KVM/QEMU. [PR #943]
- **UTM "Neptune" (blog dated 2026): "Direct3D virtualization for QEMU."** Actively-developed effort
  to get accelerated graphics into a **Windows guest on QEMU** by implementing the long-missing
  **VirGL/Gallium guest driver for Windows** (host stays `virglrenderer`, "already a mature protocol
  for transferring Gallium between guest and host"), with **gfxstream** as an alternative transport.
  Original estimate: **6–8 months** including learning curve. This is the single most important data
  point: a serious 2026 project is doing *exactly* our Windows tier-3 milestone on QEMU, and scoping
  it in months, not years. [UTM Neptune]
- **VirtualBox 7.0+ (2022).** Ships a guest **VBoxWDDM** driver that advertises a 3D-capable virtual
  adapter; guest **Direct3D 11 is serviced by running D3D over DXVK (D3D→Vulkan)** on the host side.
  Not Hyper-V, not KVM, but proves the *shape*: guest WDDM render adapter → tunnel → host D3D-on-Vulkan
  translation. [Phoronix VBox7] [VBox manual §4.5]
- **VMware SVGA 3D WDDM (`vm3dmp`).** Production WDDM render miniport paired with a **Gallium `svga`
  UMD**; VMware authored **D3D10/11 Gallium state trackers for Windows**. The canonical reference for
  "WDDM render miniport + Gallium UMD + host renderer." [Mesa svga3d]
- **OpenXT `xc-windows` `xengfx` WDDM miniport** — a Xen WDDM miniport in C, another non-Hyper-V data
  point (display-leaning, but real dxgkrnl miniport code to read). [OpenXT xengfx]

**Upstream reality check (supports Wave 1's caution):** the *shipping* virtio-gpu Windows driver is
still **display-only (KMDOD)**; full 3D remains the unmerged experimental PR above. So while precedent
exists, none of it is "install and forget." The bar is "hard research problem with maps," not
"uncharted." **This downgrades "biggest unknown" but confirms "unavoidable and large."**

## 2. Why KVM cannot borrow the cheap path — GPU-PV is VMBus-locked (this *confirms* the render pair)

Microsoft's own paravirtualization is astonishingly *thin on the guest* — and that is exactly why we
can't have it. Per the primary MS doc [GPU-PV]:

> "There's no KMD in the guest, only UMD. The **Virtual Render Device (VRD)** KMD replaces the KMD.
> VRD's purpose is to facilitate the loading of *Dxgkrnl*… There's no video memory manager (*VidMm*)
> or scheduler (*VidSch*) in the guest. *Dxgkrnl* in a VM gets thunk calls and **marshalls them to the
> host partition via VM bus channels**."

So in GPU-PV the guest **UMD is unchanged**, there is **no real guest KMD**, and **`dxgkrnl.sys`
itself does the D3DKMT marshalling** — but only over **Hyper-V VMBus** to `vmwp.exe`/`vrdumed.dll` on
the host. The transport is baked into Microsoft's closed `dxgkrnl`; "the current paravirtualization
implementation uses the VM bus." There is **no public seam** to redirect that marshalling to a KVM
host. GPU-PV also needs Hyper-V plumbing (`Add-VMGpuPartitionAdapter`, `HostDriverStore` copy, IO
space) that does not exist under QEMU.

**Consequence:** on KVM we must supply the pieces GPU-PV omits — a **real guest render miniport
(KMD)** *and* a **real D3D/Gallium UMD** — because Microsoft's "guest is a stub" trick is unusable
without their hypervisor. The Wave-1 core claim's "from-scratch WDDM UMD+KMD render pair" is therefore
**correct and load-bearing**, for a precise reason: not because Windows demands a KMD in theory
(GPU-PV proves it doesn't), but because the *only* KMD-less path is proprietary and Hyper-V-exclusive.

## 3. The concrete minimal DDI surface (and the command-remoting subset)

Verified against the MS "WDDM Operation Flow" doc [WDDM flow]. A render device drives this ordered
path; **KMD = `DxgkDdi*`, UMD = D3D user-mode DDI**:

**User-mode display driver (UMD) entry points a D3D app exercises:**
`CreateDevice` → `pfnCreateContextCb` (create GPU context/command buffer) → `CreateResource`
(→ runtime `pfnAllocateCb`) → drawing DDIs (`DrawPrimitive2`, state/shader DDIs) →
`Present`/`Flush` → runtime `pfnPresentCb`/`pfnRenderCb`. For a *remoting* UMD you do **not** author
bespoke D3D11/12 translation: you make the UMD a **Gallium winsys** and let Mesa's Gallium state
trackers produce the D3D10/OpenGL, serialized as VirGL/gfxstream — this is precisely the
max8rr8/Neptune shape and the cheapest UMD.

**Kernel render miniport (KMD) callbacks, minimal render path:**
1. `DxgkDdiCreateDevice` (returns `DXGK_DEVICEINFO`), and context creation.
2. `DxgkDdiCreateAllocation` — describe/allocate the surfaces (the runtime's `pfnAllocateCb` lands
   here). **The three core render callbacks are `DxgkDdiCreateAllocation`, `DxgkDdiSubmitCommand`,
   `DxgkDdiBuildPagingBuffer`.** [OSR/MS]
3. `DxgkDdiRender`/`DxgkDdiRenderKm` (or `DxgkDdiPresent` for present) — "validate the command
   buffer, write a DMA buffer in the hardware's format, produce an allocation list."
4. `DxgkDdiBuildPagingBuffer` — build paging DMA buffers to move allocations to GPU-accessible memory
   ("isn't called for every frame").
5. `DxgkDdiSubmitCommand` (queue paging buffer, then the DMA buffer, each carrying a **fence id**),
   `DxgkDdiPatch` (assign physical addresses).
6. `DxgkDdiInterruptRoutine` on GPU completion → `DxgkCbNotifyInterrupt` + `DxgkCbQueueDpc`.

**The command-remoting simplification is the key insight:** because the *host* does the real
rendering, our KMD's "DMA buffer in the hardware's format" is just an **opaque command blob forwarded
over our ring**; there is *no real GPU MMU/VidMm to model in the guest*. `DxgkDdiPatch`/paging degrade
to near-trivial when allocations are pinned/host-visible, and `DxgkDdiSubmitCommand` reduces to "push
blob, record fence," with completion signalled when the host acks (a *virtual* interrupt, not a real
one). This is why max8rr8's KMD can skip preemption and why the DDI count that actually *matters* is
small (~a dozen render callbacks + the VidPN/display side). GPU-PV even makes its highest-frequency
thunks (`D3DKMTSubmitCommand`, sync-object signal/wait) **asynchronous** VM-bus messages — the same
batching trick we'll want on our ring. Still: it *is* a kernel dxgkrnl miniport, with all the paging
(`DXGKDDI_BUILDPAGINGBUFFER`), fence/sync, and VidPN modeset scaffolding that implies — not a weekend.

## 4. Can IddCx-display + WARP ship real VDI for a long time? Mostly yes — with sharp edges

**What the "no render miniport" configuration actually is:** an IddCx UMDF virtual monitor (Wave-1
milestone 1) presents the desktop; with no render adapter present, **DWM composites the desktop on
WARP (the CPU software D3D rasterizer)** — this is exactly the MS-documented default ("Hyper-V Video
adapter is paired by default with the **BasicRender** adapter"). WARP is *fully conformant* D3D up to
**feature level 12_2 / DirectX 12 Ultimate** on Win11, JIT-compiled to SSE/AVX. [WARP]

**What ships fine, indefinitely, on IddCx+WARP:** the whole desktop shell, Office, browsers doing 2D,
PDF, RDP-grade productivity, 1080p **software** video playback. For an *office/knowledge-worker* VDI
persona this is a complete product and the render miniport is **not needed for a long time**.

**What breaks without in-guest D3D (be precise about "apps run their GPU work where?"):**
- **GPU-heavy D3D apps (CAD viewports, 3D, games, GPU-accelerated DirectX canvas).** They *run* on
  WARP (so they don't crash on an adapter check) but at **CPU software speed** — "very slow compared
  to any modern accelerator." Unusable for real 3D/CAD. Some titles also reject the WARP/Basic-Render
  adapter outright.
- **OpenGL apps.** With no ICD, Windows falls back to the **GDI OpenGL 1.1 software** rasterizer —
  broken for anything modern. Their GPU work has **nowhere to go** unless we also ship a GL ICD.
- **Vulkan apps.** There is **no software Vulkan** shipped by default (no lavapipe) — Vulkan apps
  simply **fail to find a device**.
- **Hardware video decode/encode, DirectML/AI compute, CUDA/OpenCL** — all unavailable/CPU-only.

**The critical architectural point for infinigpu's "3D happens host-side" thesis:** guest apps still
call their graphics API **in the guest**. A pure user-mode **Vulkan/OpenGL ICD** that remotes to the
host arbiter *can* accelerate **native Vulkan/GL apps with no kernel driver at all** — a genuinely
cheaper partial win worth banking. **But it cannot accelerate D3D or the DWM desktop**, because the
D3D runtime enumerates adapters from `dxgkrnl`, and only a **WDDM render adapter (=a KMD)** can appear
there. On Windows, **D3D is the dominant API and DWM is D3D** — so "GPU-accelerated Windows desktop /
D3D apps" is **mandatorily** gated on the render miniport. **IddCx+WARP defers, it does not replace.**

## 5. Effort, language, and the April-2026 signing regime

**Effort.** Neptune's **6–8 months** for a QEMU Windows Gallium/OpenGL guest driver is the best public
proxy — call it **2–4 engineer-quarters** for a *first* glitchy D3D10/GL render path reusing
`virglrenderer`, on top of the IddCx display work. Modern **D3D11/12** is materially more (VMware spent
years on its D3D10/11 Gallium trackers; VirtualBox punted to DXVK). Our extra twist: our host is
**NVIDIA Vulkan API-remoting**, but VirGL is *Gallium/OpenGL*-shaped — so to hit modern D3D we'd want a
**guest D3D-on-Vulkan (DXVK/vkd3d) UMD emitting Vulkan** over a Venus-style channel, i.e. a Vulkan ICD
*plus* the WDDM adapter for DXGI enumeration. **That D3D11/12-over-Vulkan-remoting piece is the real
residual unknown**, narrower than "a whole WDDM driver."

**Language.** Every render precedent (max8rr8 viogpu, Neptune, VBoxWDDM, `vm3dmp`, xengfx) is **C/C++**,
and Mesa Gallium UMDs are **C**. `microsoft/windows-drivers-rs` is early and **KMDF-only** — it has no
`dxgkrnl`/display-miniport bindings, so a WDDM render miniport in Rust means hand-generating all
`d3dkmddi` bindings + heavy `unsafe`, with **zero precedent** *(Wave-1 claim, NEEDS VERIFICATION but
consistent with what I found)*. Realistic split: **render KMD + Gallium UMD in C/C++**; keep Rust for
the host arbiter, the shared ring/protocol crate, and the **IddCx display driver** (where
`MolotovCherry/virtual-display-rs` is a working Rust precedent — *Wave-1 claim, NEEDS VERIFICATION*).

**Signing (verified, 2026).** The **April 2026 Windows update** rolls out (in *evaluation mode* first)
a kernel-trust policy on **Win11 24H2/25H2/26H1 + Server 2025** that **removes default trust for
cross-signed kernel drivers** and pushes everything through **WHCP** (Partner Center registration, EV
cert, Microsoft-signed catalogs or **attestation** signatures). [MS cross-signed] [WindowsForum] This
**tightens the screws on exactly the kernel render miniport** and **reinforces Wave 1's sequencing**:
- **IddCx (UMDF, user-mode)** dodges the kernel-trust tightening entirely — cheapest to sign, no BSOD
  risk. Ship it first.
- The **render KMD** must go through EV + Partner Center attestation (or full WHCP/HLK); **budget the
  EV cert regardless**, and expect the kernel path to be the signing-heavy one. Dev/test uses
  `TESTSIGNING`.

## 6. Recommended Windows sequencing

1. **M1 — IddCx display-only + host encode/stream (Rust).** Complete, shippable **office/knowledge
   VDI** on WARP-composited desktops. This is *not* a throwaway stepping stone — it is a real product
   for a large persona, and it de-risks EDID/modes/swapchain/encode. **IddCx suffices as long as the
   customer doesn't need hardware 3D/GL/Vulkan/video-decode.**
2. **M2 — user-mode Vulkan/GL ICD that remotes to the host arbiter (no kernel driver).** Cheap partial
   acceleration for *native* Vulkan/OpenGL guest apps; validates the ring/arbiter end-to-end on Windows
   before touching kernel code. Does **not** accelerate D3D or the desktop.
3. **M3 — the render miniport becomes MANDATORY here:** the moment the persona needs
   **hardware-accelerated D3D or a GPU-composited desktop** (CAD, 3D, GPU-accelerated browsers/apps,
   real "GPU VDI"). **Cheapest viable form:** a thin **command-remoting WDDM render miniport (C/C++)**
   whose "DMA buffers" are opaque blobs pushed to the host, paired with a **Mesa Gallium UMD** over a
   `virglrenderer`/gfxstream-style pipe — cribbing the max8rr8/Neptune architecture rather than writing
   bespoke D3D UMDs. Cap expectations at **D3D10/OpenGL** initially.
4. **M4 — modern D3D11/12** via a guest **DXVK/vkd3d UMD emitting Vulkan** remoted through our NVIDIA
   host context. This is the genuine frontier and should be scoped separately.

**Bottom line:** the WDDM render pair is unavoidable for real GPU-VDI on Windows and is a multi-quarter
C/C++ effort — but it is *precedented and bounded*, not the project's biggest unknown. IddCx+WARP
carries office VDI for a long time; the render miniport is the price of admission only for
hardware-3D personas, and its cheapest form leans hard on Mesa/Gallium + virglrenderer to avoid
writing D3D from scratch.

## Sources

- MS — GPU paravirtualization (VMBus-only; guest has no KMD/VidMm/VidSch; async D3DKMT submit): https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization
- MS — WDDM Operation Flow (full DxgkDdi + UMD DDI call sequence): https://learn.microsoft.com/en-us/windows-hardware/drivers/display/windows-vista-and-later-display-driver-model-operation-flow
- MS — DXGKDDI_BUILDPAGINGBUFFER: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_buildpagingbuffer
- MS — Submitting a Command Buffer: https://learn.microsoft.com/en-us/windows-hardware/drivers/display/submitting-a-command-buffer
- virtio-win PR #943 — viogpu3d full render miniport + D3D10/WGL UMD on KVM/QEMU (VirGL), experimental: https://github.com/virtio-win/kvm-guest-drivers-windows/pull/943
- UTM — "Introducing Neptune: Direct3D virtualization for QEMU" (2026; VirGL/Gallium Windows guest driver, gfxstream, 6–8mo estimate): https://blog.getutm.app/2026/introducing-neptune-direct3d-virtualization-for-qemu/
- UTM — Graphics.md (Windows virtio-gpu guest driver incomplete): https://github.com/utmapp/UTM/blob/main/Documentation/Graphics.md
- Mesa — VMware SVGA3D guest GL driver (Gallium svga state trackers): https://docs.mesa3d.org/drivers/svga3d.html
- Phoronix — VirtualBox 7.0 D3D11 via DXVK (guest WDDM + host D3D-on-Vulkan): https://www.phoronix.com/news/VirtualBox-7.0-Released
- Oracle — VirtualBox manual §4.5 Hardware-Accelerated Graphics (WDDM guest driver, host tunnel): https://docs.oracle.com/en/virtualization/virtualbox/6.0/user/guestadd-video.html
- OpenXT — xengfx WDDM miniport (Xen, C): https://github.com/OpenXT/xc-windows/blob/master/xengfx/wddm/miniport/xengfxwd.c
- MS — DirectX WARP guide (software D3D up to FL 12_2): https://learn.microsoft.com/en-us/windows/win32/direct3darticles/directx-warp
- MS — Advancing Windows driver security: removing trust for cross-signed drivers (April 2026): https://techcommunity.microsoft.com/blog/windows-itpro-blog/advancing-windows-driver-security-removing-trust-for-the-cross-signed-driver-pro/4504818
- WindowsForum — April 2026 kernel trust change / WHCP enforcement (evaluation mode, 24H2/25H2/26H1): https://windowsforum.com/threads/april-2026-windows-update-ends-cross-signed-kernel-driver-trust.410487/
- MS — Indirect Display Driver model overview (IddCx, user-mode, remote display scenario): https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview
- microsoft/windows-drivers-rs (KMDF-focused, early): https://github.com/microsoft/windows-drivers-rs
- MolotovCherry/virtual-display-rs (Rust IddCx precedent — NEEDS VERIFICATION): https://github.com/MolotovCherry/virtual-display-rs
