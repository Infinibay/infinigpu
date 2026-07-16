# 14 — Cross-VM fence, completion & timeout design (a deadlock-free arbiter)

**Scope:** design the synchronization/fence machinery for the per-VM replay arbiter (ADR 0003)
and the guest driver (ADR 0005) so that it **cannot deadlock** and **one guest can never freeze
another**. This is the program's #2 make-or-break risk. It builds on the multi-ring seqno protocol
(ADR 0004 / doc 11) and the blob-scanout present path (doc 09).

## Verdict up front

**CONFIRMED buildable, with a hard rule set.** Every primitive we need exists and is proven in
production virtualization stacks: Venus's `vn_ring` seqno completion, virtio-gpu's per-context
multi-timeline fences (`ring_idx`), gfxstream's 1:1 thread model, Vulkan timeline semaphores, and
the Linux `drm_sched` TDR path. The danger is not the primitives — it is **any global, shared, or
head-of-line-blocking wait**. The failure the task names is real and well-documented: helix.ml's
multi-desktop stack froze **all four desktops** because a *single* global `renderer_blocked`
counter gated command processing for every scanout, and blob-unmap RCU cleanup kept that counter
perpetually `>0` ([helix.ml GPU virtualization](https://blog.helix.ml/p/gpu-virtualization-architecture-for)).
The design below is organized around **never reproducing that class of bug**.

## 1. The failure mode we are designing against (verified)

Two independent, primary reference stacks show the exact trap:

- **helix.ml global freeze.** `renderer_blocked` is *global across all scanouts*. SPICE's GL path
  calls `graphic_hw_gl_block(true)` to pause command processing until the client acks a frame; with
  4 desktops, one slow client froze all four. Worse, blob-resource unmaps (heavy under Venus)
  incremented `renderer_blocked` during async RCU cleanup, so overlapping unmaps from 4 contexts
  held it `>0` forever. And the command loop used `QTAILQ_FIRST` + `break` on the first *suspended*
  command — a **FIFO head-of-line block** where one stuck blob-unmap stalled contexts 2, 3, 4 queued
  behind it. Their fix: delete `renderer_blocked` from the unmap path, switch the loop to
  `QTAILQ_FOREACH_SAFE` + `continue` (skip the suspended command, keep draining), and replace global
  pauses with **per-slot busy flags per scanout** ([helix.ml](https://blog.helix.ml/p/gpu-virtualization-architecture-for)).
- **virglrenderer's single-thread global state.** virglrenderer's public API "is not thread safe
  and must be called from a single thread … uses global state, so only one instance can operate in
  a process." Polling a fence on that one thread stalls *every* context, so upstream added "a thread
  [that] blocks for a single fence using a separate shared context, then uses eventfd to wake the
  main thread" ([virglrenderer fence thread](https://cgit.freedesktop.org/virglrenderer/commit/?id=89aea798b64bc998e82f32175c3e3ab3f342f64f),
  [virglrenderer docs](https://docs.rs/virglrenderer/latest/virglrenderer/index.html)).

**The four anti-patterns to ban outright:** (a) any counter/lock/semaphore whose scope spans more
than one VM context; (b) a synchronous wait on the thread that also decodes other work; (c) FIFO
break-on-block draining; (d) touching a shared gate during resource teardown (unmap/free). Our
ADR-0003 process-per-VM topology already kills the cross-VM case structurally — but the rules below
must hold *inside* each VM's arbiter too, because a VM has N contexts and multiple scanouts.

## 2. Seqno completion model (no synchronous host stalls)

Each command ring (one per guest context, ADR 0004) carries a **monotonic 64-bit submission
seqno**. The guest bumps it per `SUBMIT_CMD`; the host, after retiring work, writes the **highest
retired seqno** into a host-owned word in the ring header and raises MSI-X. This is exactly the
Venus `vn_ring` model, whose seqno "orders operations without requiring synchronous stalls"
([Mesa Venus](https://docs.mesa3d.org/drivers/venus.html)) — the same `ring_wait_seqno` primitive
Venus uses to order host/guest work ([Venus/gfxstream architecture](https://deepwiki.com/arehnman/virtio-win-mesa/7.7-virtualized-and-layered-drivers-(virtio-gpu-venus-gfxstream))).

A guest fence is just a seqno target. Waiting resolves in two tiers, **neither of which blocks any
host decode thread**:

1. **Non-blocking check:** the guest compares `retired_seqno >= target` by reading the shared word.
   This is a plain load — no ring round-trip, no host involvement. Most fence checks (Venus caches
   aggressively) never talk to the host at all.
2. **Blocking wait:** the guest registers interest and sleeps on its own interrupt. The host signals
   MSI-X when it advances `retired_seqno`; the guest KMD wakes the waiter. The guest exports this to
   userspace as a **`sync_file`/`drm_syncobj` fd** (modeled on virtio-gpu) so apps `epoll`/`poll`
   instead of busy-spinning. The wait lives entirely in the *guest*; the host never stalls waiting
   for a guest to observe a completion.

Critically, **the host decode thread never blocks on a guest-issued wait.** A guest `FENCE_WAIT`
that cannot yet be satisfied is *parked* (its ring entry marked pending) and the thread continues to
the next command — the `QTAILQ_FOREACH_SAFE` + `continue` discipline, applied per ring. In-fence
dependencies on `SUBMIT_CMD` are expressed as Vulkan timeline-semaphore waits enqueued on the host
queue (§3), so the GPU orders them; the CPU decode thread does not spin.

## 3. Guest fence ↔ host Vulkan timeline semaphore ↔ dma-fence

Three layers, one bridge each:

- **Guest seqno → host timeline semaphore.** Each command ring gets one **`VkSemaphore` of type
  `TIMELINE`** whose 64-bit payload tracks that ring's seqno. On replay, the host submits the guest's
  command batch with a device *signal* of value = submit seqno, and expresses in-fences as device
  *waits* on the relevant timelines. Timeline semaphores are purpose-built for this: a monotonically
  increasing 64-bit counter supporting device wait/signal **and** host query + host wait
  ([Khronos timeline semaphores](https://www.khronos.org/blog/vulkan-timeline-semaphores),
  [VK_KHR_timeline_semaphore](https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_timeline_semaphore.html)).
  The signal value must be *strictly greater* than the current payload — which the guest's monotonic
  seqno guarantees for free.
- **Retirement → `retired_seqno` word.** A dedicated **per-context completion poller thread** does a
  *host wait with timeout* (`vkWaitSemaphores`, or non-blocking `vkGetSemaphoreCounterValue`) on that
  ring's timeline. When it advances, the poller writes `retired_seqno` and raises MSI-X. This is the
  virglrenderer lesson made structural: the blocking wait is **off** the decode thread, on its own
  thread, per context. One context's slow GPU work never delays another's retirement.
- **Timeline semaphore → dma-fence (present only).** Scanout/present must interoperate with the
  kernel display stack, which speaks **dma-fence**. dma-fence is subject to hard kernel rules, and
  this is where deadlock lives.

**The dma-fence signalling rules (non-negotiable).** A dma-fence *must* signal in **bounded, finite
time**, because `dma_fence_wait()` is callable from memory-reclaim contexts (shrinkers, mmu
notifiers). Therefore **any code on the path to `dma_fence_signal()` must never allocate with
`GFP_KERNEL` and never take `dma_resv_lock`** — doing so can deadlock against reclaim that is itself
waiting on the fence ([kernel dma-buf docs](https://docs.kernel.org/driver-api/dma-buf.html),
[dma-fence lockdep annotations, Vetter](https://patchwork.kernel.org/project/intel-gfx/patch/20200612070623.1778466-1-daniel.vetter@ffwll.ch/)).
The kernel enforces this with `dma_fence_begin_signalling()`/`end_signalling()` lockdep annotations;
we run our present path under them in CI. And the harder constraint: **no indefinite / userspace /
"future" fences may be imported as dma-fences** — mixing sync and memory-management on one object is
an unresolvable deadlock, so the kernel bans it ([Tackling the indefinite/user DMA fence problem,
LWN](https://lwn.net/Articles/893704/), [König RFC](https://lore.kernel.org/all/20220502163722.3957-2-christian.koenig@amd.com/T/)).

**How we obey them.** A guest-controlled seqno is exactly the kind of *indefinite* signal the kernel
forbids as a dma-fence — a hostile guest could withhold it forever. So we **never** hand a raw guest
seqno to the display stack. Instead: the present dma-fence is created and signalled **only by the
host**, backed by the host timeline semaphore's *own* completion, which the host bounds with a
watchdog (§5). The flush poller thread pre-allocates the dma-fence, does no allocation in the signal
path, and calls only `dma_fence_signal()` (or `dma_fence_set_error()` then signal on timeout, the
`drm_sched` convention — [sched_main.c](https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/scheduler/sched_main.c)).
The guest's seqno gates the *guest's* view of its work; the *host* dma-fence gates scanout, and it
is always resolved by the host in bounded time. This is the firewall that keeps a guest's indefinite
wait out of the kernel's finite-time dma-fence world.

## 4. Per-VM (and per-context) isolation of waits

- **No shared global lock — 1:1 threads.** N command rings → N host decode threads, one per guest
  context, gfxstream's headline scalability fix over VirGL's single decode thread
  ([gfxstream](https://android.googlesource.com/platform/hardware/google/gfxstream/)). Plus one
  completion-poller thread per ring (§3). There is **no** process-wide renderer lock and **no**
  global `renderer_blocked` analogue anywhere in the design.
- **Per-context fence contexts with multiple timelines.** We adopt virtio-gpu's `ring_idx` model:
  fences tagged `VIRTIO_GPU_FLAG_INFO_RING_IDX` are "dispatched to be created on the target
  context," and signalling matches the `(ctx_id, ring_idx, fence_id)` tuple so each timeline
  advances independently — the fix that stopped Venus "frames going backwards" from cross-timeline
  ordering ([virtio-gpu multiple timeline](https://www.mail-archive.com/qemu-devel@nongnu.org/msg1123212.html)).
  A stall on one timeline cannot reorder or block another.
- **Backpressure is ring-local.** When a guest floods its ring, the *only* thing that fills is that
  guest's SPSC ring; the host applies ring backpressure to that context alone (ADR 0003 DoS caps).
  No cross-context queue exists to head-of-line-block.
- **Teardown never touches a shared gate.** The helix.ml root cause: blob-unmap incremented a global
  counter during RCU cleanup. Rule: **resource destroy/unmap paths take only per-resource state**,
  never a device- or context-global sync object, and a suspended teardown is *skipped over*
  (`FOREACH_SAFE`+`continue`), never allowed to `break` a drain loop.
- **Process-per-VM is the outer wall.** Because each VM is a separate jailed replay process (ADR
  0003), a hung wait, a leaked fence, or a corrupted timeline is confined to one address space; the
  broker and other VMs share no fence state at all. `kill(process)` reaps every GPU object the OS/
  driver tracked — more reliable than in-process fence bookkeeping.

## 5. Timeouts, watchdogs, and per-VM DEVICE_LOST recovery

A completion that never arrives must be *detected*, not waited on forever. Two watchdogs, an
escalation ladder, and contained recovery:

- **Submission watchdog (guest-side stall).** Per ring: if the producer publishes a seqno but the
  ring's descriptors are malformed / the payload never decodes, the decode thread times out that
  command, marks the ring's error word, and signals the ring's timeline with `dma_fence_set_error`
  semantics so downstream waiters fail fast rather than hang.
- **GPU-context watchdog (host-side TDR).** Per timeline, if the host `vkWaitSemaphores` does not
  advance within a configurable budget, we treat it like `drm_sched`'s `timedout_job`: the hardware
  fence "fail[ed] to signal in a configurable amount of time," so we set the error on the fence and
  begin recovery ([drm/sched docs, LWN](https://lwn.net/Articles/951811/)). Like the recent
  `drm_sched` "skip the reset and keep running" work, we first check whether the context is merely
  *slow but progressing* (seqno advanced since last poll) and, if so, rearm the timeout instead of
  killing it ([skip-reset patch](https://www.mail-archive.com/dri-devel@lists.freedesktop.org/msg543388.html)).
- **Escalation ladder (least-blast-radius first):** (1) fail the single outstanding fence with an
  error and let the guest retry; (2) if the whole context is wedged, tear down that guest context's
  Vulkan objects and return `VK_ERROR_DEVICE_LOST` to *that context only*; (3) if the guest ignores
  it or corruption spreads, `kill()` the VM's replay process — the OS/driver reclaim all its GPU
  state, and the broker respawns a clean arbiter for that VM; (4) only a full-device-reset-class Xid
  (79/45/62/48/119, the accepted GA102 residual in ADR 0003) escalates beyond one tenant, and that
  path quarantines the host, never silently freezes peers.
- **DEVICE_LOST is per-VM by construction.** On `VK_ERROR_DEVICE_LOST` the app must stop all ops,
  destroy the device and its children, and recreate device + resources
  ([Khronos: after a device lost](https://community.khronos.org/t/after-a-vk-device-lost-what-options-are-there/103923)).
  Because each VM owns its own `VkDevice` (never MPS, ADR 0003), that recreation is scoped to one
  tenant. The **arbiter/broker is never taken down** by a guest device-lost — it observes the child
  process exit and re-admits.

## 6. Present-path sync (tear-free, lock-free)

The frame is a **blob dma-buf** (doc 09). Present is fence-mediated, not lock-mediated:

- **Pin on flush, release on flush-fence.** On `RESOURCE_FLUSH`, the device pins the dma-buf for
  scanout and releases it only when the flush dma-fence signals; the guest does not recycle that blob
  generation until it observes the fence — so guest write and host read never overlap
  ([virtio-gpu blob dma-fence, Kasireddy](https://lore.kernel.org/qemu-devel/20210901211014.2800391-3-vivek.kasireddy@intel.com/T/)).
- **Rotate N blobs.** Double/triple buffering = the guest owns N blobs and rotates; the fence gates
  reuse. No lock, no tearing — the fence *generation* is the interlock (doc 09 §1).
- **Per-scanout busy slots, never a global block.** Directly adopting helix.ml's fix: each scanout
  has a small ring of present slots; if all slots are busy (encoder/console not drained), **that
  scanout drops a frame while every other scanout continues**. We explicitly do **not** use SPICE's
  `graphic_hw_gl_block`/`renderer_blocked` global pause ([helix.ml](https://blog.helix.ml/p/gpu-virtualization-architecture-for)).
  A slow console client costs *its own* VM dropped frames, nothing more.
- **The flush dma-fence obeys §3.** It is host-created, host-signalled in bounded time, allocated
  ahead of the signalling path, and runs under `dma_fence_begin/end_signalling` in CI.

## 7. The rule set that prevents the global freeze

1. No sync object (counter, lock, semaphore, gate) may have scope wider than one context/timeline.
2. Never block a decode thread on a wait — waits live on per-context poller threads or in the guest.
3. Drain loops skip suspended/pending work (`FOREACH_SAFE`+`continue`); never `break` on the first.
4. Resource teardown touches only per-resource state — never a shared gate.
5. Guest seqno is *indefinite*; it may never be imported as a dma-fence. Scanout dma-fences are
   host-created and host-signalled in bounded time.
6. Nothing on the dma-fence signalling path allocates `GFP_KERNEL` or takes `dma_resv_lock`.
7. Every wait has a watchdog; recovery escalates per-context → per-VM (`kill`), never per-host.
8. Present backpressure is per-scanout slot drop, never a global render pause.

## Sources

- helix.ml — GPU virtualization architecture (global `renderer_blocked` freeze, blob-unmap RCU, FIFO HOL, per-slot fix): https://blog.helix.ml/p/gpu-virtualization-architecture-for
- Kernel dma-buf / dma-fence docs (finite-time signalling, no `GFP_KERNEL`/`dma_resv_lock` in signal path): https://docs.kernel.org/driver-api/dma-buf.html
- Daniel Vetter — dma-fence lockdep annotations (`dma_fence_begin/end_signalling`): https://patchwork.kernel.org/project/intel-gfx/patch/20200612070623.1778466-1-daniel.vetter@ffwll.ch/
- Tackling the indefinite/user DMA fence problem (LWN summary): https://lwn.net/Articles/893704/
- Christian König — indefinite/user DMA fence RFC (no user/future fences as dma-fence): https://lore.kernel.org/all/20220502163722.3957-2-christian.koenig@amd.com/T/
- Khronos — Vulkan Timeline Semaphores (64-bit monotonic; device/host wait+signal+query): https://www.khronos.org/blog/vulkan-timeline-semaphores
- VK_KHR_timeline_semaphore reference (strictly-greater signal, host wait/query): https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_timeline_semaphore.html
- Mesa Venus driver (`vn_ring` seqno, async transmission, `ring_wait_seqno`): https://docs.mesa3d.org/drivers/venus.html
- Venus/gfxstream architecture (vn_ring, 1:1 threads): https://deepwiki.com/arehnman/virtio-win-mesa/7.7-virtualized-and-layered-drivers-(virtio-gpu-venus-gfxstream)
- gfxstream (1:1 thread-per-context, ASG io_uring-style ring): https://android.googlesource.com/platform/hardware/google/gfxstream/
- virtio-gpu context init multiple timeline (`ring_idx`, per-context fence contexts, `(ctx_id,ring_idx,fence_id)` signalling): https://www.mail-archive.com/qemu-devel@nongnu.org/msg1123212.html
- virglrenderer — fence-blocking thread + eventfd (single-thread global state, main-thread stall fix): https://cgit.freedesktop.org/virglrenderer/commit/?id=89aea798b64bc998e82f32175c3e3ab3f342f64f
- virglrenderer usage notes (API not thread-safe, global state, one instance/process): https://docs.rs/virglrenderer/latest/virglrenderer/index.html
- Linux drm/scheduler `sched_main.c` (TDR `timedout_job`, `dma_fence_set_error`): https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/scheduler/sched_main.c
- drm/scheduler documentation improvements (TDR, error bubbling) — LWN: https://lwn.net/Articles/951811/
- drm/sched skip-reset-keep-running (progress check, rearm timeout): https://www.mail-archive.com/dri-devel@lists.freedesktop.org/msg543388.html
- Khronos forums — recovery after VK_ERROR_DEVICE_LOST (destroy device+children, recreate): https://community.khronos.org/t/after-a-vk-device-lost-what-options-are-there/103923
- virtio-gpu blob scanout dma-fence pin/release (Kasireddy): https://lore.kernel.org/qemu-devel/20210901211014.2800391-3-vivek.kasireddy@intel.com/T/
