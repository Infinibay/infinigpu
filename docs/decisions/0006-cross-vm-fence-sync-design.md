# ADR 0006 — Cross-VM fence / completion / timeout design

- **Status:** accepted
- **Date:** 2026-07-16
- **Feeds from:** research/14-fence-sync-design.md, research/06-data-plane-and-host-gpu.md, research/13-redteam-showstoppers.md (S1)

## Context

The #1 red-team showstopper (S1) is that the only field report of this architecture (helix.ml)
**deadlocks at 4+ concurrent desktops**: one slow desktop froze all four. If one guest can freeze
others, multi-tenant VDI is dead. This ADR fixes the synchronization design so that **no guest can
stall another**, using only primitives already proven in production.

## Root cause (from the field report)

helix.ml froze because a **single global `renderer_blocked` counter** gated *every* scanout's command
processing, and an async blob-unmap kept it perpetually >0; the command loop also used FIFO
`QTAILQ_FIRST`+`break`, so one suspended item **head-of-line-blocked** every context behind it. It was
a specific shared-state/HOL bug, **not** an inherent property of API-remoting.

## Decision

**Per-context seqno completion, bridged to Vulkan timeline semaphores, with host-bounded present
dma-fences, 1:1 decode/poller threads, `ring_idx` multi-timelines, and a per-context→per-VM-kill
watchdog ladder.** Concretely:

- **Completion = seqno, never a blocking call on a decode thread.** Each command ring has a monotonic
  64-bit submit seqno + a host-written retired-seqno word raised via MSI-X (Venus `vn_ring` model).
  A guest fence-wait is a plain shared-memory load or an in-guest interrupt sleep exported as a
  `sync_file`/`drm_syncobj` fd. The host decode thread **never blocks** on a guest wait — unsatisfiable
  waits are parked and the loop continues.
- **Three-layer bridge:** guest seqno → **one Vulkan timeline semaphore per ring** (signal = seqno) →
  retirement observed by a **dedicated per-context completion-poller thread** doing host
  wait-with-timeout → writes retired-seqno. Present-path only: timeline → a **host-created dma-fence**.
- **The dma-fence firewall (kernel rule, load-bearing):** a dma-fence **must** signal in bounded time
  and the kernel **bans importing indefinite/user fences** as dma-fences. So a guest-controlled seqno
  is **never** handed to the display stack; scanout dma-fences are **host-created and host-signalled
  in bounded time**, backed by a watchdog. A guest that stops advancing is **force-completed with an
  error** — the guest driver must handle a present fence that signals with error.
- **Isolation:** N command rings → N host decode threads (gfxstream 1:1) + N poller threads; per-context
  fence contexts using virtio-gpu `ring_idx` multi-timelines so timelines advance independently;
  ring-local SPSC backpressure; **process-per-VM (ADR 0003) is the outer wall** — no cross-VM fence
  state, and `kill(process)` reaps all GPU objects.
- **Watchdog / recovery ladder (least blast radius first):** fail one fence
  (`dma_fence_set_error`) → tear down one guest context (`VK_ERROR_DEVICE_LOST` to that context only) →
  kill the VM's jailed replay process (broker respawns) → only a full-reset-class GA102 Xid escalates
  host-wide (quarantine — the accepted ADR-0003 residual). Modeled on `drm_sched` `timedout_job`,
  including "skip reset if seqno still advancing, rearm timeout." **The arbiter/broker is never taken
  down by a guest device-lost.**
- **Present sync is fence-mediated, not lock-mediated:** pin blob dma-buf on `RESOURCE_FLUSH`, release
  on the flush-fence, rotate N blobs; per-scanout slot-drop on backpressure so a slow console client
  only costs its own VM dropped frames.

### The 8-rule invariant (reviewable; wire `dma_fence_begin/end_signalling` lockdep into CI)

1. No sync scope wider than one timeline. 2. Never block a decode thread. 3. Teardown touches no
shared gate. 4. Drain with `FOREACH_SAFE`+`continue`, never `FIRST`+`break`. 5. A guest seqno never
becomes a dma-fence. 6. No `GFP_KERNEL`/`dma_resv_lock` in a fence signal path. 7. Every wait has a
watchdog escalating per-context→per-VM-kill. 8. Per-scanout slot-drop backpressure.

## Consequences

- **Positive:** rebuts S1's deadlock half with a design from proven primitives; one slow/hostile guest
  costs only its own dropped frames + eventual per-VM kill; the broker/arbiter stay alive.
- **Negative / accepted:** guest and scanout timelines are deliberately **decoupled** (a stalled guest
  gets an error-signalled present, not a hang); watchdog timeout is a real tuning knob (too tight kills
  long compiles, too loose lets a context wedge) — needs empirical per-workload tuning. The
  **device-wide GA102 reset residual (S1 half 2)** is untouched by fence discipline (ADR 0003).
  The **Windows/WDDM guest fence mapping is unbuilt** (deferred to M3, ADR 0005).
- **MVP:** implement the full multi-ring/`ring_idx` ABI but instantiate **one** ring + one poller + one
  timeline; prove the retired-seqno→MSI→guest `sync_file` loop end-to-end before scaling to N contexts.
- **Verification gate:** reproduce the helix.ml N=4–8 wall with this design **before** building the
  scheduler (RISKS.md burndown step 4).
