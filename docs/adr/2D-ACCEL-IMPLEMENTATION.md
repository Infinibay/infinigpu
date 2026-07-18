# 2D Acceleration — Implementation Plan

**Status:** Proposed (2026-07-18). Design synthesized from an adversarial multi-agent
review of the real tree (ABI, guest driver, device, HostGpu, integration). Supersedes the
"2D-first quick win" rung in `ROADMAP.md` with a concrete, PR-by-PR plan.

## Context

Today every guest page-flip sends `DISPLAY_SCANOUT` (encoding `0x0101`; `ScanoutPresent`
in `crates/infinigpu-abi/src/wire.rs` has **no damage rect**). The device
(`present_scanout`, `crates/infinigpu-device/src/lib.rs`) DMA-reads the **entire**
framebuffer and runs a **scalar per-pixel CPU repack** to BGRA, then feeds ffmpeg via
`PixelStreamer::submit_bgra`. During a drag/scroll that is ~8 MB read + ~2 M-pixel scalar
work **every flip**, even when a few percent of the screen changed.

This plan moves the **host present/transfer/encode** path onto the A5000 in ordered,
independently-shippable rungs, and builds — in dependency order — the exact infrastructure
3D needs next (real ring drainer, per-VM `ResourceTable`, fence-retire, composite→encode).

### Honest limit (do not oversell)

The guest still rasterizes 2D into a **dumb framebuffer on llvmpipe (CPU)** because the
driver advertises no `DRIVER_RENDER` node (that is the [3D plan](3D-ACCEL-IMPLEMENTATION.md)).
So a software-heavy repaint stays guest-CPU-bound. The win here is **lower host CPU, lower
transfer, lower latency during interaction, and a smooth cursor** — not faster guest drawing.
And for the common fbcon `XRGB8888` case the BGRA "convert" is just forcing `alpha=255`, so
**most of the measurable CPU win is captured by the damage-only rung (PR2+PR3) alone**; the
GPU convert earns its keep via the cursor composite and by enabling the readback-free
dma-buf path.

## Decisions

1. **Additive damage encoding, not grow-in-place.** Add a new `encoding::DISPLAY_SCANOUT_DAMAGE = 0x0102`
   with a new 40-byte `ScanoutPresentDamaged { width, height, pitch, format, scanout_addr@16, dx, dy, dw, dh }`.
   The existing 24-byte `ScanoutPresent` (`0x0101`) is **untouched**, so today's guest/host
   keep working with **no lockstep rebuild**. (The alternative — grow `ScanoutPresent` 24→48
   in place — preserves `offset_of(scanout_addr)==16` but forces a same-release guest+host
   rebuild.) Regardless of choice, the host decoder **must** switch from a fixed
   `size_of::<T>()` read to a `min(payload_len, size_of)` zero-filled read — a ~3-line change
   in `process_ring`, and a hard prerequisite.
2. **Everything is `DEV_CAPS`-gated and fails safe to full-frame `0x0101`.** A new
   `caps::DISPLAY_ACCEL` bit is advertised by the device; the guest only emits the accelerated
   present when it sees the bit. An old device, an old guest, denied broker admission, or **any
   GPU/encoder error** degrades to today's working full-frame path — **never black**.
3. **GPU work never runs on the vfio-user callback thread.** `present` is serviced
   synchronously on the single vfio-user thread; adding GPU work + broker throttle there would
   freeze the guest vCPU (the same class of bug as the encoder-respawn mouse-lag freeze fixed in
   `f14ad69`). From PR5 on, the convert/composite runs on a
   **per-VM worker thread** behind a **latest-wins Mailbox**, billed inside `ticket.run()`.
4. **Defer the contested `CursorUpdate` byte layout.** Reserve only the opcode (`0x0042`) in
   PR1; freeze the body in PR6, by which point the PR4 `ResourceTable` exists (so a res_id-based
   cursor is an option).

## PR sequence

Rungs **PR1–PR3** (~3.5–4 wk) deliver a measurable interaction win on their own. **PR4** is
the pivot that makes everything after it reusable for 3D.

### PR1 — ABI: additive damage encoding + caps + conformance *(S, ~0.5 wk; no behavior)*
- `wire.rs`: `DISPLAY_SCANOUT_DAMAGE = 0x0102`, `struct ScanoutPresentDamaged` (40B, `repr(C)`,
  zerocopy derives), `capset::CAP_DISPLAY_2D`.
- `regs.rs`: `caps::DISPLAY_ACCEL` + `PHASE1_DEV_CAPS`.
- `lib.rs`: compile-time `layout_asserts` (size==40, `scanout_addr`@16); bump `ABI_MINOR` 1→2 in
  `ids.rs`; update the `abi_version` test.
- `cbindgen.toml` include list + `scripts/gen-abi-header.sh`; `_Static_assert`s in
  `guest/include/abi_conformance.c` (compiled `-Werror` = the cross-language gate).
- **Accept:** `cargo build/test -p infinigpu-abi` (layout asserts compile only if exact;
  `abi_version()==0x0000_0002`); `gen-abi-header.sh` regenerates the header and compiles
  `abi_conformance.c` clean — the C view byte-matches Rust.

### PR2+PR3 — Damage path: guest emits merged damage, device CPU-patches only dirty rows **(FIRST VISIBLE WIN)** *(M, ~2.5–3 wk)*
- **Guest** (`guest/linux/infinigpu.c`): `accel_2d` from `caps & DISPLAY_ACCEL` at probe;
  `igpu_pipe_update` extracts the merged damage box via `drm_atomic_helper_damage_merged`
  (the clip `drm_gem_fb_create_with_dirty` at line 234 already computes and throws away) and
  emits `DISPLAY_SCANOUT_DAMAGE`; first flip after modeset / NULL damage sends full-frame;
  `!accel_2d` keeps `igpu_submit_scanout` (`0x0101`) unchanged. `BUILD_BUG_ON(sizeof==40)`.
- **Device** (`crates/infinigpu-device/src/lib.rs`): a `DISPLAY_SCANOUT_DAMAGE` match arm +
  `present_scanout_damaged` that validates/clamps the rect (reuse the fail-closed geometry
  guard), DMA-reads only damaged rows, patches them (alpha-force for XRGB / channel-swap for
  RGBA) into a **new persistent per-VM `ScanoutBuffer`**, then the existing `stream_frame`.
  Advertise `DISPLAY_ACCEL`.
- **Risk:** partial-frame corruption if the persistent buffer goes stale on resize / dropped
  damage → force full-frame on modeset/resize + reset on size change.
- **Accept:** 1080p GPU VM, scripted `xdotool` drag/scroll; per-present timer + bytes-read
  counter beside the infiniPixel stats line. Mean bytes-read/present drops ~8 MB → ~`dw*dh*4`;
  device-process CPU during drag drops materially; colors correct for XRGB & RGBA. Fallback
  proof: accel guest against a `PHASE0_DEV_CAPS` device → `0x0101` unchanged. Safety: a unit
  test with `dx+dw>width` / overflowing dims → rejected-or-clamped, seqno still retires, no OOB.

### PR4 — Real ring drainer + sparse-mmap index page + per-VM `ResourceTable` + fence retire **(THE 3D FOUNDATION)** *(L, ~3–3.5 wk device + ~1 wk guest)*

> **Status (core landed, transport + guest hardware-gated):** the pieces that are *pure logic* are
> built and unit-tested off-hardware — the loom-`repr(C)` `Indices`/`from_ptr` view (PR1.3,
> `infinigpu-ring`), the fail-closed `ResourceTable` (`resource.rs`, 5 tests), `DmaTable::host_ptr`,
> and now the **two-phase bounded drainer** (`drain.rs`: `pop_batch` + `ring_over_shared` +
> `retire_over_shared`, 6 tests including the full push-N → pop-bounded → retire cycle over one
> shared page — the ADR's *biggest structural risk*, the `repr(C)` viewer + the borrow split,
> proven with owned buffers standing in for the sparse-mmap page). The **phase-2 dispatch** is also
> built + tested: `dispatch.rs`'s `execute_resource` decodes
> `RESOURCE_CREATE_BLOB/ATTACH_BACKING/SET_SCANOUT_BLOB/RESOURCE_FLUSH/RESOURCE_DESTROY` into the
> `ResourceTable` fail-closed (6 tests), backed by the additive `AttachBacking`+`MemEntry` ABI (0.4,
> layout-asserted + C-conformance green).
>
> **And the live wiring is now landed + tested off-hardware (no QEMU needed).** A **DMA-resident
> ring** (index page + descriptor array in guest RAM, addressed by the new `CMD_RING_INDEX` /
> `CMD_RING_BASE` / `CMD_RING_SIZE` registers; non-zero `CMD_RING_INDEX` switches a ctx to the
> drainer, zero keeps the Phase-0 single-descriptor fallback) is driven by `process_ring`→`drain_ctx`
> (two-phase: pop under the ring view → `execute_descriptor` under `&mut self` → retire on the shared
> page). `RESOURCE_FLUSH` routes to `present_resource_flush`, which resolves the blob's phase-1
> single-segment backing and reuses the damage-present path. The **PR4 accept criterion** is a green
> unit test (`pr4_real_ring_drain_presents_a_blob_resource`): over a memfd standing in for guest RAM,
> it programs the ring via BAR0, publishes CREATE_BLOB→ATTACH_BACKING→SET_SCANOUT_BLOB→RESOURCE_FLUSH
> through the SPSC producer, rings the doorbell, and asserts `head==tail` + `seqno_retired==N` +
> resource/scanout registered + the presented BGRA matches the blob + per-VM isolation + fail-closed
> on an unknown resource.
>
> **Guest half — protocol now verified off-hardware (cross-language).** The `.ko`'s PR4 wire
> protocol (the SPSC ring *producer* + the `RESOURCE_CREATE_BLOB/ATTACH_BACKING/SET_SCANOUT_BLOB/`
> `RESOURCE_FLUSH` payload construction) is implemented as a freestanding C reference
> (`crates/infinigpu-guest-conformance/csrc/guest_ring_ref.c` — the exact logic the `.ko` mirrors,
> `_Static_assert`-pinned) and driven through the **tested Rust device consumer** (`drain` +
> `dispatch`) by `tests/interop.rs`: the guest builds a real ring + `RESOURCE_*` stream, the device
> drains it, and every message decodes to the intended `ResourceTable` effect with the guest reading
> back the retired seqno — byte-level guest↔device interop, no QEMU. Also checks the shared full-ring
> backpressure condition. So the transcription-risk half of the guest driver is verified.
>
> **And the guest `.ko` PR4 support is now written + compile-verified** (kernel 6.14 headers), behind
> a default-off module param `infinigpu.ring_drainer=1` so today's tested single-descriptor path is
> unchanged. `infinigpu.c` gained: the shared `RingIndices` page + a descriptor/payload ring
> (`dmam_alloc_coherent`), the in-kernel SPSC `igpu_ring2_push` (mirrors the interop-verified C
> reference; tail publish via `smp_store_release`), `igpu_resource_register` (CREATE_BLOB +
> ATTACH_BACKING + SET_SCANOUT_BLOB with a 4-entry FB→`res_id` cache + round-robin DESTROY),
> `igpu_flush_resource` (RESOURCE_FLUSH of the damage rect), probe-time `CMD_RING_INDEX/BASE/SIZE`
> programming, and `BUILD_BUG_ON` layout pins. The `igpu_flush`/`igpu_flush_damaged` paths route
> through it when the param is set (full-ring drops the frame rather than corrupting the ring base;
> the legacy selftest is skipped on this path).
>
> **Remaining (genuinely runtime/hardware-gated):** **runtime validation of the guest `.ko` under
> QEMU** (does the guest↔device actually exchange frames over the real ring on a running system —
> the FB lifecycle, DMA coherency, retire timing); the `build_regions(index_fd)` **sparse-mmap BAR2
> transport** (a zero-copy optimization over the DMA-resident ring — only needed/exercisable under
> QEMU); and multi-segment scatter-gather backing (phase-1 takes the single-segment shortcut). The
> PR3 CPU-patch path stays the live fallback.

- `infinigpu-ring` becomes a device dep; `#[repr(C)]` on `Indices` + `Indices::from_ptr` so the
  loom-verified SPSC pop/retire/len runs over the shared page. New `index.rs`
  (memfd-backed `SharedIndexPage`, sparse-mmap'd via `build_regions(index_fd)`). New
  `resource.rs` (`HostResource`/`ScanoutBinding`/`CursorState`/`ResourceTable` with fail-closed
  caps: `MAX_RESOURCES=1024`, `MAX_BLOB_BYTES=64 MiB`, `MAX_DIM=16384`). `process_ring` rewritten
  as a **two-phase bounded drain** (pop a batch under the ring borrow into a local `ArrayVec`,
  release, then `execute_descriptor` under `&mut self`). Decode `RESOURCE_CREATE_BLOB /
  ATTACH_BACKING / SET_SCANOUT_BLOB / RESOURCE_FLUSH`, each IOVA validated via new
  `DmaTable::host_ptr`. Guest registers each dumb FB once and page-flips become
  `RESOURCE_FLUSH(res_id, damage)`. The PR3 CPU-patch path stays as pre-drainer fallback.
- **Biggest structural risk.** The `repr(C)` Indices viewer must be layout-exact vs
  `RingIndices` (pinned by asserts — validate on the target toolchain); the two-phase borrow
  split is the likeliest compile pitfall; descriptor slots in guest RAM are TOCTOU (harmless —
  `Descriptor` is Copy ints re-validated after copy); the QEMU vfio-user client must honor
  `FLAG_MMAP` + the sparse cap for a partial-BAR mmap.
- **Accept:** extend `tests/loopback.rs` — push N descriptors + bump tail + one doorbell →
  `head==tail`, `seqno_retired==N`; CREATE_BLOB→ATTACH_BACKING→SET_SCANOUT_BLOB→RESOURCE_FLUSH
  → presented BGRA matches inside the damage rect, unchanged outside; fail-closed on
  unmapped IOVA / oversized rect / unknown res_id; per-VM isolation (res_id 5 in A invisible to
  B); `cargo test -p infinigpu-ring` (loom) stays green after `repr(C)`.

### PR5 — Host-GPU convert on a per-VM worker thread inside `ticket.run()` *(M–L, ~2.5 wk)*
- `HostGpu::convert_present(key, meta, damage, read_guest, sink)` in `infinigpu-replay`:
  persistent per-scanout `ScanoutTarget` (staging + persistent out `VkImage` in `B8G8R8A8` so
  undamaged pixels survive), damage-rect `vkCmdCopyBufferToImage` + `vkCmdBlitImage`
  guest→BGRA convert, union-rect readback into a persistent `host_frame`, `sink → submit_bgra`.
  `scanouts: Mutex<HashMap<u64, ScanoutTarget>>` keyed per-VM.
- **Device seam (MANDATORY, lands WITH the GPU move):** per-VM latest-wins Mailbox on the
  callback thread + a per-VM worker thread running `self.ticket.run(|| convert_present(...))` —
  so the broker throttle never runs on the vfio-user thread.
  - *Status (seam built + tested, consumer hardware-gated):* the latest-wins hand-off is shipped —
    `mailbox.rs` (`Sender::post` never blocks + coalesces to the freshest frame + hands back the
    stale one; `Receiver::recv` blocks the worker; close/drop shutdown), 7 tests incl. the
    anti-bufferbloat "1000 posts → worker sees only the latest" and "recv blocks until post". This
    is the exact mechanism that keeps GPU work off the vfio-user callback thread (the `f14ad69`
    mouse-lag class). **Remaining (needs the A5000):** the per-VM worker thread wiring + the
    `convert_present` GPU body it drives.
- **Accept:** `convert_present_roundtrip` unit (known XRGB & RGBA rects → assert readback BGRA;
  a second small-damage present leaves the undamaged region byte-identical); E2E: `nvidia-smi
  dmon` GPU util rises on present while host CPU/present drops; colors correct; the callback
  thread only DMA-reads+enqueues (guest never freezes).

### PR6 — Hardware cursor: guest cursor plane + `CURSOR_UPDATE` + GPU composite *(M, ~2 wk)*
- Freeze the `CURSOR_UPDATE` (`0x0042`) body + `cursor_flags` (VISIBLE/MOVE_ONLY) + asserts.
- **Guest (the big structural change):** abandon `drm_simple_display_pipe` for an explicit
  primary + `DRM_PLANE_TYPE_CURSOR` + CRTC + encoder atomic pipeline
  (`drm_universal_plane_init`, `drm_plane_create_hotspot_properties`,
  `drm_plane_enable_fb_damage_clips`); cursor `atomic_update` emits `CURSOR_UPDATE`
  (MOVE_ONLY on pure motion). **Keep last among guest changes**; keep the DISPLAY_SCANOUT
  fallback + KMS selftest green. Kernel floor ≥6.6 for hotspot props (Ubuntu 6.14 fine; guard
  older DKMS).
- **Device:** decode `CURSOR_UPDATE` → per-VM `CursorState` → composite over the out image
  (blend pass reusing `render_triangle_inner`'s build); MOVE_ONLY = cheap re-composite + re-encode,
  **zero framebuffer bytes read**. Gated by `caps::HW_CURSOR`.
- **Accept:** `modetest` shows a CURSOR plane with hotspot props; moving the mouse in
  weston/Xorg emits `CURSOR_UPDATE` (MOVE_ONLY), does **not** flush the primary plane, cursor
  composited correctly with no trail; no-HW_CURSOR host → software cursor, still renders.

### PR7 — Zero-copy dma-buf → NVENC ingest *(L, ~2 wk; highest risk, ship last)*
- `export_dmabuf()` on `ScanoutTarget` (reuse the existing `VK_EXT_external_memory_dma_buf`
  export; out img **LINEAR** for the first cut). `PixelStreamer::submit_dmabuf(fd,w,h)` parallel
  to `submit_bgra`; ffmpeg reads a CUDA hw surface (`-hwaccel cuda`/`hwupload`) instead of
  rawvideo-on-stdin. Gated behind `HostGpu::can_export()`; `submit_bgra` always-available fallback.
- **Risk:** ffmpeg CUDA/Vulkan interop is finicky and the dev-stack container may lack the CUDA
  userspace (cf. the "ffmpeg-in-backend" note) — possibly blocked on a base-image change.
- **Accept:** `can_export()==true` → correct colors, readback bytes-to-host counter ~0;
  `false` → `submit_bgra` unchanged; measure glass-to-glass latency drop vs PR5.

### PR8 — Multitenant accounting: VRAM + NVENC-session admission *(S, ~0.5 wk)* — **SHIPPED**
- Parameterize `VRAM_ESTIMATE_MB` by the negotiated framebuffer size so each per-VM
  `ScanoutTarget` (~2–3× `w*h*4`) is counted in admission; count NVENC sessions at admission
  (GA102 `max_sessions=1`) so a second per-VM encoder is **denied-with-reason**, not a silent
  black stream.
- **Accept:** admit two 1080p GPU VMs → ledger reflects ~2× per-VM scanout VRAM; admission
  denies fail-closed when the estimate exceeds the reservation.
- *Status (shipped, unit-tested off-hardware):* `infinigpu-sched` gained `AdmitRequest`
  (`vram`/`streaming`, `From<u64>` so every pre-PR8 call site is untouched),
  `BrokerConfig::max_enc_sessions` (`None`=unlimited; env `INFINIGPU_MAX_ENC_SESSIONS`),
  `AdmitError::NoEncoderSession`, per-session accounting (reaped on ticket drop),
  `scanout_vram_estimate_mb(w,h,factor)`, and `adjust_vram`/`VmTicket::adjust_vram` (fail-closed
  post-attach true-up — a refused grow keeps the baseline, never black). The device claims a
  session when it streams (`ensure_admitted` → `streaming` iff a pixel port is bound) and trues up
  the VRAM reservation on the first present at each size (`account_scanout_vram`, once per size).
  5 sched tests cover the session cap (deny-with-reason + reap-frees-it), the unlimited default,
  the `From<u64>` no-session path, the fail-closed true-up, and the overflow-safe estimate. The
  only hardware-gated piece is the **E2E benchmark** (two live 1080p GPU VMs on the A5000 showing
  ~2× ledger VRAM) — the admission *logic* is complete and verified.

## Biggest risks
- **Blocking the vfio-user callback thread** (freezes the guest vCPU). The per-VM
  worker+Mailbox split (PR5) is mandatory and lands WITH the GPU move.
- **NVENC single-session cap** (GA102 = 1) surfaces as a silent second-VM black stream unless
  counted at admission (PR8).
- **Guest cursor-plane rework** (PR6) is the largest guest change and the easiest way to
  regress the boot/console path — keep it last, mirror vkms/tiny, keep selftest green.
- **dma-buf→NVENC** (PR7) may be blocked on the ffmpeg base image.

## Open questions
- Damage transport: additive `0x0102` (recommended) vs grow-in-place — one decision governs PR1.
- `CursorUpdate` wire layout (guest-phys addr vs res_id) — freeze in PR6.
- Retained-surface semantics for `RESOURCE_FLUSH`: the whole bandwidth win depends on the host
  keeping a persistent per-resource imported surface a flush updates only within the damage rect.
- Guest kernel floor: commit ≥6.6/6.14 only, or add `LINUX_VERSION_CODE` guards + software-cursor
  path for older DKMS targets?
- Isolation: keep the Phase-1 single shared `HostGpu` keyed per-VM, or move the scanout convert
  into the per-VM jailed replay process (`process.rs`) so a GPU fault takes down one VM, not the
  whole device? Design `convert_present` transport-agnostic so it can move behind the
  `ReplayProcess` socket without a rewrite.
- Multi-head: single CRTC today (`MAX_SCANOUTS=1`) — confirm no multi-head requirement.
