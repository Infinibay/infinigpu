# 15 — Windows M3 WDDM render miniport: a concrete DDI implementation sketch

**Scope:** turn ADR-0005's M3 line item ("thin command-remoting WDDM render miniport, opaque DMA
blobs, ~a dozen DDIs, crib max8rr8/Neptune") into a real map: the exact `DxgkDdi*` callback set,
how each maps to "push an opaque blob over our ring" with **no guest VidMm/MMU**, which user-mode
driver pairs with our **Vulkan** host arbiter (ADR-0002/0003), how paging degenerates, the
preemption gap, and the April-2026 signing reality.

**Verdict: FEASIBLE-BUT-RISKY.** The kernel miniport is *smaller than it looks* and now
**source-verified** against a working KVM/QEMU render miniport (max8rr8's `viogpu3d`): the render
half is ~1,500 lines of C++ and its "GPU command submission" is literally `memcpy` + push-blob +
raise-synthetic-interrupt. The real risk is **not** the KMD — it is the UMD/host pairing, because
the cheap precedents (max8rr8, VBox, Neptune) all render on **GL/DXVK hosts**, not a Vulkan-only
arbiter. Getting D3D onto *our* Vulkan replay process is the residual unknown, and it is an M4-shaped
problem we can either fold in or defer.

## 1. The exact minimal DDI set (verified against max8rr8/viogpu3d `driver.cpp`)

max8rr8's `DriverEntry` fills one `DRIVER_INITIALIZATION_DATA` (full render miniport, **not** the
display-only `KMDDOD_INITIALIZATION_DATA`) and calls `DxgkInitialize`. The registered callbacks split
cleanly into **three buckets** [max8rr8 driver.cpp]:

**A. Render-path core (the only genuinely GPU-specific code — ~6 callbacks):**
`DxgkDdiCreateDevice` / `DxgkDdiCreateContext` (per-D3D-device + per-engine context handles) →
`DxgkDdiCreateAllocation` / `DxgkDdiDescribeAllocation` / `DxgkDdiDestroyAllocation` (surfaces) →
`DxgkDdiRender` (+ `DxgkDdiPresent`) → `DxgkDdiPatch` → `DxgkDdiSubmitCommand` →
`DxgkDdiBuildPagingBuffer` → `DxgkDdiInterruptRoutine` / `DxgkDdiDpcRoutine`. Plus the fence/reset
plumbing: `DxgkDdiQueryCurrentFence`, `DxgkDdiPreemptCommand`, `DxgkDdiResetEngine`,
`DxgkDdiRestartFromTimeout`, `DxgkDdiCancelCommand`, `DxgkDdiCollectDbgInfo`.

**B. VidPN / display scaffolding (mandatory but boilerplate, cribbed from KMDOD — ~12 callbacks):**
`DxgkDdiIsSupportedVidPn`, `RecommendFunctionalVidPn`, `EnumVidPnCofuncModality`, `CommitVidPn`,
`UpdateActiveVidPnPresentPath`, `SetVidPnSourceAddress`, `SetVidPnSourceVisibility`,
`RecommendMonitorModes`, `QueryVidPnHWCapability`, `SystemDisplayEnable/Write`, pointer shape/position,
`StopDeviceAndReleasePostDisplayOwnership`.

**C. PnP/power lifecycle (pure WDF boilerplate — ~12 callbacks):** `AddDevice`, `StartDevice`,
`StopDevice`, `RemoveDevice`, `DispatchIoRequest`, `SetPowerState`, `QueryChildRelations/Status`,
`QueryDeviceDescriptor`, `QueryAdapterInfo`, `Escape`, `Unload`, `ResetDevice`.

So "~a dozen DDIs that matter" (ADR-0005) is accurate: **bucket A is the driver**; B and C are copied
from the shipping virtio-gpu KMDOD and the WDK KMDOD sample almost verbatim. If M1 already ships an
IddCx (or a KMDOD) path, buckets B/C are largely done.

## 2. How each render callback maps to "push an opaque blob over our ring"

This is the crux, and max8rr8's source makes it concrete — there is **no real GPU MMU, no VidMm
participation, no GPU VA translation** to model. Verified mappings:

| DDI | What a real GPU driver does | What the command-remoting miniport does (verified) |
|---|---|---|
| `DxgkDdiRender` | Validate UM command buffer, translate to HW DMA format, emit patch list | **`memcpy`** the UMD's command stream (a series of `{VIOGPU_COMMAND_HDR, body}`) from the paged UM buffer into the kernel DMA buffer inside `__try/__except`; write an all-zero patch-location list. No translation — the bytes are already the host protocol. [device.cpp `VioGpuDevice::Render`] |
| `DxgkDdiPatch` | Write physical GPU addresses into the DMA buffer | **No-op.** `VioGpuCommander::Patch` is `UNREFERENCED_PARAMETER(pPatch); return STATUS_SUCCESS;` — there are no physical addresses to patch. [command.cpp] |
| `DxgkDdiSubmitCommand` | Kick the GPU ring, record fence | Record `SubmissionFenceId` + the `[StartOffset,EndOffset)` window of the DMA buffer, enqueue a `VioGpuCommand`, return **asynchronously**. [command.cpp `SubmitCommand`/`PrepareSubmit`] |
| *(worker thread)* | — | Walk the DMA blob; for each `VIOGPU_CMD_SUBMIT` copy the body and call `ctrlQueue.SubmitCommand(blob, size, ctxId, cb)` — **the actual push onto the virtio control ring**. `VIOGPU_CMD_TRANSFER_TO/FROM_HOST` become resource-transfer ring ops. [command.cpp `Run`] |
| `DxgkDdiInterruptRoutine` | Real MSI on GPU completion | Never fires from hardware. On ring-drain the driver **synthesizes** completion: `DXGKARGCB_NOTIFY_INTERRUPT_DATA{ InterruptType = DXGK_INTERRUPT_DMA_COMPLETED, SubmissionFenceId }` via `NotifyInterrupt`, then `CommandFinished`. dxgkrnl retires the fence exactly as if hardware had signalled. [command.cpp `Run`] |
| `DxgkDdiBuildPagingBuffer` | Fill/transfer/GPU-VA-map DMA operations | Handles **only** `MAP_APERTURE_SEGMENT` / `UNMAP_APERTURE_SEGMENT`; everything else returns `STATUS_NOT_SUPPORTED`. Map just attaches the MDL backing and sets a fake linear "physical" address (`OffsetInPages * PAGE_SIZE`). [driver.cpp + allocation.cpp `MapApertureSegment`] |

**Mapping onto our stack (ADR-0004):** `ctrlQueue.SubmitCommand` → our per-context command ring's
`SUBMIT_CMD` with an opaque, encoding-tagged payload; the synthetic `DXGK_INTERRUPT_DMA_COMPLETED`
→ our seqno-completion model (host writes retired-seqno, guest compares and raises the WDDM interrupt
when `retired >= SubmissionFenceId`). max8rr8 pushes over a virtio ctrl queue; we push over our
vfio-user command ring. **The KMD is payload-agnostic** — it never parses the blob — so it is reusable
essentially verbatim regardless of whether the payload is VirGL, Venus-Vulkan, or D3D12-DDI (exactly
what ADR-0004's envelope layer promised).

## 3. Allocations & paging degrade to near-trivial

max8rr8 models the adapter as a **single memory segment** (`SegmentId0 = 1`), every allocation
`CpuVisible = TRUE`, `EvictionSegmentSet` set so the aperture is not used for eviction, and
`SupportedRead/WriteSegmentSet = 0b1`. `CreateAllocation` just records size + a private
`VIOGPU_CREATE_ALLOCATION_EXCHANGE` (width/height/format/virtio resource id); `DescribeAllocation`
returns the format and *admits* "these values are RANDOM" for multisample/refresh. There is no real
VidMm residency dance because the host owns the actual pixels — the guest allocation is a **handle +
a host-side resource id**, and `TRANSFER_TO/FROM_HOST` ring ops move bytes when the guest genuinely
needs CPU access. For us this is even cleaner: with `memory-backend-memfd,share=on` (ADR-0001) the
allocation backing is a shared memfd the host maps zero-copy, so most `TRANSFER_*` ops vanish. Net:
**CreateAllocation/DescribeAllocation/DestroyAllocation + a two-case BuildPagingBuffer + a no-op
Patch** is the entire "memory manager." This is the single biggest reason M3 is bounded.

## 4. User-mode driver: which UMD pairs with our Vulkan arbiter

Three candidate UMD shapes, and only one reuses our arbiter cheaply:

- **(A) Mesa Gallium D3D10/WGL UMD (the literal max8rr8 path).** `viogpu_d3d10.dll` +
  `viogpu_wgl.dll` are Gallium state trackers whose winsys serializes **VirGL**; the host is a patched
  **virglrenderer/vrend (OpenGL)**. This is the cheapest UMD to stand up because Mesa writes the D3D10/GL
  for you — **but its host is GL, not Vulkan.** It does **not** reuse our Vulkan replay arbiter; it
  would require standing up a *second* host renderer (virglrenderer) that contradicts ADR-0002's
  Vulkan-first decision. Reuse: **low.**
- **(B) A guest Vulkan ICD (our M2) as the UMD command source, D3D via guest-side DXVK/vkd3d.** The
  WDDM adapter makes a Vulkan-capable adapter appear to DXGI; a guest Vulkan ICD encodes **Venus**
  (ADR-0004's `VULKAN_VENUSLIKE` payload) into the same blob the KMD pushes; **DXVK (D3D9/10/11) and
  vkd3d-proton (D3D12) run as ordinary guest user-mode DLLs on that Vulkan ICD**, translating D3D→Vulkan
  *in the guest*. The host is **our existing Vulkan replay process, unchanged.** Reuse: **maximal** —
  this is the shape that collapses M3+M4's host into the arbiter we already have.
- **(C) VBox/Neptune "DXVK on the host" shape.** VirtualBox 7's `VBoxWDDM` guest ships a D3D command
  stream; the host runs **DXVK-Native → Vulkan**. UTM **Neptune** (2026) is the same idea for QEMU: its
  own debugging notes name the pipeline **mesa (guest wrapper) → neptune-protocol (encoder) →
  virglrenderer (host stub) → dxvk (host D3D11-on-Vulkan)**, with the stated north-star of "DirectX
  working through DXVK." This *does* end on Vulkan, but puts DXVK/virglrenderer on the host — more host
  surface than (B) and a translation hop we don't need if DXVK lives in the guest. Reuse: **medium.**

**Recommendation:** pair the max8rr8 **KMD skeleton** with a **(B) Venus-Vulkan UMD + in-guest
DXVK/vkd3d**. The KMD is reused from a working precedent; the UMD is our M2 Vulkan ICD; D3D falls out
of DXVK/vkd3d for free (the same components Proton ships); the host is our arbiter with **zero new
renderer**. The price is that D3D correctness now depends on DXVK/vkd3d's coverage over *remoted*
Vulkan — real, but a known quantity, and Venus has carried DXVK/Zink since 2023.

## 5. The scheduling / preemption gap and how the arbiter compensates

max8rr8 **disables preemption**: `DxgkDdiPreemptCommand` logs "UNSUPPORTED PREEMPTION FUNCTION" and
returns `STATUS_SUCCESS` without preempting, and it sets a registry flag to disable OS preemption
system-wide. Its "scheduler" is `#define VIOGPU_MAX_RUNNING 1` — a worker thread that keeps exactly
**one DMA buffer in flight**, strict FIFO, next one dispatched only after the host acks the previous.
This is a **cooperative, non-preemptible, single-context-at-a-time** model.

For a single guest that is fine (dxgkrnl still time-slices *contexts* at DMA-buffer granularity across
the queue). The exposure is **cross-tenant**: with N VMs each pushing non-preemptible blobs, fairness
and TDR (2-second GPU timeout) live entirely **host-side**. This is precisely where ADR-0003's
topology earns its keep: one **jailed replay process per VM** with its own Vulkan context/queues, plus
a **per-host broker holding quota/policy**. The broker does the scheduling the guest miniport refuses
to: bounded per-VM in-flight blobs, per-VM submission rate/queue-depth limits, and — because a single
guest's runaway shader can still wedge a shared queue — Vulkan-level watchdogs (per-submit timeouts,
`VK_ERROR_DEVICE_LOST` handling) that fault **one** replay process rather than the box. **Residual
(ADR-0003, unchanged):** a severe GA102 Xid fault (79/45/62/48/119) forces a device-wide GPU reset →
all tenants drop; no guest-side or arbiter-side scheduling fixes that on unlicensed GA102. Keep
`RestartFromTimeout`/`ResetEngine` honest so a guest TDR recovers its own context cleanly instead of
BSODing.

## 6. Signing & logistics under the April-2026 regime (verified — and less scary than feared)

The April-2026 change is narrower than doc 08 implied. It removes trust for the **legacy cross-signed
driver program** — kernel drivers signed with your own EV cert chaining to a cross-signed root and
installed *without* Microsoft counter-signing. It does **not** kill **attestation signing**: you still
submit an **EV-signed CAB** through **Partner Center / Hardware Dev Center**, Microsoft runs automated
checks and **counter-signs** (no HLK/WHQL lab pass), and the resulting Microsoft-signed catalog installs
on locked-down Win11 24H2/25H2/26H1 + Server 2025. An **EV code-signing certificate is a prerequisite**
for both attestation and full WHCP [MS cross-signed; MS code-signing-reqs]. So the M3 kernel miniport
path is: **EV cert → Partner Center attestation submission → MS-signed catalog.** Dev/test uses
`bcdedit /set testsigning on` with our self-signed cert. Practical consequences:

- The kernel-trust tightening **reinforces the sequencing**, not blocks it: M1 IddCx (UMDF) and M2
  Vulkan ICD (user-mode) dodge the kernel funnel entirely; only M3 pays the attestation tax.
- **No Rust for the miniport.** `windows-drivers-rs` is KMDF-only with no `dxgkrnl`/display-miniport
  bindings and a still-broken production-signing funnel; every render precedent (viogpu3d, Neptune,
  VBoxWDDM, vm3dmp, xengfx) is **C/C++**. M3 KMD + any bespoke UMD glue is **C/C++**; keep Rust for the
  arbiter, the shared ABI crate, and the M1/M2 user-mode drivers (ADR-0005 unchanged).

## 7. Effort estimate & the cheapest viable M3

**Cheapest viable M3 shape:** fork max8rr8's `viogpu3d` KMD; keep buckets B/C nearly verbatim; retarget
bucket A's ring push from virtio to our vfio-user command ring (ADR-0004); make the payload **Venus-encoded
Vulkan** from our M2 guest Vulkan ICD; run **DXVK + vkd3d-proton in the guest** for D3D; render on our
**existing Vulkan arbiter** — no virglrenderer, no GL host, no bespoke D3D UMD DDI. Cap the first cut at
`VIOGPU_MAX_RUNNING`-style single-in-flight, no preemption, D3D11 via DXVK; add D3D12/vkd3d and deeper
queue-depth later.

**Effort (engineering-quarters, greenfield-with-precedent):**
- KMD render miniport (fork + reseam to our ring + fence/TDR correctness + attestation): **~1.5–2.5 EQ.**
- Guest Vulkan ICD / Venus encoder as the UMD source (**largely M2**, reused here): **~2–3 EQ** if not
  already done for M2; near-zero incremental if M2 shipped.
- DXVK/vkd3d integration + D3D conformance shakeout over *remoted* Vulkan: **~1–2 EQ** (the residual
  unknown; bounded by DXVK/vkd3d maturity, not by us writing D3D).
- Signing/logistics (EV cert, Partner Center, catalog plumbing): **~0.25 EQ + calendar time.**

Total **~3–5 EQ on top of M2**, consistent with Neptune's independently-stated 6–8 months for a
comparable QEMU Windows effort — and materially cheaper than the naive "write D3D UMDs from scratch"
reading, because DXVK/vkd3d *is* the D3D UMD and max8rr8 *is* the KMD.

**Bottom line:** the M3 kernel miniport is a **bounded, source-precedented, C/C++** job whose GPU-facing
code is copy-blob + synthetic-interrupt; paging and patching are near-empty; preemption is skipped and
compensated by the host broker (ADR-0003). The one decision that actually shapes the architecture is
UMD pairing: choosing an **in-guest DXVK/vkd3d-on-Venus-Vulkan** UMD makes M3 reuse our Vulkan arbiter
with **no second host renderer**, folds the "modern D3D" M4 host into M3, and confines the real risk to
DXVK/vkd3d coverage over remoted Vulkan.

## Sources

- max8rr8 viogpu3d — `driver.cpp` (full `DRIVER_INITIALIZATION_DATA` DDI registration, render miniport): https://github.com/max8rr8/kvm-guest-drivers-windows/blob/viogpu3d/viogpu/viogpu3d/driver.cpp
- max8rr8 viogpu3d — `viogpu_command.cpp` / `.h` (SubmitCommand/Patch no-op, worker `VIOGPU_MAX_RUNNING 1`, `Run` blob push, synthetic `DXGK_INTERRUPT_DMA_COMPLETED`): https://github.com/max8rr8/kvm-guest-drivers-windows/blob/viogpu3d/viogpu/viogpu3d/viogpu_command.cpp
- max8rr8 viogpu3d — `viogpu_device.cpp` (`Render` memcpy of UM stream into DMA buffer, `Present`): https://github.com/max8rr8/kvm-guest-drivers-windows/blob/viogpu3d/viogpu/viogpu3d/viogpu_device.cpp
- max8rr8 viogpu3d — `viogpu_allocation.cpp` (single CpuVisible segment, `BuildPagingBuffer` MAP/UNMAP only, `DescribeAllocation`): https://github.com/max8rr8/kvm-guest-drivers-windows/blob/viogpu3d/viogpu/viogpu3d/viogpu_allocation.cpp
- virtio-win PR #943 — viogpu3d (no preemption, WDDM 1.3, Win10 22H2, glitchy/experimental): https://github.com/virtio-win/kvm-guest-drivers-windows/pull/943
- MS — WDDM Operation Flow (DxgkDdi render call sequence): https://learn.microsoft.com/en-us/windows-hardware/drivers/display/windows-vista-and-later-display-driver-model-operation-flow
- MS — DXGKDDI_BUILDPAGINGBUFFER: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_buildpagingbuffer
- MS — Submitting a Command Buffer (DxgkDdiSubmitCommand / fences): https://learn.microsoft.com/en-us/windows-hardware/drivers/display/submitting-a-command-buffer
- UTM — "Introducing Neptune: Direct3D virtualization for QEMU" (2026; mesa→neptune-protocol→virglrenderer→dxvk; Venus-style batching; DXVK north-star; 6–8mo): https://blog.getutm.app/2026/introducing-neptune-direct3d-virtualization-for-qemu/
- UTM — Graphics.md (Venus vs gfxstream; Venus carries DXVK/Zink since 2023): https://github.com/utmapp/UTM/blob/main/Documentation/Graphics.md
- Collabora — state of GFX virtualization using virglrenderer (vrend GL vs Venus Vulkan): https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/
- Phoronix — VirtualBox 7.0 (VBoxWDDM guest + host D3D11 via DXVK-Native → Vulkan): https://www.phoronix.com/news/VirtualBox-7.0-Released
- Mesa — VMware SVGA3D Windows Gallium state trackers (`vm3dmp` precedent): https://docs.mesa3d.org/drivers/svga3d.html
- MS — Advancing Windows driver security: removing trust for cross-signed drivers (April 2026; attestation/WHCP unaffected): https://techcommunity.microsoft.com/blog/windows-itpro-blog/advancing-windows-driver-security-removing-trust-for-the-cross-signed-driver-pro/4504818
- MS — Driver code-signing requirements (EV cert prerequisite for attestation + WHCP): https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-reqs
- MS — Attestation signing Windows drivers (EV CAB → MS counter-sign, no HLK): https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation
- microsoft/windows-drivers-rs (KMDF-only; no dxgkrnl bindings): https://github.com/microsoft/windows-drivers-rs
