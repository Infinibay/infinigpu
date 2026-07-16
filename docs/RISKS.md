# infinigpu — Risk register & go/no-go (from the Wave-3 red-team)

> Source: research/13-redteam-showstoppers.md (adversarial), rebutted/qualified by
> research/14-fence-sync-design.md and research/15-windows-m3-ddi-concrete.md. Date: 2026-07-16.
> **This is the honest assessment. Read it before committing engineering quarters.**

## The blunt verdict

- **As a generic commercial multi-tenant VDI product sold under an availability SLA → NO-GO.**
  Two reasons dominate: (1) on the RTX A5000 (GA102) a single guest's severe GPU fault forces a
  **device-wide reset that blacks out every tenant** (no MIG/SR-IOV isolation on this card); (2) the
  **business case is inverted** — the A5000 is on NVIDIA's vWS matrix, so vGPU (~$450/CCU perpetual +
  ~$100/CCU/yr) already delivers turnkey licensed sharing on the exact card, for ~an engineer-week of
  cost vs a multi-quarter build.
- **As a principle-driven, owned, multi-vendor platform → scoped GO.** It flips to GO when *all* hold:
  (a) **"no proprietary blob in the guest path / 100% ownership" is a product principle**, not a
  cost/density play; (b) scope is **Linux + Vulkan best-effort first**, Windows-3D sequenced later;
  (c) the team **accepts the device-wide-reset residual** on GA102 (mitigated on other silicon).

> **How this maps to the owner's stated direction:** the owner has repeatedly required *100% ownership,
> no per-VM licensing, and multi-vendor (NVIDIA/AMD/Intel)* support, plus a *custom low-latency
> perceptual remote protocol*. Those are precisely condition (a) — and they also **defuse the S2
> business objection**, because vGPU is NVIDIA-only and per-VM-licensed: it does **not** give you an
> AMD/Intel story, a license-free story, or an owned low-latency protocol. So the value is
> "ownership + vendor-independence + a purpose-built protocol," not "cheaper than vGPU." Priced that
> way, the project is coherent — provided the residual risks below are accepted with eyes open.

## Showstoppers (ranked likelihood × impact), with current status

| # | Risk | Sev | Status after Waves 2–3 |
|---|------|-----|------------------------|
| **S1** | **Cross-VM freeze + device-wide reset** — helix.ml (the one field report) deadlocks at 4+ desktops; a severe Xid resets the shared GPU → all tenants down. | HIGH×FATAL | **Split.** The *deadlock* half is a **solved design problem** (doc 14: helix.ml froze on a *global* counter + FIFO head-of-line — a specific bug; our per-context seqno/timeline design + 8-rule set avoids it, all primitives proven). The *device-wide-reset* half is **irreducible on GA102** (accepted residual; **better on AMD/Intel** per the vendor-HAL work — a reason multi-vendor matters). |
| **S2** | **Inverted business case** vs turnkey vGPU on the same A5000. | HIGH×FATAL | **Reframed, not refuted.** Only justified as a principle/ownership/**multi-vendor**/own-protocol play. vGPU gives none of those. Do the TCO one-pager honestly. |
| **S3** | **Windows in-guest 3D has no KVM precedent.** | HIGH×HIGH | **Rebutted (doc 15).** A working KVM render miniport exists (max8rr8/viogpu3d); fork it, reseam to our ring, and run **DXVK+vkd3d-proton in the guest** on our Vulkan arbiter — no bespoke D3D UMD. ~3–5 quarters on top of M2. Still the largest build, but *precedented*. |
| **S4** | **Version-skew maintenance trap** — Venus "relies on implementation-defined behavior" and pins host NVIDIA driver versions; Android/ChromeOS survive only via guest+host **lockstep**, which a generic product can't. | MED-HIGH×HIGH | **Real, partially mitigated.** Infinibay is an **appliance-like self-hosted stack that controls the host** → it *can* pin+CI-test the host GPU driver against the shipped guest image (unlike a generic ISV). Treat the host driver as part of the appliance; gate host-driver updates through a compat matrix. Ongoing cost, not fatal. |
| **S5** | **vfio-user is NVMe/SPDK-shaped, not GPU-shaped** — no GPU / multi-vector-MSI-X / display hot-unplug over vfio-user exists anywhere; Windows class-code bind unexercised. | MED×HIGH | **Falsifiable cheaply.** Expand the ADR-0001 spike to 8+ MSI-X vectors + a Windows guest binding by class code + hot-unplug-on-stop. If it wobbles → Option-B fallback now. |
| **S6** | **KSM loss** — `memory-backend-memfd,share=on` (needed for zero-copy DMA) forfeits kernel same-page merging, a primary VDI dedup lever for near-identical Windows images; balloon/overcommit interaction unknown. | MED×MED | **Measure early.** Only GPU-attached VMs pay it; non-GPU VMs keep KSM. Quantify host RSS with/without and the density ceiling before scaling. |

## Risk-burndown — run these BEFORE committing quarters (cost-to-kill order)

1. **~2 days — vGPU-vs-build TCO one-pager** for 2× A5000 + N seats (real reseller quote). Have the "principle vs cost" conversation up front. *(S2)*
2. **~1 week — host-driver-skew matrix:** replay the same ~20-call Vulkan workload against 3 pinned NVIDIA drivers; count breaks. Decide the host-driver-pinning policy. *(S4)*
3. **1–2 weeks — expand the vfio-user spike** (ADR-0001) to 8+ concurrent MSI-X vectors + Windows class-code bind + clean hot-unplug-on-stop. Fall back to Option B if it wobbles. *(S5)*
4. **~1 week — reproduce the helix.ml N=4–8 concurrency wall AND the all-tenants-down reset** with the doc-14 per-context design. **This is the true go/no-go gate** — prove the freeze is avoided and characterize the reset blast radius before building the scheduler. *(S1)*
5. **~3 days — measure KSM loss + p95 interactive latency at 4–8 guests** on one GA102. *(S6, S1-perf)*
6. **weeks, only if 1–5 survive — WDDM enumeration/signing spike** (does a vfio-user class-`0x030000` device enumerate in Windows; EV-cert + attestation dry run). *(S3)*

If steps 1–5 clear, proceed to Phase 0 (`PHASE-0-PROTOTYPE.md`). If S1's reset blast radius or S4's skew prove intolerable for the target deployment, **de-scope** to single-GPU / few-tenant / Linux-only / best-effort — or, for a plain licensed-VDI need on NVIDIA, buy vGPU and ship an SLA this quarter.

## Residual risks accepted by design (documented, monitored — not solved)

- **GA102 device-wide GPU reset** on a severe Xid downs all tenants (no MIG). Quarantine + monitor;
  expect better isolation on AMD/Intel (vendor-HAL). Fatal only for a hard multi-tenant SLA on NVIDIA
  consumer/pro silicon.
- **Host-driver/guest-protocol compat matrix** owned in perpetuity (mitigated by appliance-side pinning).
- **Windows 3D** trails Linux by quarters; Windows ships **IddCx-display + host-side rendering** first.
