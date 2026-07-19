# Micro-optimization playbook

What I learned doing the infinigpu 3D-submit perf audit, distilled into a reusable method. The examples
are from the GPU hot path (`guest ICD → SUBMIT_FORWARDED → device server → NVIDIA Vulkan → DMA writeback`),
but the discipline transfers to any latency-critical path. Read this before optimizing; it will stop you
optimizing the wrong thing.

## The one rule

**No perf change without p99 before *and* after, measured under the load you actually run.** A mean hides
the tail; a single-thread benchmark hides contention. If you can't measure the win, you don't have one —
you have a guess that also added risk. (Bugs are different: fix those on sight.)

Everything below serves that rule.

## Method (the loop)

1. **Reframe first — find where the time *can't* be.** Before profiling, rule out whole regions. Our data
   plane was already zero-copy (guest RAM mapped once, socket carries only control) — so no amount of
   "optimize the transport" would help; the cost was per-submit work. Knowing where **not** to look is half
   the win. Also check the code is even *reached*: our fair-share/admission layer turned out **inert** in
   production (one device process per VM), so "optimize the scheduler" was zero-value until a shared broker
   exists. Verify the topology before you tune it.
2. **Build a micro-benchmark that isolates the hop.** `bench_forwarded` runs just the render hop N times and
   prints p50/p90/p99/p999/mean + throughput. Small, dependency-light, reproducible. This is the instrument
   you'll A/B against for the rest of the work.
3. **Add per-phase breakdown instrumentation, opt-in and zero-cost when off.** `INFINIGPU_BREAKDOWN=1` split
   the frame into setup/record/gpu/copy. It immediately showed the **readback copy was 72%** of a small
   frame — which no one would have guessed. You cannot optimize what you haven't attributed to a phase.
4. **Measure the baseline across sizes and regimes**, not one point. Single-VM *and* N-concurrent (= N
   tenants); small *and* realistic input sizes. The regime changes the answer (see traps below).
5. **Form a hypothesis, make the smallest change, re-measure the same way.** Keep the change behind a flag so
   the A/B is one binary.
6. **Verify correctness every time.** Re-run the golden output test (`render_forwarded_matches_builtin`) after
   each change. A faster wrong answer is worthless.
7. **Keep it or kill it based on the numbers**, and write the numbers down (commit message + the audit doc).

## Traps that will fool you (each one bit us)

- **The single-VM mean lies about the multi-VM tail.** Fix A (pipeline cache) barely moved the *fleet*
  worst-p99 — because under contention the **allocation churn**, not the compile, dominated. The mean said
  "done"; the tail said "you haven't started." Always measure the metric that matters (tail, under load).
- **A win at one input size can be a loss at another.** The one-copy present change was *slightly negative* at
  256×256 (fixed overheads dominate, both copies are cache-hot) and **−61% p999 at 1080p** (an 8 MB per-frame
  alloc that mmap/munmaps every frame). Measure at the size you actually ship.
- **Instrumentation has an observer effect.** `INFINIGPU_BREAKDOWN`'s `Instant::now()` calls inflated the mean
  (78→94µs). Use heavy instrumentation for **ratios/attribution**, not absolute numbers; take the headline
  numbers with instrumentation off.
- **"Allocation contention" may not be where you think.** Each VM is a separate process → separate address
  space, so the *CPU heap* allocator doesn't contend across VMs. The contention that mattered was the *GPU
  driver* allocator (shared device). Name the actual shared resource before "fixing contention."
- **Throughput can go up while QoS goes down.** The 2-copy path had higher aggregate submit/s — because one
  VM raced ahead and starved the others. The 1-copy path was "slower" in total but **fair and predictable**.
  Under multi-tenant, low variance + fairness beats peak throughput.

## Hardware truths worth memorizing

These are the discoveries that produced the biggest wins — they're not obvious from the code:

- **Not all "host-visible" memory is cached.** On NVIDIA, `HOST_COHERENT` is write-combined / **uncached** —
  CPU *reads* crawl (that was the 72%-of-frame readback). Prefer `HOST_CACHED` and pay the explicit
  `vkInvalidateMappedMemoryRanges` before reading GPU-written data. Result: copy 221→32µs (−86%). Know your
  memory types; an uncached read over PCIe is a cliff.
- **Blocking waits cost a context switch.** `vkWaitForFences` sleeps and is woken by an IRQ — tens of µs of
  scheduler latency. When the thing you're waiting for usually finishes fast, **spin-poll the status first**
  (`vkGetFenceStatus`) for a bounded window, then fall back to blocking. Won single-VM p99 203→85µs. Caveat:
  a spin *burns a core*, so on a CPU-oversubscribed host it steals cycles from vCPUs — default it off, bound
  it small, enable where cores are plentiful.
- **Large allocations are syscalls in disguise.** glibc serves anything over ~128 KB with `mmap`, freed with
  `munmap` — a syscall pair **and** first-touch page faults **every allocation**. At video frame rates that's
  thousands of syscalls/s and the source of **tail spikes** (our p999 was 9 ms from this alone). Reuse a
  persistent buffer; the memcpy stays, the syscalls and faults vanish.

## The techniques (a menu, cheapest first)

1. **Memoize invariant work, keyed by a cheap hash.** Pipeline/shader compiles cached by SPIR-V hash across
   submits (100% hit in steady state). Anything recomputed *identically* each iteration is a candidate. Bound
   the cache and evict fail-closed so a hostile/pathological input can't blow memory.
2. **Reuse, don't allocate, in the hot path.** Persistent per-(w,h) scratch (image/memory/framebuffer/
   readback), a **persistent mapping** (map once, not per frame), reused command pools/fences. If you *must*
   allocate, keep it local to the consumer and prefer large pages.
3. **Remove a copy by changing ownership, not by copying faster.** You can't drop a copy while the API forces
   owned data. We added `render_forwarded_present(present: FnOnce(&[u8]))` so the consumer copies **directly
   from the source mapping** into its destination — two copies → one, zero intermediate alloc. The
   optimization *was* the API change (borrow-and-callback instead of return-owned).
4. **Cut context switches.** Spin before block (fences); avoid needless thread wakeups, lock hand-offs, and
   `thread::sleep` on the inline path; batch to amortize syscalls.
5. **Place work for locality (bigger lever, more setup).** Pin the worker and its memory to the device's NUMA
   node; prealloc + bind guest RAM; huge pages. Gated because it depends on host topology.
6. **Pipeline / go async (biggest change, last resort).** Overlap stage N+1 with stage N (submit the next
   frame while reading back the current one); replace a synchronous trapped doorbell with an event + IRQ
   completion. High value, high risk — only with before/after p99 and careful correctness work.

## Engineering hygiene that made this safe

- **One independent env flag per fix, default off, A/B on a single binary.** `INFINIGPU_PIPELINE_CACHE`,
  `INFINIGPU_SCRATCH_CACHE`, `INFINIGPU_FENCE_SPIN_US`, `INFINIGPU_NUMA_NODE`. Zero-cost when off. Lets you
  attribute each delta to exactly one change and roll back instantly.
- **Isolate the fast path from the default path.** The cached render lives in its own method; with the flag
  off the original code runs untouched, so "off" is provably identical to before.
- **Ship the measurement harness with the fix** (the bench, the breakdown, the profiler). The next person
  reproduces your number instead of trusting it.
- **Attribute cost to named hops** (decode/wait/render/dma) and confirm a fix moved the *right* hop — not a
  neighbouring one you didn't mean to touch.
- **RAII/guards on the hot path too.** Persistent objects still need fail-closed cleanup on every early
  return, or a perf refactor leaks resources on the error path.

## Pre-flight checklist

- [ ] Do I have a p99/p999 baseline, under realistic **load** and **input size**?
- [ ] Have I attributed the cost to a specific hop/phase (not guessed)?
- [ ] Is the change behind a flag, default-off, isolated from the default path?
- [ ] Did the golden correctness test pass after the change?
- [ ] Did I re-measure the **same** way, and does the tail actually improve (not just the mean)?
- [ ] Did I check it doesn't regress at other sizes / higher concurrency / oversubscription?
- [ ] Are the numbers written down where the next person will find them?

See `PERF-AUDIT.md` for the concrete findings, measured tables, and flag reference this playbook came from.
