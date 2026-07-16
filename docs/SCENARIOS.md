# infinigpu — Failure-mode scenarios ("what happens if…")

> Stress-tests of the design against real events, from the Wave-review. Each row: **event → what the
> design does today → verdict (✅ handled / 🟡 partial / 🔴 unhandled) → mitigation.** Feeds the ADRs.
> Date: 2026-07-16.

## A. Host device & data plane (vfio-user)

| Event | Now | Verdict | Mitigation |
|---|---|---|---|
| The rust-vmm server can't emit `GET_REGION_IO_FDS` → no ioeventfd doorbell | Every doorbell traps as a `REGION_WRITE` socket round-trip | 🟡 | Patch the crate / use C libvfio-user / **batched doorbell per submission** (ADR 0001) |
| Guest crashes mid-ring-submission (partial/malicious descriptor) | `kill(process)` reaps GPU state; watchdog force-completes fences | 🟡 | **Bounds-check** TAIL + fail-closed IOVA lookup *before* dereference, else host OOB read |
| Guest writes a ring-base IOVA outside any `DMA_MAP` | Rust crate has **no** socket-DMA fallback (`DmaRead` unsupported) | 🟡 | Fail-closed: drop command + ring error; every base/backing must be fully `DMA_MAP`-covered |
| BAR0 index/seqno page mapped uncached | Seqno polling becomes slow uncached reads | 🟡 | It's coherent RAM (vring-like) → **WB cacheable + `smp_wmb/rmb` barriers**, not UC |
| Host runs QEMU < 10.1.1 | Device gets wrong class code → guest driver never binds (cryptic) | 🟡 | **Fail-fast preflight** `qemu --version` + probe `x-pci-class-code`; pin QEMU in the appliance |
| `memfd,share=on` vs ballooning/overcommit | Ballooned page still server-mapped → races DMA | 🟡 | GPU-VMs **non-overcommittable**, balloon disabled; `hugetlb=on` OK; size RAM for full footprint |
| 64 MSI-X vectors truncated by the young client/crate | SET_IRQS fd-buffer may cap ~16 | 🟡 | MVP: 1 vector + shared "pending" bitmap the ISR scans (MSI carries no payload) |
| Two command rings race on completion | Per-ring seqno words → no race | ✅ | Pad rings to separate cachelines (avoid false sharing) |

## B. Sharing, isolation & GPU faults (the multi-tenant core)

| Event | Now | Verdict | Mitigation |
|---|---|---|---|
| **Severe Xid (79/45/62/48/119) resets the whole GA102** | Quarantine device, kill+drain all replay procs, reset, re-admit; provoking VM audited | 🟡 **accepted residual** | No fix on GA102 (needs MIG/vGPU or AMD/Intel per-queue reset); spread personas across the 2 GPUs so a reset halves not totals; **cold-failover** downed VMs to the healthy GPU |
| Guest submits an **infinite-loop shader** | Token bucket is blind (debits on completion); relies on NVIDIA RC watchdog (may escalate device-wide) | 🟡 | **Broker in-flight GPU-time watchdog** kills the replay process before RC escalates |
| Greedy VM exhausts **VRAM** | Admission caps + `VK_ERROR_OUT_OF_DEVICE_MEMORY` hard cap | ✅ | (working) |
| **9am boot storm** + a severe Xid at peak | Attach staggered; but mass VRAM page-in unmodeled; reset downs all GPU0 tenants at peak | 🟡 | Page-in restore queue (rate-limit concurrent restores); post-reset persona-priority cold re-admit; make the burndown exercise the reset **under** storm concurrency |
| `global_priority` capped at MEDIUM on NVIDIA | doc 16's REALTIME/HIGH bands don't work; unprivileged replay can't elevate | 🔴→fixed in ADR | QoS = **token bucket + submission back-pressure + watchdog**; MEDIUM-vs-LOW hint only; no in-flight pre-emption |
| Hostile **video bitstream** → host NVDEC | doc 10 hardened the Vulkan ring, not the NVDEC path | 🟡 | Decode/compose/encode **inside the per-VM jail**; validate bitstream dims/profile/level/ref-count; NVDEC/NVENC hangs → Xid quarantine |
| Foreground boost **starves a designer** | vruntime resists total starvation but FG_BOOST×4 can hold share ≈0 | 🟡 | Min token-refill **floor** + vruntime aging + cap/decay FG_BOOST |
| VM attach → run → **crash** → reap → re-admit | `kill=reap` sound; but VRAM-ledger/NVENC-session release on abrupt crash unspecified | 🟡 | Explicit reap sequence (free ledger → tear down encoder/NVENC → release ring/socket → re-admit) |

## C. Guest drivers (Windows / Linux)

| Event | Now | Verdict | Mitigation |
|---|---|---|---|
| **Host NVIDIA driver updated** under a running fleet | Boot-time-negotiated Vulkan extension set silently invalidated → DXVK/vkd3d break | 🔴→fixed in ADR | Host↔guest Vulkan **re-handshake** on driver change (or pin/stage rollouts); lost extension → recoverable `DEVICE_LOST` + quarantine |
| **M3 miniport code fault** (not a TDR) | Bugchecks (BSODs) that one guest | 🟡 | Blast radius = one VM (documented); minimal payload-agnostic miniport; harden TDR/reset; host auto-restart the guest |
| DXVK/vkd3d **lacks a D3D12 feature** over remoted Vulkan | App fails / renders wrong | 🟡 | Advertise to DXGI only covered feature levels (clamp); degrade to WARP not hard-fail; gate M4 on a feature matrix |
| cbindgen ABI header **drifts** from the Rust crate | Silent guest↔host protocol corruption | 🟡 | **CI-gate** the struct-layout round-trip test (like infiniservice's HMAC cross-lang test) |
| Rust KMS unavailable at kernel 7.2 | Linux KMD stays C | ✅ | (accepted; Rust crate via cbindgen) |

## D. Remote protocol, perceptual & client delegation

| Event | Now | Verdict | Mitigation |
|---|---|---|---|
| **Client disconnects & reconnects** (sleep / Wi-Fi handoff / proxy reset) | Cold re-negotiation; warm tile cache (MBs) discarded → text-bandwidth win resets | 🔴→fixed in ADR | Resumption token + `CACHE_IMPORT_OFFER` re-assert (revalidate, don't reset `cacheEpoch`) |
| **QUIC datagram loss/reorder** of a per-frame sidecar | Stale sidecar dropped; **lost sidecar → region defaults to host pixels** (safe) | ✅ | Idempotent directives; epoch fencing; absence = safe baseline |
| **WebTransport blocked** by a corporate proxy | Falls to WebSocket/TCP; video lane loses HoL-avoidance | 🟡 | Parallel per-lane WS (or lane-drop), cap res/fps; hostile networks → SPICE rung |
| Browser has **no HW HEVC decode** | Per-session codec negotiation | ✅ | Fall back to H.264 |
| Sidecar prepend pushes lead datagram **past 1200 B MTU** | Static "own datagram" fallback | 🟡 | **Always** emit sidecar as its own `frameSeq` datagram |
| User **screen-shares / scrolls a code editor** (text in the high-churn "video island") | ML super-res runs on the text → hallucinated glyphs | 🟡 | Text-detect island sub-tiles → cap to spatial upscale / host pixels; run legibility gate on island output |
| Late sidecar `epoch=N+1` arrives before the reliable `RECONFIG` | Client epoch still N → mismatch → region shows host pixels (safe, 1 frame wasted) | ✅ | Gate epoch advance on `apply_at_frame`; unify `streamEpoch` name |
| **Audio underruns** during a Teams/Zoom call | Ambiguous (doc 18 video-master vs doc 19 audio-master) | 🔴→fixed in ADR | **Persona-conditioned:** video-master for interactive (audio slips ≤125 ms), audio-master for passive media |
| Capability downgrade mid-session (WebGPU lost / battery saver) | Renegotiate down the ladder | ✅ | AIMD step-down-fast; thin-client floor (ADR 0009 host pixels) |
| Delegated op **NAK / times out** | Revert region to host pixels within 2–3 frames via overlap + intra-refresh | ✅ | Degrade-never-deny |

## E. Cross-cutting / whole-system

| Event | Now | Verdict | Mitigation |
|---|---|---|---|
| **Host + client + network all under pressure** at once | Two control loops (broker fair-share vs per-session AIMD) can fight; no NVENC-exhaustion policy | 🟡 | **Strict hierarchy:** broker owns the budget cap; per-session loop spends *within* it (can lower, never raise); NVENC = admission resource |
| Run on an **AMD RDNA3 host**, a guest hangs the GPU | amdgpu per-queue reset → `DEVICE_LOST` on one context → kill+reap one tenant (**better than GA102**) | 🟡 | Verify amdgpu/Xe reset uevent carries a per-tenant hint; fall back to per-context `DEVICE_LOST` attribution; unattributable device-reset → treat as whole-GPU |
| Full VM GPU lifecycle: attach → workload → crash → reap → re-admit | Core primitive sound; abrupt-crash resource release under-specified | 🟡 | Explicit reap sequence (ADR 0007); extend PHASE-0 done-criteria to an induced mid-submit crash |
