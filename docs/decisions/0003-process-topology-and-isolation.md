# ADR 0003 — Process topology & multi-tenant isolation

- **Status:** accepted
- **Date:** 2026-07-16
- **Feeds from:** research/10-security-isolation.md, research/06-data-plane-and-host-gpu.md

## Context

Infinibay is multi-tenant (departments + RBAC + per-VM fail-closed firewalls). The GPU-sharing
layer must not become the weak link. There is **no hardware isolation** on GA102 (no MIG/SR-IOV),
so isolation is only as strong as our software topology. The host replay decoder consumes an
**untrusted** guest command stream in a privileged position (a confused-deputy target; virglrenderer
has a guest→host CVE history, e.g. CVE-2022-0175).

## Options considered

- **One arbiter process per host, all VMs multiplexed inside it:** simplest, but one arbiter
  bug/crash/compromise, or one guest's head-of-line blocking, hits **every** tenant (helix.ml's
  global `renderer_blocked` froze all contexts). ❌
- **NVIDIA MPS to pack clients:** MPS multiplexes clients into **one shared context** → a single
  client's fatal fault destroys the shared context and kills all co-runners. **Banned.** ❌
- **One jailed replay process per VM + a small per-host broker:** blast radius of a bug/crash/
  compromise = one tenant; matches Infinibay's per-VM TAP+nftables model. ✅

## Decision

**Per-VM isolation:**
- A thin **privileged broker** (one per host) holds department/quota/policy state and admits context
  creation.
- **One unprivileged replay process per VM**, jailed (namespaces + seccomp-BPF + dropped caps, à la
  crosvm Minijail), each with **its own NVIDIA Vulkan context** (never MPS). `kill(process)` is the
  resource-reaping primitive (OS + driver reclaim all GPU state — more reliable than in-process
  bookkeeping).
- **Treat the guest command stream as hostile:** validate every descriptor/handle/offset against the
  per-VM ResourceTracker, reject unknown handles, **force host-side `robustBufferAccess`/robustness2
  ON** regardless of guest request, detect infinite descriptor chains, and write the decoder in
  **Rust** to retire the C-decoder CVE class.
- **DoS caps:** virtqueue/ring backpressure (self-limited to the guest's own ring), per-VM VRAM
  **admission caps** (refuse `vkAllocateMemory` past quota), low `VK_EXT_global_priority` +
  token-bucket submission throttle, and handle `DEVICE_LOST` as a per-VM fault.
- **Fail-closed:** no GPU access unless department/VM policy allows.

## Consequences

- **Positive:** guest compromise and the common GPU-fault case (Xid 13/31/43, contained by NVIDIA
  Robust Channels) stay within one tenant; clean mapping to Infinibay RBAC + Prisma quota
  (`gpuEnabled` default false, `vramCapMB`, `priorityTier`, `maxConcurrentGpuVMs`,
  `submissionRateTokens`) + Socket.IO/Postgres audit.
- **Negative / accepted (the irreducible residual):** a severe GPU fault class (Xid 79/45/62/48/119)
  forces a **device-wide reset that downs all tenants**; no software fully prevents this on GA102
  (the only real fix is MIG, which this card lacks). **Documented, monitored, quarantined on any
  full-reset-class Xid.** Per-VM processes cost more memory/context-switch than a shared arbiter.
- **Follow-up / NEEDS VERIFICATION:** exact per-Xid contained-vs-device-reset boundary against
  NVIDIA's official Xid table + open-gpu-kernel-modules; whether an Ampere engine reset always
  recovers cleanly vs. escalating to full-device reset.
- **Top-3 MVP must-dos:** (1) one jailed replay process per VM with its own context, never MPS;
  (2) hostile-input validation + forced host robustness + Rust decoder; (3) fail-closed admission
  control + fault quarantine.
