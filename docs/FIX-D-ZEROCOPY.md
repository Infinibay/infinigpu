# Fix D — zero-copy scanout (guest-memfd import)

The last big cost on the 3D-submit hot path is the **CPU copy of the finished frame** into the guest
scanout — ~1 ms/frame at 1080p, pure memcpy (see `PERF-AUDIT.md`, phase attribution). Fix D removes it:
instead of rendering into a host readback buffer and copying, the GPU **DMAs the frame straight into the
guest's scanout RAM**. This doc is the design + the shell-validated core; the device wiring is in place,
gated off, ready to validate end-to-end on the A5000 host.

## Idea

Guest RAM is a `memory-backend-memfd,share=on` region that QEMU hands the vfio-user device over
`DMA_MAP` (an `SCM_RIGHTS` fd); the device `mmap`s it once (`DmaTable`), so any guest-physical address is a
pointer-add into a stable host mapping. `VK_EXT_external_memory_host` lets us import an arbitrary
**host pointer** as `VkDeviceMemory`. So we import the scanout region's host pointer as a `TRANSFER_DST`
`VkBuffer` and make it the destination of the existing `cmd_copy_image_to_buffer`. The GPU's DMA lands the
`R8G8B8A8` result directly in guest RAM — **no readback buffer, no CPU copy**.

```
 before:  GPU image ──copy_image_to_buffer──► host readback buf ──CPU memcpy──► guest scanout (2 hops, ~2ms)
 Fix D:   GPU image ──copy_image_to_buffer──────────────────────────────────► guest scanout (1 hop, GPU DMA)
```

## Why it's safe / correct here

- **Pitch:** the 3D `VulkanWorkload` scanout is **tightly packed** (`pitch = width*4`, no padding — unlike the
  2D `ScanoutPresent` path), so `cmd_copy_image_to_buffer` with the default row length matches exactly. No
  stride handling.
- **Contiguity:** `DmaTable::host_ptr(addr, len)` is fail-closed — it returns a pointer only if `[addr, addr+len)`
  fits inside **one** mapping, i.e. a contiguous host VA. Guest RAM is a single memfd mapped once, so the
  scanout is contiguous. If it ever isn't, `host_ptr` returns `None` → fall back to the copy path.
- **Alignment:** `minImportedHostPointerAlignment` = **4096** on the A5000. The mmap base is page-aligned and
  DRM/KMS framebuffers are page-aligned, so the scanout host pointer is 4 KB-aligned; we still check at
  runtime and fall back if not. The imported size is rounded up to a page (the extra bytes stay inside the
  mapping — verified by asking `host_ptr` for the rounded length).
- **Coherency:** on x86, PCIe DMA into system RAM is snooped (cache-coherent), so the guest CPU sees the GPU's
  writes after the fence — no explicit flush. (The old readback `invalidate` was for the CPU *reading* a
  HOST_CACHED host buffer; here the CPU never reads on the host side.)
- **Lifetime:** the import caches a raw host pointer into the memfd mmap. If the device remaps guest RAM
  (`DMA_MAP`/`DMA_UNMAP`), that pointer can dangle — so the device calls `forget_all_guest_imports()` on **any**
  remap, and the next zero-copy render re-imports lazily. One submit thread per process ⇒ no concurrent
  forget-vs-submit. `free_memory` on an imported allocation frees only the Vulkan handle, never the guest RAM.
- **Trust:** the GPU only *writes* the guest's own scanout; a hostile guest mutating it concurrently can't
  corrupt the host (we never read it back on the host).

## Shell validation (done, on the A5000)

Two things were validated from the shell without a full guest/QEMU stack — the two things that could have
sunk the approach:

1. **Capability** (`cargo run -p infinigpu-replay --bin probe_extmem`): both RTX A5000s report
   `VK_EXT_external_memory_host = true`, `minImportedHostPointerAlignment = 4096`,
   `VK_EXT_external_memory_dma_buf = false` (so host-pointer import is the right path, not dma-buf).
2. **Mechanic** (`render_forwarded_zerocopy_matches_builtin`, a `#[ignore]` GPU test): render a triangle
   straight into a page-aligned host buffer (standing in for guest scanout RAM) and assert the pixels are
   **byte-identical** to the copy path. Passes: `lit=11858/65536, matches builtin`.

### Measured (RTX A5000, `bench_forwarded`, 1080p, single VkDevice; `BENCH_PRESENT=0/1/2`)

| Path | p50 | p99 | p999 | submit/s |
|------|----:|----:|-----:|---------:|
| two-copy (pre-audit prod) | 3080µs | 3342µs | 9666µs | 321 |
| one-copy present | 2322µs | 2599µs | 4488µs | 424 |
| **zero-copy (Fix D)** | **1325µs** | **1344µs** | **1352µs** | **757** |

vs the original two-copy path: **p99 −60%, p999 −86%, throughput ×2.4**, and a *near-flat* distribution
(p50→p999 spans 27µs) because the ~1 ms cache-cold memcpy and its page-fault tail are simply gone.

**4 VkDevices @1080p (multi-VM):** vs one-copy, zero-copy gives **+33% aggregate throughput, −23% p50, −15%
worst-p999**; worst-p99 rises slightly (7.6→8.3 ms) because removing the CPU copy exposes raw GPU contention —
under saturation the tail is GPU-bound, which is the shared-broker's job, not this fix's.

## Implementation (in tree, gated `INFINIGPU_ZEROCOPY_SCANOUT`, default off)

- `infinigpu-replay`:
  - `HostGpu::open` enables `VK_EXT_external_memory_host` when present and records
    `min_imported_host_pointer_alignment`; `supports_zerocopy_scanout()` exposes availability.
  - `import_guest_buffer(ptr, size)` — `vkGetMemoryHostPointerPropertiesEXT` → pick a HOST_VISIBLE type →
    allocate `VkDeviceMemory` with `VkImportMemoryHostPointerInfoEXT` → create+bind a `TRANSFER_DST` buffer.
  - `render_forwarded_zerocopy(w,h,bg,draw, guest_ptr)` — reuses the cached `(w,h)` `SizedScratch` for the
    render, records the copy into the imported buffer (shared `record_forwarded_frame`), submits, waits. No
    CPU copy, no invalidate. Imports cached by `(ptr,size)`; `forget_all_guest_imports()` drops them.
  - Validated by `render_forwarded_zerocopy_matches_builtin`.
- `infinigpu-device`:
  - `SharedGpu`: `supports_zerocopy()`, `render_forwarded_zerocopy(...)`, `forget_all_guest_imports()`.
  - `submit_vulkan` FORWARDED: if enabled + supported + `host_ptr(scanout, aligned)` resolves → run the
    zero-copy render under the fair-share ticket (profiler folds the writeback into the render hop, `dma_us=0`);
    otherwise the one-copy present. `dma_map`/`dma_unmap` call `forget_all_guest_imports()`.

## What still needs the owner's host to validate

1. **End-to-end on a real GPU VM:** set `INFINIGPU_ZEROCOPY_SCANOUT=1` (+ `INFINIGPU_SCRATCH_CACHE=1`) on the
   device-server env, boot a GPU VM, confirm the guest page-flip shows correct frames, and check
   `INFINIGPU_PROFILE=1` shows the `dma` hop → ~0 and `total` p99 dropping by ~1 ms/frame vs the copy path.
2. **Real scanout address:** confirm the guest's DRM/KMS framebuffer address is page-aligned and mapped as one
   interval in practice (the code already falls back safely if not — watch for "falling back" staying silent,
   i.e. that zero-copy actually engages; add a one-time log if useful).
3. **Remap churn:** exercise mode-sets / resolution changes to confirm `forget_all_guest_imports` on remap is
   correct (no stale import, no crash).

Only after (1)–(3) pass should `INFINIGPU_ZEROCOPY_SCANOUT` be considered for default-on (alongside the
already-justified `INFINIGPU_SCRATCH_CACHE`).
