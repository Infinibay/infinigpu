# infinigpu — ERRATA (documentation review log)

> Findings from the 5-reviewer adversarial review (2026-07-16) of the full corpus. **Status:** ✅ fixed
> inline · 📝 recorded here + in the affected ADR's "Corrections" section (apply during implementation).
> Failure-mode walkthroughs live in [`SCENARIOS.md`](SCENARIOS.md).

## High-priority (factual errors & load-bearing inconsistencies)

| # | Location | Problem | Fix | Status |
|---|---|---|---|---|
| 1 | ADR-0009 / research 09,18,26 | "GA102 has **two NVENC blocks**" — false | GA102/A5000 = **1× NVENC + 2× NVDEC** (dual-NVENC is Ada+); halve encode-density ceiling; make NVENC a first-class admission resource | ✅ (ADR-0009/0007); 📝 research docs |
| 2 | 99-synthesis §3.6 | host "**H.264/AV1-encodes**" on Ampere (can't AV1-encode) | → "H.264/HEVC (AV1 needs Ada+/RADV, negotiated)" | ✅ |
| 3 | 99-synthesis §3.6 vs §7; research 06 | SPICE relay port **6100-6119** (stale) vs 6100-6199 | → **6100-6199** everywhere | ✅ (synthesis); 📝 research 06 |
| 4 | ROADMAP, research/README, 99-synthesis | "ADR **0001-0009**" / "the five ADRs" (11 exist) | → **0001-0011** | ✅ |
| 5 | ADR-0001, research 07/11 | ioeventfd doorbell "confirmed shipping" — but rust-vmm/vfio server crate **rejects `GET_REGION_IO_FDS`** | patch crate / C libvfio-user / **batched trapped doorbell**; downgrade the claim | ✅ (ADR-0001); 📝 research 07/11 |
| 6 | research 24 / 25 / 28 | placeholder PCI **`1b36:0100` collides with QXL** (qxl driver binds it); `1b36:0001` = PCI-PCI bridge | pick an unallocated DEV in QEMU pci-ids.rst; real PCI-SIG vendor ID before GA | ✅ (ADR-0001); 📝 research 24/25/28 |
| 7 | research 24 §4 | IOVA→HVA translation & consume loop lack **bounds checks** (hostile guest → host OOB read) | fail-closed interval-map lookup, clamp TAIL, bounds-check descriptors | 📝 (ADR-0001 note) |
| 8 | research 16 §4 vs ADR-0007/0008 | scheduler built on **REALTIME/HIGH `global_priority`** bands NVIDIA caps at MEDIUM (and unprivileged replay can't elevate) | QoS = token bucket + back-pressure + watchdog; MEDIUM-vs-LOW hint only | ✅ (ADR-0007); 📝 research 16 |
| 9 | ADR-0005 M2 / research 08 | "M2 = **no kernel driver**" — impossible (ICD needs a KMDF function driver to reach the PCI rings) | reword "**no WDDM render miniport**"; specify minimal function driver (still signed) | ✅ (ADR-0005) |
| 10 | ADR-0005 M1 / research 03 | Windows M1 IddCx **frame path to host encoder undefined** (frames in guest RAM, ADR-0009 assumes host NVENC) | IddCx writes swapchain to a shared memfd → host NVENC | ✅ (ADR-0005) |
| 11 | research 17 / 16 / 10 | **NVENC device-wide capacity** absent from accounting; **guest video bitstream** absent from the threat model | NVENC admission gate; harden the NVDEC bitstream path inside the per-VM jail | ✅ (ADR-0007); 📝 research 10/16/17 |
| 12 | doc 18 §5 vs doc 19 §2 | **A/V master-clock contradiction** (video-master vs audio-master) | persona-conditioned: video-master interactive, audio-master passive media | ✅ (ADR-0009) |
| 13 | docs 30/31 & ADR-0011 | per-frame counter named **`streamEpoch` vs `deleg_epoch`**; two incompatible sidecar headers; dropped `present_deadline_us` | one name (`streamEpoch`), one merged header incl. `present_deadline_us` | ✅ (ADR-0011); 📝 research 30/31 |
| 14 | ADR-0011 / doc 31 | **no reconnect/resume** flow (warm tile cache lost on any drop) | resumption token + `CACHE_IMPORT_OFFER` revalidate | ✅ (ADR-0011) |
| 15 | research 25/28 | `pgprot_noncached`/**UC** mis-applied to the coherent RAM index page | WB cacheable + `smp_wmb/rmb`; UC only for true MMIO | ✅ (ADR-0001/SCENARIOS); 📝 research 25/28 |

## Medium-priority (recorded; apply during implementation)

| Location | Problem | Fix | Status |
|---|---|---|---|
| research 06 | Wave-1 prototype still recommends **vhost-user + single-arbiter/one-context-per-guest** (superseded by ADR-0001/0003) | add a "superseded by ADR-0001/0003" banner | 📝 |
| ADR-0001 / RISKS S6 | all-guest-RAM `DMA_MAP` makes **balloon/overcommit unsafe** (not just KSM loss) | GPU-VMs non-overcommittable, balloon off | ✅ (ADR-0001) |
| ADR-0001 | QEMU ≥10.1.1 dependency **not enforced** | fail-fast version/feature preflight | ✅ (ADR-0001) |
| ADR-0001 | **64 MSI-X vectors** unnecessary (MSI has no payload) + crate fd-buffer risk | 1 vector + shared pending bitmap | ✅ (ADR-0001) |
| research 06/07/24 | described **socket DMA_READ fallback doesn't exist** in the Rust crate | any unmapped IOVA = fail-closed | ✅ (ADR-0001) |
| research 03 §Sequencing | milestone numbering contradicts the M1–M4 scheme | note "superseded by ADR-0005 M1–M4" | 📝 |
| ADR-0005 Consequences | M3 **BSOD blast radius** omitted | record blast radius = one VM | ✅ (ADR-0005) |
| research 16 §2 | Vulkan **timestamp deltas over-count** under contention (treated as authoritative) | reconcile vs device busy; prefer GPM SM-active | ✅ (ADR-0007); 📝 research 16 |
| research 16 §4 / ADR-0007 | **no anti-starvation floor**/aging; FG_BOOST uncapped | min token floor + aging + cap/decay boost | ✅ (ADR-0007) |
| research 16 §5/§7 | **concurrent page-in** unbounded (boot storm / post-reset stampede) | page-in admission queue + PCIe accounting | ✅ (ADR-0007) |
| research 10 §4 | never-completing shader invisible to token bucket | broker in-flight GPU-time watchdog | ✅ (ADR-0007) |
| ADR-0003/0007, docs 10/16 | **Prisma field set disagrees** across docs | canonical 7-field set | ✅ (ADR-0007) |
| research 20 / ADR-0008 / ADR-0007 | **four seams numbered inconsistently**; ADR-0007 cites "SEAM 1" ADR-0008 never labels | number seams identically or refer by name | 📝 |
| research 20 | `finest_reset_scope` **misnomer** (DeviceWide = coarsest) → over-quarantines NVIDIA's contained faults | drive blast-radius off per-fault `GpuFault.scope`; rename `worst_case_reset_scope` | 📝 |
| ROADMAP | ADR-0010/0011 (**client offload+delegation**) absent from phases | add to Phase 2/3 / cross-cutting | 📝 |
| doc 23 §4 | leads NVENC recipe with **emphasis** mode (ADR-0009 mandates **delta**) | lead with `NV_ENC_QP_MAP_DELTA`; + NEEDS-VERIFICATION that delta coexists with AQ on SDK 13 | 📝 |
| doc 26 §2/§5 | super-res/interp `~3–4 ms` cited to an **unnamed source**; controller keys on the estimate | mark "unverified estimate"; key activation on measured lag | ✅ (ADR-0011) |
| docs 30 §4.1 / §4.3 | Region struct annotated **10 B** but math uses 12 B | → 12 B (11 used + 1 pad) | ✅ (ADR-0011) |
| doc 30 §5 | `SUPERRES_REF` spuriously gated on **`cacheEpoch`** | gate GPU-POST/SR/INTERP on `streamEpoch` only | ✅ (ADR-0011) |
| docs 30/31 | `DELEG_NACK` vs `DELEGATION_NAK` **name collision** | negotiation OFFER/ACCEPT/DECLINE/READY; per-frame FRAME_ACK/FRAME_NAK | ✅ (ADR-0011) |
| ADR-0009 / doc 31 | ~14–22 ms mislabeled **motion-to-photon** (it's datapath-only; true MTP ~40–70 ms) | relabel "display-datapath budget within ~40–70 ms MTP" | ✅ (ADR-0011) |
| ADR-0009 / doc 27 | **TCP-fallback datapath** unspecified | parallel per-lane WS / lane-drop; cap res/fps; SPICE for hostile nets | ✅ (ADR-0011/SCENARIOS) |

## What the review confirmed is *correct* (survived adversarial check)

- doc 12 correctly refutes doc 05's optimistic Rust-KMS rating (Linux KMS stays C). ✅
- doc 15 correctly narrows the April-2026 Windows signing alarm (attestation survives). ✅
- The frame-binding/epoch-fencing contract, degrade-never-deny fallback, damage-gated signaling, and
  the two-plane wire split are sound; most delegation stress scenarios are handled by design. ✅
- The vendor-HAL four-seam story, the AV1-on-Ampere caveat (in ADR-0008), and the honest
  device-wide-reset / go-no-go framing hold up. ✅
