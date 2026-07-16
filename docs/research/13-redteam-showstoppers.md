# 13 — Red-Team: Showstoppers, Ranked (hostile review)

**Role:** hostile reviewer trying to *kill* infinigpu. This doc does not re-litigate the ADRs'
internal engineering claims (those hold on their own terms); it attacks the **product thesis** —
"share one A5000 across a dozen+ Windows+Linux VDI desktops, license-free, 100% owned" — with
current (2026) primary sources.

## VERDICT: **NO-GO as a commercial multi-tenant VDI feature under an SLA.**

It flips to a **scoped GO** only under three simultaneous conditions: (a) the goal is genuinely
*ownership/learning* and licensing is a hard external constraint, not a cost-optimization; (b)
scope is **Linux + Vulkan, best-effort availability**, with Windows-3D deferred *indefinitely*, not
"later"; and (c) the team accepts that one guest can black-out all tenants. As pitched — a
commercial VDI product with a dozen concurrent Windows+Linux desktops on 2× A5000 — the stack of
residual risks below is not survivable, and the business case is inverted because **the A5000 you
already own supports NVIDIA vGPU**.

## Ranked showstoppers (likelihood × impact)

| # | Showstopper | Likelihood | Impact | Score |
|---|-------------|-----------|--------|-------|
| S1 | Multi-tenant concurrency deadlock wall **+** device-wide-reset residual (one guest downs all) | High | Fatal | **9** |
| S2 | Business-case inversion: the A5000 already supports vGPU; build cost ≫ license cost | High | Fatal | **9** |
| S3 | Windows in-guest 3D (from-scratch WDDM render pair, no KVM precedent) | High | High | **8** |
| S4 | Version-skew maintenance trap (impl-defined behavior couples host replay to exact NVIDIA driver) | Med-High | High | **7** |
| S5 | vfio-user *client* immaturity for a complex GPU-class device (no GPU precedent; NVMe/SPDK-shaped) | Medium | High | **6** |
| S6 | Density/perf: `memfd,share=on` forfeits KSM; API-remoting double-work collapses under contention | Medium | Med | **5** |

---

### S1 — The multi-tenant availability wall (concurrency **and** reset) — *the single most likely kill*

This is two failure modes that both terminate a VDI SLA, and the honest read is that **a real team
building this exact thing could not get past four desktops.**

**Concurrency/deadlock.** The helix.ml multi-desktop GPU-virtualization writeup — a production
attempt at precisely our model (per-desktop contexts replaying onto one host GPU) — reports that
*"the deadlocks only appear at 4+ concurrent desktops — which is exactly the regime we need for
production use."* Concretely: *"With 4 gnome-shells, if scanout 1's SPICE client is slow to
acknowledge, ALL four desktops freeze"*; a single suspended blob-unmap from context 1 *"would block
commands from contexts 2, 3, and 4 that are sitting later in the queue"*; and a virtual-clock
deadlock where *"all vCPUs eventually enter WFI, the virtual clock stops, and `fence_poll` never
fires."* The post ends unresolved (*"Will we fix it? Stay tuned"*). This is synthesis hard-problem
#2 confirmed in the field: the fence/queue design is not a detail, it is the product, and the
reference implementation of our architecture is stuck at N=4.

**Device-wide reset.** ADR0003 already concedes the residual, and NVIDIA's own docs make it
unavoidable on GA102: an unrecoverable Xid (e.g. 79 "GPU fell off the bus", 48 DBE, 119 GSP RPC
timeout) requires a **GPU reset**, and per NVIDIA a reset *"requires root access and there can't be
any applications using these devices"* and *"can trigger a reset of one or more GPUs."* On a single
shared A5000 that means **every** host Vulkan context — i.e. every tenant — is torn down by one
guest's fault. A commercial VDI SLA cannot promise "your desktop stays up" when any co-tenant's
buggy shader can reset the card.

*Cheapest falsifier (~1 wk, do this FOURTH, before building the scheduler):* stand up N=4→8 trivial
Vulkan replay contexts on one A5000 behind one arbiter with a deliberately slow "SPICE ack" on one;
measure whether the other N-1 stall. Separately, deliberately induce a hang/Xid on one context and
observe whether all contexts die. If either reproduces (it will), you have your SLA answer before
writing a line of Windows driver.

### S2 — The business case is inverted

The stated justification for the entire multi-quarter build is constraints 1 (own it) and 2 (no
per-VM license). But **the RTX A5000 is on NVIDIA's vWS support matrix** (listed with a 1×24 vGPU
config), so vGPU is a *turnkey* alternative *for the exact card in hand*. Street price for the
license it avoids: **~$450/CCU perpetual + ~$100/CCU/yr SUMS** (SHI lists the 1-CCU perpetual at
$479). For a dozen seats that is roughly **$5.4k one-time + $1.2k/yr** — i.e. the whole
"license-free" savings is on the order of **one engineer-week**, against a build that ADR0005 itself
scopes at *6–8 months for the Windows render miniport alone.* Passthrough (1 GPU→1 VM) plus a couple
more A5000s is another off-the-shelf answer for small tenant counts. Building infinigpu is only
rational if licensing is a **hard external prohibition** (air-gapped/sovereignty, or a philosophical
"no proprietary blob in the per-VM path" product promise) — *not* if the goal is saving money or
"more density," because vGPU delivers both today with a real SLA. This is the strongest single
argument to kill the project on ROI grounds.

*Cheapest falsifier (~2 days, do this FIRST):* price a vWS quote for 2× A5000 + N seats from one
reseller; write the one-page TCO vs. an honest 3–4 quarter build estimate. If the only column
where infinigpu wins is "no proprietary driver in the guest path," the project is a *principle*, not
a product — decide on that basis.

### S3 — Windows in-guest 3D has no KVM precedent

Synthesis hard-problem #1, and it doesn't get cheaper on contact. Microsoft's GPU-PV does exactly
our marshalling — but it is **Hyper-V/VMBus-only** and unavailable to KVM guests. There is no open
precedent for a from-scratch WDDM UMD+KMD render pair on KVM. ADR0005 bounds it at ~6–8 months by
cribbing viogpu3d/UTM-Neptune, but that is an *estimate for the hardest, least-precedented component
in the stack*, gated behind WDDM signing and the April-2026 kernel-trust tightening. Realistic risk:
Windows ships as **IddCx-display + host-side 3D only** for a very long time, meaning Windows guests
get a *streamed remote desktop with no in-guest GPU acceleration* — which is a materially weaker
product than "each VM has a GPU."

*Cheapest falsifier (weeks, do this LAST):* don't build the miniport to test it; instead
time-box a spike that gets a **minimal WDDM KMD to enumerate and pass HLK's basic display DDI
conformance** against a paravirtual PCI adapter. If even enumeration/signing is a multi-week slog,
the 6–8mo estimate is optimistic and Windows-3D should be declared out of scope in writing.

### S4 — Version-skew is a perpetual maintenance trap

Mesa's Venus docs state plainly the model *"violates the spec and relies on implementation-defined
behaviors to support `vkMapMemory`"* and that *"the long-term plan is to create a new Vulkan
extension for the host drivers to address this specific use case"* — i.e. the clean version-decoupled
mechanism **does not exist yet**. Venus pins **specific tested host drivers** ("NVIDIA (Proprietary)
570.86 or later"). Here is the part the ADRs understate: **Venus/gfxstream survive skew by never
having any** — Android and ChromeOS ship the guest encoder (Mesa) and host decoder
(virglrenderer/gfxstream) **as one lockstep image/update**. A VDI product **cannot**: the guest
driver lives inside a customer's Windows/Linux VM image that updates on its own cadence, while the
host NVIDIA driver is patched for security independently. Every NVIDIA host-driver bump is now a
**potential fleet-wide regression** in behavior our replay relies on, with no upstream to absorb it —
a CI matrix of {guest protocol vsn} × {NVIDIA host driver} that is ours forever.

*Cheapest falsifier (~1 wk, do this SECOND):* encode/replay a ~20-call Vulkan workload (map/upload/
draw/present) and run the *same guest bytes* against 3 pinned NVIDIA host drivers (e.g. 550 / 570 /
580). Count behavioral breaks in `vkMapMemory`/coherency/alignment. Any breakage across a minor bump
quantifies the maintenance tax before you commit to it.

### S5 — vfio-user client: real, but NVMe/SPDK-shaped, not GPU-shaped

Doc 07 confirmed the *primitives* (BAR mmap, ioeventfd, memfd DMA, reset) exist in QEMU 10.1.1. The
hostile point is **maturity for a *complex* device**, and the evidence is thin exactly where a GPU is
hard. The 10.1 client author frames it as *"enough implemented to cover our immediate needs"* with
*"an awful lot more we could be doing,"* and it already needed a post-10.1 regression fix
(`x-pci-class-code`) *just to present a correct PCI class code* — the very config-space plumbing a
VGA-class device depends on. Every public vfio-user device is **NVMe (SPDK), GPIO, or blk** —
cloud-hypervisor still labels vfio-user *"experimental"* and notes *iommu is not supported*. **There
is no GPU, no multi-vector MSI-X GPU interrupt model, and no display-adapter-hotplug precedent over
vfio-user anywhere.** Presenting an *arbitrary GPU-class device to BOTH Windows and Linux* — MSI-X
with many vectors, correct config-space so WDDM/DRM bind by class, and clean **unplug on VM stop
without leaking the server process** — is unexercised territory. This is survivable (it's the planned
spike, and Option B is a fallback) but must not be assumed.

*Cheapest falsifier (1–2 wk, do this THIRD — expand the doc-07 spike):* the doc-07 device *plus*
(i) **8+ MSI-X vectors** delivered concurrently; (ii) a **Windows guest** binding the device by
class code (not just Linux); (iii) **hot-unplug on VM stop** and verify the Rust `ServerBackend`
tears down with no leak/hang. If MSI-X multi-vector or Windows-bind wobbles, fall back to Option B
*now*, not after building on A.

### S6 — Density and interactive-perf reality

Two concrete costs the "near-native, thin layer" framing hides:

- **KSM is forfeited.** Zero-copy DMA forces *all* guest RAM onto `memory-backend-memfd,share=on`
  (doc 07). KSM **only merges anonymous private pages** (`MADV_MERGEABLE`); file-/shared-backed
  memfd pages are ineligible. For VDI — dozens of near-identical Windows images — KSM dedup is a
  primary density lever, and share=on **turns it off**. Hugepages remain compatible (`hugetlb=on`),
  but hugepages themselves fight ballooning/overcommit, so the density budget shrinks from both
  ends. *(Ballooning/overcommit interaction with a device that mmaps all guest RAM is unquantified —
  NEEDS VERIFICATION, but the KSM loss alone is a definite density tax.)*
- **API-remoting is double CPU work under contention.** Venus is "near-native" for *one* client
  because it's a thin transport; the moment you multiplex a dozen desktops onto one GPU + one arbiter,
  you pay encode (guest CPU) + decode/replay (host CPU) *per stream*, serialize through the
  arbiter's rings, and contend one GA102's firmware time-slicer. There are **no published
  multi-tenant interactive-desktop numbers** showing this holds at a dozen VMs — and the one field
  report we have (helix.ml, S1) collapses at four. "Interactive desktop for a dozen+ VMs on 2×
  A5000" is an **unvalidated assumption**, not a measured result.

*Cheapest falsifier (~3 days, fold into S1's rig):* boot 8 identical guests, measure host RSS
with/without share=on to quantify the KSM loss; then drive a compositor + one 3D app in 4–8 guests
at once and measure frame latency/jitter vs. a single guest. If p95 interactive latency degrades
super-linearly past ~4, density economics don't close.

---

## Risk-burndown order (cheapest, most-decisive first)

The ranking above is by severity; the **order to actually run** is by *cost-to-kill-the-project*:

1. **S2 — TCO one-pager (~2 days).** If vGPU on the A5000 you own wins on every axis but "no blob in
   guest," stop and have the principle conversation before spending an engineer-quarter.
2. **S4 — driver-skew matrix (~1 wk).** Quantify the perpetual maintenance tax on a throwaway encoder.
3. **S5 — expanded vfio-user spike (1–2 wk).** MSI-X multi-vector + Windows-bind + unplug-on-stop.
4. **S1 — concurrency + reset blast-radius (~1 wk).** Reproduce the helix.ml N=4 wall and the
   all-tenants-down reset *before* building the scheduler. This is the true go/no-go gate.
5. **S6 — density + latency measurement (~3 days, fold into #4).**
6. **S3 — Windows WDDM enumeration/signing spike (weeks).** Only worth starting if 1–5 survive.

If S1 or S4 comes back as expected, the honest move is to **de-scope to a single-GPU, few-tenant,
Linux-only, best-effort tool** (which is defensible as owned R&D) — or to buy vGPU and ship a real
SLA this quarter.

## Sources

- Helix.ml — GPU virtualization for multi-desktop containers (N=4 deadlocks, global-lock freeze): https://blog.helix.ml/p/gpu-virtualization-architecture-for
- NVIDIA — Xid Errors index / catalog (Xid aborts jobs; reset semantics): https://docs.nvidia.com/deploy/xid-errors/index.html
- NVIDIA — GPU Debug Guidelines (GPU reset requires no apps using device; resets one or more GPUs): https://docs.nvidia.com/deploy/gpu-debug-guidelines/index.html
- NVIDIA — Virtual GPU Packaging, Pricing & Licensing Guide (vWS per-CCU model; A5000 on matrix): https://docs.nvidia.com/vgpu/packaging-pricing-licensing-guide/latest/index.html
- NVIDIA — RTX Virtual Workstation product page: https://www.nvidia.com/en-us/design-visualization/virtual-workstation/
- SHI — NVIDIA RTX vWS Perpetual License, 1 CCU ($479 street): https://www.shi.com/product/46247132/NVIDIA-RTX-VWS-PERPETUAL-LICENSE,-1-CCU
- CDW — NVIDIA RTX vWS perpetual license, 1 CCU: https://www.cdw.com/product/nvidia-rtx-vws-perpetual-license-1-concurrent-user/4825431
- Mesa — Venus driver docs ("violates the spec and relies on implementation-defined behaviors"; tested NVIDIA 570.86+; "long-term plan is a new Vulkan extension"): https://docs.mesa3d.org/drivers/venus.html
- Microsoft — GPU Paravirtualization / GPU-PV (Hyper-V/VMBus-only): https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization
- John Levon — "vfio-user client in QEMU 10.1" ("enough...for our immediate needs"; post-10.1 regression): https://movementarian.org/blog/posts/2025-08-27-vfio-user-client-in-qemu/
- QEMU — vfio-user client system-device doc: https://www.qemu.org/docs/master/system/devices/vfio-user.html
- QEMU — vfio-user protocol spec (DMA fd / region mmap / ioeventfd / reset): https://www.qemu.org/docs/master/interop/vfio-user.html
- cloud-hypervisor — vfio-user doc (labeled "experimental"; iommu unsupported; NVMe/SPDK + GPIO examples): https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/vfio-user.md
- Linux kernel — KSM docs (merges only anonymous/MADV_MERGEABLE pages, not file/shared): https://docs.kernel.org/admin-guide/mm/ksm.html
- QEMU — memory-backend / shared memory semantics (share=on caveats): https://www.qemu.org/docs/master/system/devices/ivshmem.html
- Collabora — State of GFX virtualization via virglrenderer (2025; Venus vs vDRM host/guest sync cost): https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/
- gfxstream (Google) — codegen'd encoders/decoders, guest+host shipped together: https://android.googlesource.com/platform/hardware/google/gfxstream/
