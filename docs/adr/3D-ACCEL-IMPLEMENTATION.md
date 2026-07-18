# 3D Acceleration — Implementation Plan

**Status:** Proposed (2026-07-18). Design synthesized from an adversarial multi-agent
review of the real tree (ABI, guest KMD, device, `infinigpu-replay`/`HostGpu`, verified host
GPU state). Companion to the [2D plan](2D-ACCEL-IMPLEMENTATION.md): 3D is **additive** and
composes on the exact same ring drainer / `ResourceTable` / fail-closed bounds / per-ctx
`seqno_retired`+MSI-X fence plumbing the 2D rungs build. Display stays intact throughout.

## Context

Today the guest rasterizes 3D on **llvmpipe (CPU)** — `glxgears`/`vkcube` run at ~10-20% of
native because `guest/linux/infinigpu.c` is **display-only** (`DRIVER_MODESET|ATOMIC|GEM`, no
`DRIVER_RENDER`, no `.ioctls`), so Mesa finds no render node and falls back to lavapipe/llvmpipe.
Three concrete gaps stand between here and real A5000 hardware 3D:

1. **Guest has no render node.** No `DRIVER_RENDER` ⇒ no `/dev/dri/renderD128` ⇒ Mesa can't bind
   a hardware ICD.
2. **Device executes only display.** `process_ring` (`crates/infinigpu-device/src/lib.rs`) renders
   `DISPLAY_CLEAR`/`DISPLAY_SCANOUT`; `encoding::VULKAN_VENUSLIKE` now has a **recognition arm**
   (`submit_vulkan`: admit + bound-check + retire the fence + count, no-op executor — Phase 1 PR1.4
   partial) but **no real decoder** yet. `BAR2_APERTURE_MB` reads 0 and `caps::BLOB_APERTURE` is
   unset (no host-visible aperture).
3. **No host 3D decoder.** `infinigpu-abi` is declared in `crates/infinigpu-replay/Cargo.toml` but
   referenced nowhere in `replay/src` — the Venus command stream has no consumer.

**The ABI was pre-shaped for exactly this.** `wire.rs` already layout-pins `CTX_CREATE (0x0010)`,
`RESOURCE_CREATE_BLOB (0x0020)` / `ATTACH_BACKING (0x0021)` / `MAP_BLOB (0x0022)`, `SUBMIT_CMD`
encoding `VULKAN_VENUSLIKE (1)`, and `capset::CAP_VULKAN`; `regs.rs` reserves a BAR2 blob aperture
+ `caps::BLOB_APERTURE`. Each piece maps 1:1 onto virtio-gpu's taxonomy — deliberately, so the
guest KMD translation is thin.

### The one strategy: remote **one** API (Vulkan), collapse everything else to it in-guest

Do **not** remote D3D or GL. In the guest, DXVK (D3D9/10/11→Vulkan) + vkd3d-proton (D3D12→Vulkan)
+ Zink (GL→Vulkan) translate every graphics API to **Vulkan**, which is remoted to the host as a
single **Venus** byte stream. The host therefore builds exactly **one** hostile-input decoder, not
five — a large security and effort win.

### VERIFIED host state — the whole premise is UNPROVEN until a spike says GO

The host NVIDIA proprietary driver is **550.163.01**, **below** Mesa's documented **570.86** Venus-
host floor. 550 predates NVIDIA's Venus host support (host-visible dma-buf import/export, DRM format
modifiers, external fence/semaphore fd, timeline semaphores, foreign-queue). **A spike on 550 is a
guaranteed false NO-GO.** Nothing in Phase 2+ is worth a line of code until the Phase-0 gate passes
on a pinned driver.

## Decisions

1. **Present the guest KMD as a `virtio_gpu`-uAPI-compatible render node** — Path A. Mesa's Venus ICD
   (`vn_renderer_virtgpu.c`) never talks to our device; it binds the **stable** kernel virtgpu DRM
   uAPI (`include/uapi/drm/virtgpu_drm.h`, frozen struct layouts = low ABI risk) and emits the Venus
   wire protocol. The **only** guest requirement is that our KMD present that ioctl surface and report
   `drmGetVersion()->name == "virtio_gpu"` (Mesa gates on that exact `strcmp`). The host underneath
   stays **100% ours**. Keep the KMD ioctl struct shapes byte-identical to `virtgpu_drm.h` so the
   own-name-vs-`virtio_gpu`-name choice is a one-line rename, not a redesign. **Reject** a bespoke
   in-guest ABI + custom Mesa ICD for Linux/Vulkan: a Vulkan ICD is the largest, most volatile piece
   of the stack (Venus is tens of thousands of codegen'd lines per `vk.xml` release) — writing our own
   forfeits it *and* still needs a host decoder. See the **Fallback** section for when this flips.

2. **The host decoder runs in the per-VM jailed replay process, never in-process.** virglrenderer's
   venus path has thread-unsafe global state (one instance per process) and the Venus stream is fully
   attacker-controlled (handles, offsets, descriptor indices, raw GPU VAs via
   `buffer_device_address`). It lands in a privileged, GPU-holding decoder — the CVE-2022-0175
   confused-deputy class. So the decoder lives in the ADR-0003 jailed replay **process** (blast
   radius = 1 VM), behind the same namespace+seccomp jail, and that jail **must ship before** the
   decoder does.

3. **`HostGpu` becomes import+present+capability, not the 3D executor.** On Path A, virglrenderer
   owns its own `VkInstance`/`VkDevice` created from the decoded guest `vkCreateDevice`. The existing
   `HostGpu` (headless Vulkan 1.3 on NVIDIA proprietary, `VK_EXT_external_memory_dma_buf` + working
   `get_memory_fd`) is repurposed to import the presented swapchain blob as a dma-buf and route it to
   `PixelStreamer::submit_bgra` (or, later, `submit_dmabuf`). Only the **bespoke fallback** drives 3D
   directly on `HostGpu`'s ash device.

4. **Fail-closed stream firewall, always.** Every guest handle/offset/size is bounds-checked against a
   per-VM `ResourceTracker` (mirrors `DmaTable::resolve`) before any host deref; `robustBufferAccess2`
   / `robustImageAccess2` / `nullDescriptor` are **forced** at device-create; a `vkCreateDevice`
   feature/extension allowlist rejects unbounded `bufferDeviceAddress`; `VK_ERROR_DEVICE_LOST`
   quarantines by **killing that one replay process** and marking the ring FATAL. Never Debug-print a
   payload.

5. **Windows gets the identical layering, deferred.** DXVK/vkd3d run in the Windows guest as ordinary
   DLLs on the same Vulkan ICD and emit the same Venus stream to the same host decoder. Ship the
   KMDF PCI companion (the missing piece behind the existing IddCx skeleton) now; **defer** the WDDM
   render miniport strictly behind Phase-0 GO **and** a shipped Linux decoder.

## Phase 0 — DE-RISK SPIKE (go/no-go gate) *(S–M, ~1–2 wk)*

**Run this first, before one line of decoder.** Use the **stock** virtio-gpu + virglrenderer-venus
stack as a measurement instrument — deliberately **not** our vfio-user device — to isolate the single
load-bearing question: *can a Venus command stream drive NVIDIA's closed Vulkan on THIS A5000?*

- **PR0.1 — HOST PREP (non-negotiable):** pin/upgrade NVIDIA proprietary `550.163.01 → ≥570.86`
  (fleet baseline `570.153.02` or a `575.x`); reboot; `nvidia-smi` confirms.
- **PR0.2 — `scripts/spike-venus-nvidia.sh`:** the **distro** `qemu-system-x86_64` (NOT
  `/opt/qemu-vfio-user`) with `-device virtio-gpu-gl,blob=true,venus=true,hostmem=4G` and
  `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/nvidia_icd.json` **on the QEMU process** (the crux — it
  makes virglrenderer's venus backend bind NVIDIA proprietary on the host). Guest = Ubuntu 25.04
  (virtio-gpu is already `DRIVER_RENDER`) + Mesa 25.0.7 venus ICD, `VN_DEBUG=init`.
- **PR0.3 — four-rung ladder + `docs/spikes/venus-nvidia-a5000.md` ledger:**
  1. `vulkaninfo` ⇒ `driverID=VK_DRIVER_ID_MESA_VENUS`, `deviceName='NVIDIA RTX A5000'`,
     `apiVersion≥1.3` (NOT llvmpipe).
  2. `vkcube` at vsync while host `nvidia-smi dmon` shows the qemu PID with non-zero GPU-Util
     (silicon, not llvmpipe).
  3. **THE CRUX:** a `HOST_VISIBLE|HOST_COHERENT` compute round-trip is byte-correct — exactly what
     DXVK/vkd3d staging buffers need and NVIDIA-Venus's historical weak point (host-visible dma-buf
     export).
  4. `wine`+DXVK `d3d11-triangle` with `DXVK_HUD=devinfo` showing the Venus device — de-risks the
     **whole** Windows D3D path on Linux with zero WDK work.

**GO/NO-GO:** all four pass ⇒ **GO**, authorize the host-decoder budget. Rung 1 or 3 fail on the
pinned driver ⇒ **NO-GO for Path A** → the **Fallback** section. Record the exact NVIDIA host-
extension set negotiated (feeds the driver-skew matrix).

## Phase 1 — Guest render node + host ring drainer *(M–L; ~1–2k LoC)*

Datapath proof with **no Venus/NVIDIA dependency** — provable on the A5000 with no QEMU-guest-Mesa
involved. (Can overlap Phase 0 in wall-clock; gate on GO if risk-averse, since it's wasted motion
only if the whole Venus approach NO-GOs.)

- **PR1.1 (guest KMD):** add `DRIVER_RENDER`; replace `DEFINE_DRM_GEM_DMA_FOPS` with
  `DEFINE_DRM_GEM_FOPS`; add `.ioctls[]` (virtgpu-shaped structs, `DRM_RENDER_ALLOW`), `.open`/
  `.postclose`, per-file state. **Keep the KMS/dumb/scanout path intact** (2D + display unaffected).
- **PR1.2 (guest KMD):** `CONTEXT_INIT` / `RESOURCE_CREATE_BLOB` (GUEST via `dma_alloc_coherent`
  phase-1 shortcut, single `ATTACH_BACKING` segment) / `MAP` / `EXECBUFFER` / `WAIT` handlers that
  translate to `infinigpu-abi` wire ops. Phase-1 register-based caps fallback (read `DEV_CAPS` /
  `BAR2_APERTURE_MB`, hardcoded Venus capset — no control-ring round-trips yet).
- **PR1.3 (ring):** `infinigpu-ring` `Indices` → `#[repr(C, align(64))]` layout-identical to
  `wire::RingIndices` + const size assert; `unsafe Ring::from_raw`; **loom model-check stays green.**
- **PR1.4 (device):** add `infinigpu-ring` dep; `DmaTable::host_ptr` (fail-closed); rewrite
  `process_ring` as a bounded drain (`MAX_DRAIN=capacity`) over a `Ring::from_raw` on guest RAM; add
  the `encoding::VULKAN_VENUSLIKE` arm forwarding the payload to a **v0 in-process executor** on
  `SharedGpu`. (This is the same real ring drainer the 2D plan's PR4 builds — do it once.)
  - *Status (partial, off-hardware):* the **recognition arm is landed** — `InfinigpuBackend::submit_vulkan`
    (device `lib.rs`) admits fail-closed, bound-checks the opaque payload against the 64 MiB geometry
    cap, retires the fence (no ring stall), and counts submits (`vulkan_submits`, unit-tested), so a
    guest render node drives the datapath with a no-op executor instead of the old *unsupported
    encoding* warn. The `infinigpu-ring` `#[repr(C,align(64))]` `Indices`/`from_ptr` (PR1.3) and
    `DmaTable::host_ptr` are already in place (2D-ADR PR4). **Remaining (needs QEMU + guest KMD
    PR1.1/1.2):** replace the single-descriptor read with the bounded `Ring::from_raw` two-phase drain,
    and swap the no-op body for the v0 `SharedGpu` executor.

**Accept:** `ls /dev/dri/renderD128` + `drm_info` shows `DRIVER_RENDER`; a minimal C test does
`GET_PARAM/CONTEXT_INIT/RESOURCE_CREATE_BLOB/EXECBUFFER(FENCE_OUT)/WAIT` with `retired≥seqno`; the
host logs the ring retiring the `SUBMIT_CMD`; the KMS scanout selftest still PASSes.
**Honest limit:** `DRIVER_RENDER` alone does **not** stop llvmpipe — Mesa still uses lavapipe until
Phase 2's uAPI-name + host decoder land. Phase 1 is a datapath proof, not user-visible acceleration.

## Phase 2 — Host Venus decoder + BAR2 aperture *(L, ~3–4 wk)*

Hardware shows up: adopt the virtio_gpu-compatible uAPI so **unmodified** Mesa Venus binds, wire
libvirglrenderer's venus path into the jailed replay process, stand up the real BAR2 `HOST_VISIBLE`
aperture + blob lifecycle.

- **PR2.1 (abi):** `wire.rs` add `RESOURCE_ATTACH_BACKING` body (`AttachBacking{res_id,num_entries}`
  + `MemEntry{addr,length}[]`); `lib.rs` layout asserts; `regs.rs` set `BAR2_SIZE` + OR
  `caps::BLOB_APERTURE` into the advertised caps.
- **PR2.2 (device):** `build_regions` creates the BAR2 memfd (`memfd_create` + `ftruncate` to
  `BAR2_APERTURE_MB`) as vfio-user region index 2 with `sparse_areas`+`mmap_fd`; `BAR2_APERTURE_MB`
  returns the real size; decode `RESOURCE_CREATE_BLOB`/`ATTACH_BACKING`/`MAP_BLOB` (`MAP_FIXED` into
  aperture)/`UNMAP`/`DESTROY`, all fail-closed bounds-checked.
- **PR2.3 (replay):** new `venus/` module — `virgl.rs` safe wrapper over libvirglrenderer
  (`virgl_renderer_init(VENUS|NO_VIRGL|USE_EXTERNAL_BLOB|THREAD_SYNC)`, `get_cap_set(VENUS=4)` as a
  build/runtime gate, `context_create_with_flags`, `submit_cmd`, `resource_create_blob`/`map`/
  `export_blob`, `context_create_fence`); `ffi.rs` + `build.rs` (pkg-config+bindgen,
  `feature=venus` default OFF). Expose `HostGpu` accessors + `alloc_blob_exportable`/
  `import_dmabuf_image`.
- **PR2.4 (replay):** `ResourceTracker` (per-VM `res_id` table, `DmaTable::resolve`-style fail-closed
  lookup) + `process.rs` Venus opcodes — **exactly one** `VirglVenus` per jailed process.
- **PR2.5 (device):** on admission spawn the jailed `ReplayProcess` for 3D; route the
  `VULKAN_VENUSLIKE` arm to it; guest KMD `GETPARAM` now advertises `HOST_VISIBLE` (gated on the
  **real** aperture) + Venus capset via `GET_CAPS`.

**Accept:** a Linux guest with **only** stock Mesa `libvulkan_virtio.so` (zero infinigpu-authored
userspace) → `vulkaninfo` reports `driverID=NVIDIA` / `deviceName 'NVIDIA RTX A5000'` with a
`HOST_VISIBLE` memory type — real hardware, not llvmpipe.

## Phase 3 — Fences + present into infiniPixel *(M, ~2 wk)*

Complete the fence bridge (`FenceBridge`: `VK_SEMAPHORE_TYPE_TIMELINE`; `SUBMIT_CMD.out_fence` →
`virgl_renderer_context_create_fence(ctx,_,ctx,S)`; the `write_context_fence` upcall runs **off** the
decode thread, does `retired.fetch_max(S)`, writes `RingIndices.seqno_retired` in the shared index
page, raises MSI-X `ctx+1`; `in_fence` → `wait_semaphores` before dispatch) and route the presented
swapchain image into infiniPixel. **First triangle / vkcube visible in the browser stream**; Zink→
Venus proves GL rides the same path.

## Phase 4 — Guest userspace stack (real D3D apps) *(S–M, ~1–2 wk, mostly off-the-shelf)*

Package the guest golden image: Mesa Venus + Zink + Wine 10 + DXVK 2.6 + vkd3d-proton 2.14, all
emitting into the **same** `libvulkan_virtio.so`. `WINEDLLOVERRIDES='d3d9,d3d11,d3d10core,d3d12,
dxgi=n,b'`; ship `/etc/infinigpu/dxvk.conf` + `VKD3D_CONFIG`; i386 multilib for 32-bit titles.
**Hard dependency:** vkd3d-proton needs `buffer_device_address(+captureReplay)`, `descriptor_indexing`,
`timeline_semaphore`, `robustness2`, `dynamic_rendering`, `synchronization2`, `mutable_descriptor_type`
— older Venus drops several (esp. BDA capture-replay), so gate on the `vulkaninfo` extension list and
pin **Mesa 25.x**. Phasing: D3D11-via-DXVK likely covers most office apps; treat full D3D12-via-vkd3d
as a sub-rung with a **WARP fallback** for the rare D3D12 app.

## Phase 5 — Hardening for N-VM scale *(L, multi-week)*

Graduate the replay process from `setrlimit`-only to a real per-VM **namespace+seccomp jail**; add
incremental VRAM admission + a watchdog→kill quarantine ladder; add a **completion-poller thread**
(dup the MSI-X eventfd) so the single-threaded `vfio_user 0.1.3 Server::run` doesn't serialize N VMs
(a synchronous 3D round-trip on the callback thread would freeze all contexts — the same "never block
the decode thread" rule the mouse-lag work enshrined). The seccomp allowlist is empirically fragile
and **driver-version-coupled** — build it via `strace -f` per pinned driver, CI-gated; `ioctl` can't
be filtered by request number (NVIDIA numbers are computed), so containment leans on the mount-ns
file restriction.

## Phase 6 — Windows *(KMDF companion S–M; WDDM miniport XL, deferred)*

Ship Windows office VDI with the KMDF PCI companion (direct port of `igpu_submit_scanout`) behind the
existing IddCx skeleton. **Stage but do not start** the WDDM render miniport until Phase-0 GO + the
Linux decoder ships — then fork `max8rr8/viogpu3d`, keep the VidPN + PnP/power buckets near-verbatim,
retarget only the ring-push onto our `SUBMIT_CMD{VULKAN_VENUSLIKE}` and synthesize
`DXGK_INTERRUPT_DMA_COMPLETED` from our retired-seqno model. DXVK/vkd3d run in-guest on M2's ICD — no
bespoke D3D UMD, no second host renderer.

## Fallback — our own Venus decoder (if Path A NO-GOs on NVIDIA)

Per the explicit requirement: **if virglrenderer-venus cannot drive NVIDIA's proprietary Vulkan on the
A5000, we implement our own.** The Phase-0 gate is precisely what forces this decision, cheaply, before
any decoder budget is spent. Two fallback shapes, in increasing ownership/cost:

- **Path B1 — own Venus-protocol decoder (`vn_protocol_renderer`-shaped), keep unmodified guest Mesa.**
  Reimplement virglrenderer's `vkr` Venus deserialization against `HostGpu`'s ash `VkDevice` (which
  *does* execute 3D in this path, unlike Path A). The guest stays 100% stock Mesa Venus — the
  "unmodified upstream guest" lever is preserved — but the host build is **large** (the Venus wire
  protocol is codegen'd from `vk.xml` and must be regenerated per release). This is the truest match to
  the 100%-ownership principle and keeps the guest identical to Path A; only the host consumer changes.
- **Path B2 — own thin guest ICD + hand-rolled opcode set.** If even accepting the Venus wire format is
  the blocker (e.g. a host-visible-memory semantic NVIDIA-Venus can't satisfy), abandon "unmodified
  Mesa" and ship our own minimal guest Vulkan ICD emitting a **small owned opcode set** (`BEGIN_CB /
  BIND_PIPELINE / DRAW / END_CB / SUBMIT / PRESENT_BLOB`) that the host replays directly on
  `HostGpu`'s ash device (reusing `render_triangle_inner`'s pipeline scaffolding). Largest scope, but
  fully owned end-to-end and decoupled from Mesa's release cadence. The Phase-1 v0 in-process executor
  is deliberately this shape, so Path B2 is a **superset of work already on the critical path** — the
  datapath (ring → replay → fence → present) is proven regardless of which decoder wins.
- **Or pivot host silicon.** If the A5000/GA102 specifically can't host Venus but the fleet can be
  AMD/Intel-first, Path A works unchanged there — a procurement decision, not an engineering one.

The `ResourceTracker`, `FenceBridge`, `StreamFirewall`, BAR2 aperture, guest render node, and ring
drainer are **identical across A and B** — only the decoder core differs. So Phases 0–1 and the
device/abi/guest scaffolding are **not** wasted if the gate flips; only the *choice of decoder core*
is deferred until first light.

## Biggest risks

- **The gate itself** (blocks everything): does virglrenderer-venus drive NVIDIA proprietary on the
  A5000? Venus is CI-validated mostly on Intel/AMD Mesa hosts, and 550.163.01 is below the 570.86
  floor. **The driver pin IS the spike.** Rung-3 host-visible dma-buf export is NVIDIA-Venus's
  historical weak point.
- **No-MIG Xid blast radius:** the GA102 A5000 has no MIG, so a severe Xid (full-reset class) resets
  the **whole** card and downs every tenant. The jailed per-VM process bounds *process* blast radius,
  **not** device-reset scope. Irreducible on this card — mitigate with driver pinning, forced
  `robustness2`, watchdog→kill quarantine, monitoring; document as an appliance-level residual.
- **Host-driver skew:** Venus "relies on implementation-defined behavior" and pins host driver
  versions; guest Mesa venus, the negotiated capset `wire_format_version`, and the host driver must
  stay in lockstep across the fleet — pin 2–3 baselines + a compat matrix + a host↔guest re-handshake;
  the seccomp allowlist is itself driver-version-coupled.
- **Hostile guest stream:** the `VULKAN_VENUSLIKE` payload is fully attacker-controlled and lands in a
  privileged GPU-holding decoder. Reusing libvirglrenderer imports its venus attack surface into the
  trust boundary — the jail must ship **before** the decoder.
- **HOST_VISIBLE / BAR2 aperture is net-new device work,** not config: Venus/DXVK/vkd3d all *require*
  mappable HOST3D blobs but `BAR2_APERTURE_MB` reads 0 today, and whether a `MAP_FIXED`'d HOST3D blob
  dma-buf inside a vfio-user sparse-mmap region actually reaches the guest is untested on
  QEMU 10.1.1 + `vfio_user 0.1.3`.
- **Threading:** `Server::run` is single-threaded — a synchronous 3D round-trip on the callback thread
  freezes all contexts. Phase 5's out-of-band completion-poller is mandatory at scale.
- **vkd3d-proton extension surface** (above) — pin Mesa 25.x, gate on `vulkaninfo`, WARP fallback.

## Open questions

- **Guest uAPI name:** report `drmGetVersion` name literally `"virtio_gpu"` (spoof, zero in-guest ICD,
  but cannot coexist with a real virtio-gpu) vs a one-line-patched `libvulkan_virtio.so` accepting
  `"infinigpu"` (Mesa fork/maintenance cost)? **Recommend** the `virtio_gpu` spoof; confirm the exact
  `strcmp` + accepted `version_major/minor` tuple in the pinned Mesa `vn_renderer_virtgpu.c`.
- **Host decoder ownership:** reuse libvirglrenderer's venus path (fast, proven, imports its attack
  surface + a licensing/ownership question) vs hand-roll an owned `vn_protocol_renderer` (matches 100%
  ownership, large build)? The spike proves virgl works; ownership can be revisited after first light —
  see **Fallback**.
- **GEM backing:** migrate to `drm_gem_shmem` + `ATTACH_BACKING` sg-lists now vs ship phase-1
  contiguous dma-coherent blobs and defer? Phase 1 takes the contiguous shortcut; Phase 2+ needs
  sg-lists for large Venus streams.
- **Fence model:** raw `seqno` poll for first light vs straight to `dma_fence`+`drm_syncobj`+
  `sync_file`; and legacy `write_fence` vs per-context `write_context_fence` (matches per-context
  `seqno_retired`)? **Recommend** the context-fence path.
- **Present zero-copy vs CPU bounce:** can `infinigpu-pixel`'s NVENC encoder ingest the exported
  dma-buf directly (→ 2D-plan PR7) or must v1 keep the `resource_map → BGRA → submit_bgra` bounce? The
  bounce is the safe first cut.
- **Office VDI D3D depth:** do real apps hit D3D12 enough to need vkd3d-proton in phase 1, or is
  DXVK-D3D11 (+ WARP) sufficient — deferring the harder vkd3d-on-Venus work?
- **Per-VM uid:** does `infinization` allocate a dedicated unprivileged uid per VM at spawn (alongside
  TAP+nftables) for the jailed replay, or does the replay derive it?

## Relationship to the 2D plan

3D reuses, does not duplicate: the **real ring drainer + `ResourceTable` + fence-retire** are 2D-plan
PR4 (built once, consumed by both); the **per-VM worker + Mailbox** off the vfio-user thread is
2D-plan PR5's device seam; **dma-buf → NVENC** present is 2D-plan PR7. Land the 2D rungs first — they
de-risk the shared infrastructure on a simpler (display-only) payload before the hostile Venus stream
arrives.
