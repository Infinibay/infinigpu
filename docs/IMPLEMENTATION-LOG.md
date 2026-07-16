# infinigpu — Implementation log

> The running record of the design→code transition. Design corpus is in `docs/`
> (research/, decisions/, ROADMAP, PHASE-0-PROTOTYPE). This file tracks what is
> actually built and what the code taught us that the docs didn't know.

## 2026-07-16 — Kickoff: foundation crates + host GPU datapath proven

### What exists now (Rust workspace)

```
Cargo.toml                     # workspace: abi, ring, replay
crates/infinigpu-abi/          # no_std, no-alloc wire ABI (zerocopy)   — DONE, tested
crates/infinigpu-ring/         # no_std SPSC ring + seqno               — DONE, tested + loom-verified
crates/infinigpu-replay/       # host Vulkan backend (ash)              — renders on the A5000
scripts/build-qemu-vfio-user.sh
```

- **`infinigpu-abi`** — PCI identity, BAR0 register map, and all Phase-0 wire
  structs as `#[repr(C)]` + zerocopy `FromBytes/IntoBytes/Immutable/KnownLayout`,
  with compile-time layout assertions. `#![forbid(unsafe_code)]` (zerocopy's derives
  are compatible). 7 tests green.
- **`infinigpu-ring`** — SPSC descriptor ring viewed over caller memory, seqno
  completion, `Release`/`Acquire` publish protocol. 5 unit/stress tests green **plus
  a `loom` model check** (`RUSTFLAGS="--cfg loom" cargo test --test loom_ring`) that
  exhaustively proves lossless, race-free ordering — the ADR-0004 requirement.
- **`infinigpu-device`** — the vfio-user PCI device `ServerBackend` (config space,
  BAR0 control registers, an mmap'd IOVA→HVA DMA table with fail-closed bounds checks,
  MSI-X). Validated **without QEMU** by `tests/loopback.rs`, which drives it with the
  real `vfio_user` `Client` (the same protocol QEMU speaks) and proves: PCI identity +
  display class, BAR0 `MAGIC`/`ABI`/`CAPS`/`GLOBAL_CTRL`, **zero-copy DMA read+write
  through a shared memfd**, and **MSI-X delivery** via eventfds. 1 integration test green.
- **`infinigpu-replay`** — headless Vulkan on the physical GPU via `ash` (prefers the
  NVIDIA proprietary driver → Vulkan for free, no vGPU license). `HostGpu::render_clear`
  runs a real graphics render pass and DMA-reads the result back. The smoke binary
  verified pixel-exact readback on an **RTX A5000** (render ~10 ms). This closes the
  **GPU-facing half of the Phase-0 loop** with no QEMU involved.

### Two ground-truth findings that changed the design (verified against source)

1. **The `vfio-user` Rust crate (v0.1.3) has NO ioeventfd doorbells.**
   `GET_REGION_IO_FDS` is hard-rejected, so a BAR write is always a synchronous
   socket round-trip — the "doorbell = eventfd" hot path in research/24 is not
   available with the stock crate. This is exactly the ADR-0001 fallback (ERRATA #5),
   now **confirmed mandatory**. Resolution baked into the ABI: the device advertises
   `caps::POLL_SUBMIT` (host polls the sparse-mmap'd shared index page SQPOLL-style;
   the trapped doorbell only *wakes* an idle poller) and does **not** advertise
   `IOEVENTFD_DOORBELL`. Zero-copy guest RAM (memfd via `dma_map`) and MSI-X (hand-
   rolled cap, per-vector eventfds) both work as designed.
2. **`vfio-user-pci` is upstream in QEMU since 10.1** (no oracle fork). Build ≥ 10.1.1
   to a private prefix via `scripts/build-qemu-vfio-user.sh`. Property is `socket=`
   (SocketAddress); `share=on` on the RAM backend is mandatory for DMA; there is no
   live-migration knob (savevm fails cleanly — acceptable).

### 2026-07-16 (later) — real QEMU integration verified

Built QEMU **10.1.5** with the upstream `vfio-user-pci` client into `/opt/qemu-vfio-user`
(via `scripts/build-qemu-vfio-user.sh`). `scripts/smoke-qemu-device.sh` boots it headless
with our `infinigpu-device` attached and **no guest OS**, and the device server log proves
the seam works against the *actual* QEMU vfio-user client (not just the loopback):

- `config read @0x00 (PCI enumeration): 0x1b36:0x0110` — SeaBIOS enumerated our device;
- `DMA_MAP iova=0x0 size=0x40000000 (guest RAM mapped zero-copy)` — QEMU shared the full
  1 GB guest-RAM memfd into our device process.

Device fix from this run: a `DMA_MAP` **without** a shared fd (BIOS/ROM shadow, MMIO holes)
is now a silent no-op (the region simply stays unmapped → guest DMA into it fails closed),
instead of erroring. Notes: `socket` is a `SocketAddress` union so the **JSON `-device`
form is required** (flat `socket.type=` is rejected); `x-pci-class-code` takes a number
(`229376` = `0x038000`). Both recorded in the smoke script.

### 2026-07-16 (later still) — full host pipeline fused end-to-end

`infinigpu-device` now depends on `infinigpu-replay`, and a doorbell write runs a
**submit engine**: it decodes the `SUBMIT_CMD` at the command-ring base from guest RAM
(via the DMA table + zerocopy), and for a Phase-0 `DISPLAY_CLEAR` payload renders on the
GPU and DMA-writes the frame back to the guest scanout address, raising the completion
MSI-X. The `infinigpu-pipeline-demo` binary drives the real backend in-process and verifies
the whole chain on the **A5000**:

```
guest rings command-ring-0 doorbell (submits DISPLAY_CLEAR)…
replay GPU: NVIDIA RTX A5000 (NVIDIA_PROPRIETARY)
seqno 1: rendered 256x256 on the GPU → scanout 0x80100000
completion MSI-X fired: true
scanout[0,0] in guest RAM = [0, 153, 204, 255]  (expected [0, 153, 204, 255])
OK — the guest's ring submission rendered on the GPU and the frame was DMA-written back
```

This fuses **abi (wire format) + device (DMA/decode/MSI-X) + replay (physical GPU)** into one
working datapath — the entire Phase-0 host side, minus the guest OS. What remains for a true
guest→GPU loop is the guest driver.

### 2026-07-16 (guest side) — real guest kernel enumerates the device; driver built

- **Guest enumeration verified.** `scripts/guest-enumerate.sh` direct-kernel-boots a real
  Linux kernel under our QEMU with the device attached (host kernel + a busybox initramfs,
  no distro image needed) and the guest kernel reports
  `0000:00:03.0 vendor=0x1b36 device=0x0110 class=0x038000` — our device, correct display
  class, on the guest PCI bus.
- **Guest driver written + compiled.** `guest/linux/infinigpu.c` (+ `Makefile`) is a plain
  PCI driver that binds `1b36:0110` and runs an in-kernel **self-test** in `probe()`: map
  BAR0, check `DEV_MAGIC`, build a one-entry command ring in coherent DMA memory, submit a
  `DISPLAY_CLEAR`, and verify the host rendered it on the GPU and DMA-wrote the frame back.
  Builds cleanly to `infinigpu.ko` against the 6.14 headers. Added a pollable
  `CMD_RING0_RETIRED` register so this first test syncs without needing MSI-X in the guest.
  `scripts/guest-driver-test.sh` boots it and checks `dmesg` for `SELFTEST: PASS` — ready to
  run; needs a readable copy of the matching host kernel (one `sudo install` — see the script).
- **cbindgen ABI header (Step 2 tail).** `scripts/gen-abi-header.sh` regenerates
  `guest/include/infinigpu_abi.h` (the wire structs) from `infinigpu-abi` and compiles
  `guest/include/abi_conformance.c`, whose `_Static_assert`s pin the C layout to the Rust
  ABI — the cross-language drift guard (mirrors infiniservice's HMAC cross-lang test).

### 2026-07-16 — 🎯 FULL GUEST→GPU LOOP CLOSED (Phase-0 objective met)

`scripts/guest-driver-test.sh` boots a real Linux guest, loads `infinigpu.ko`, and its
in-kernel self-test passes end-to-end:

```
[guest] infinigpu 0000:00:03.0: magic=0x49475055 abi=0x1 caps=0x1c
[host]  replay GPU: NVIDIA RTX A5000 (NVIDIA_PROPRIETARY)
[host]  seqno 1: rendered 256x256 on the GPU → scanout 0x28c0000
[guest] INFINIGPU-SELFTEST: PASS retired=1 scanout[0]=[0,153,204,255]
```

A real guest-kernel driver submits a command through our device; the host decodes it,
renders on the **physical A5000**, DMA-writes the frame back into guest RAM, and the guest
verifies the pixels — the whole point of the project, working through our own stack.

**Load-bearing fix — `x-no-posted-writes=true` is mandatory.** Without it the guest's BAR
MMIO writes desync the protocol (QEMU "unexpected reply"/"bad header size" → read timeout →
broken pipe): QEMU posts MMIO writes by default (no reply expected) but the `vfio_user`
v0.1.3 server always replies to REGION_WRITE. Enumeration (SeaBIOS) doesn't hit it; a guest
driver does immediately. **`infinization`'s `QemuCommandBuilder.addInfinigpuDevice()` must
include `x-no-posted-writes`** (until the crate honors the posted-write flag). Recorded in
`ERRATA`-style project memory.

### 2026-07-16 — 🖥️ REAL DRM/KMS DISPLAY DRIVER (guest shows a true framebuffer)

`guest/linux/infinigpu.c` is now a real **DRM/KMS display driver**, not the plain-PCI
self-test. It registers `/dev/dri/card0` with one CRTC/plane/encoder/connector, uses the
**GEM-DMA helpers** for contiguous dumb framebuffers, and enables **fbdev emulation** so
the kernel's **fbcon binds to our framebuffer**. Every page-flip hands the host the
framebuffer's guest-physical `dma_addr` via a new `DISPLAY_SCANOUT` ring command; the host
reads the pixels and presents them (`present_scanout` in `infinigpu-device`), dumping each
frame as a PPM (`INFINIGPU_PRESENT_DIR`) so the guest's console is **viewable host-side**.

`scripts/guest-kms-test.sh` boots a real guest and proves it end to end:

```
[guest] [drm] Initialized infinigpu 1.0.0 for 0000:00:02.0 on minor 0
[guest] Console: switching to colour frame buffer device 128x48   ← fbcon on OUR fb
[guest] [drm] fb0: infinigpudrmfb frame buffer device  →  /dev/dri/card0 + /dev/fb0
[guest] INFINIGPU-KMS: PASS pipe present retired=1 seqno=1
[host]  present: frame 1 128x128 …  16384 non-blank px (100.0%)   ← KMS self-test gradient
[host]  present: frame 5 1024x768 … 30862 non-blank px (3.9%)     ← real fbcon CONSOLE TEXT
```

`/tmp/infinigpu-frames/latest.png` shows the **live guest kernel console** — including our
own driver's boot lines — rendered by fbcon onto our framebuffer and scanned out through the
vfio-user device to the host. This is PHASE-0 Step 3 (Linux DRM/KMS) + a pure-2D Step 6
(present), done for real.

**Design notes / findings:**
- **Contiguous (GEM-DMA) framebuffers were the right call.** Each buffer is one `dma_addr`
  the host reads as a single blob — no scatter-gather. Cost: exactly one extra guest module,
  `drm_dma_helper.ko` (`CONFIG_DRM_GEM_DMA_HELPER=m` on Ubuntu; everything else in the DRM
  stack — core, KMS helper, fbdev, GEM-shmem — is `=y`). The harness decompresses the host's
  `.ko.zst` and `insmod`s it before ours; `modinfo -F depends infinigpu.ko` = just
  `drm_dma_helper`.
- **6.14 fbdev API**: `drm_fbdev_dma_setup()` is gone. Use `DRM_FBDEV_DMA_DRIVER_OPS`
  (`.fbdev_probe`) in the driver + `drm_client_setup(dev, NULL)` after `drm_dev_register`
  (`<drm/clients/drm_client_setup.h>`). `struct drm_driver.date` was also removed.
- **The doorbell round-trip is the flip sync.** Because the host processes the ring *inside*
  the (non-posted) doorbell `region_write` before replying, the guest's `iowrite32(doorbell)`
  returning already means "presented" — no vblank IRQ needed; the pipe completes flip events
  immediately with `drm_crtc_send_vblank_event`.
- **`-vga none` is required** in the test so infinigpu is the *only* DRM device and fbcon
  binds to `fb0` = ours (not a default QEMU VGA). `console=tty0 console=ttyS0` routes the
  kernel log to both our framebuffer and the serial capture.
- The KMS self-test presents a recognizable gradient *before* `drm_client_setup`, so its
  deterministic PASS can't race a concurrent fbcon flush.

### 2026-07-16 — 🧠 THE GPU BROKER: two VMs share one A5000 (ADR-0007, Phase-1)

`infinigpu-sched` is the VDI capacity manager + scheduler "brain" — the *differentiator*
(share one GPU across many desktops, no MPS, no per-VM license). Built in the ADR-0007
order (accounting → admission → fair-share), **GPU-agnostic** so it unit-tests with no GPU:

- **Fail-closed admission** at GPU-attach: a broker-owned **VRAM commit ledger** + a
  concurrent-GPU-VM cap + a per-VM VRAM cap. Over-capacity is denied (typed `AdmitError`),
  never best-effort. An RAII **`VmTicket`** releases the VRAM + slot on drop — the explicit
  reap on stop *or* panic-unwind.
- **GPU-time accounting + token bucket**: every render is measured and debited from a per-VM
  bucket whose refill rate ∝ `gpuTimeWeight` × priority boost. The bucket is the **hard QoS
  backstop** (ADR-0007: `VK_EXT_global_priority` is only a soft MEDIUM/LOW hint on NVIDIA).
- **Weighted fair-share**: a hog that empties its bucket is throttled (blocked) until it
  refills. A per-VM **anti-starvation floor** keeps the lightest desktop moving; a **watchdog**
  flags a render that overruns its budget (real design kills the per-VM *process*, ADR-0003).
- 11 deterministic unit tests (`ManualClock`, no GPU): admission fail-closed on all three
  caps, reap-frees-capacity, the **weighted-share law verified at 3×**, the interactive boost
  at 1.5×, anti-starvation, watchdog, and real-clock run-serialization.

**Wired into the device** (`infinigpu-device`): a shared `GpuBroker` + one `SharedGpu`
(single Vulkan context, serialized by the broker's run-lock — cooperative, never MPS).
`InfinigpuBackend` admits at `GLOBAL_CTRL` enable and routes every GPU render through
`ticket.run(...)`; `reset_state`/backend-drop reaps. `serve_with_broker()` lets one host
process serve many VMs off one broker.

**Proven on the real A5000** — `infinigpu-broker-demo` (no QEMU):

```
Act 1 admission: designer-3 DENIED (VRAM ledger) · greedy DENIED (per-VM cap) ·
                 office-2 DENIED (concurrency cap) — every over-capacity request fails closed
Act 2 fair-share: designer (w3) got 1.95× the office (w1×1.5 Interactive) desktop's GPU-time
                  — matching the 2.0× effective-weight target — on ONE physical A5000.
```

That is two VM desktops sharing one physical GPU with capacity-aware weighted fair-share and
no per-VM license — the VDI thesis, working.

**Faithful-but-simplified (called out, not hidden):** (1) one shared Vulkan context in one
process rather than ADR-0003's per-VM jailed *process* (so this validates the scheduling brain,
not yet the isolation/NVML-attribution story); (2) GPU-time = wall-clock of the serialized
render, a proxy for the authoritative Vulkan-timestamp currency; (3) the token bucket enforces
weighted shares in the token-limited regime — under *full* GPU saturation with generous tokens,
vruntime-ordered dispatch (tracked, not yet wired) is what makes shares weight-proportional.
These do not change the admission/fair-share math proven here. Remaining Phase-1: multi-ring
scale-out, per-VM replay process + NVML attribution, infiniPixel remote protocol.

**Adversarially verified** (`verify-scheduler` workflow — 4 diverse-lens critics × per-finding
verification: 10 raised, 3 confirmed + fixed):
- **The load-bearing one:** a render panic could poison the shared GPU run-lock (`.unwrap()`) and
  brick submission for **every** VM — a host-wide outage from one bad guest. Now **contained** by
  `catch_unwind` + poison-tolerant locks (`unwrap_or_else(|e| e.into_inner())`), and its
  guest-reachable trigger — unvalidated `width×height` → u32 overflow in the render path — is
  closed by geometry validation (mirroring `present_scanout`) + u64 arithmetic. Regression-tested
  (`a_panicking_render_is_contained_and_does_not_brick_the_fleet`).
- **vruntime consistency:** the fair-share yardstick now divides by *effective* weight (weight ×
  boost), matching the share the token bucket enforces (was raw weight) — via a single
  `VmConfig::effective_weight_num()` used by refill, throttle, and vruntime alike.
- **Startup fairness:** the one-time Vulkan context open is pre-warmed *outside* the broker's
  timed region, so the first tenant isn't billed the init and over-throttled.

### 2026-07-16 — 📡 infiniPixel v0: the owned remote-display protocol (ADR-0009, Phase-1)

`infinigpu-pixel` is the first cut of **infiniPixel** — the owned low-latency datapath that
replaces SPICE's GPU display path. A host framebuffer is encoded on the GPU's **dedicated NVENC
block**, wrapped in an **owned frame protocol**, streamed over WebSocket, and decoded in the
browser with **WebCodecs**. We control all three ends; SPICE (readback → CPU-encode → TCP →
native viewer) can't.

- **Encode:** H.264 (the ADR's universal fallback; broadest WebCodecs support) on `h264_nvenc`
  — the A5000's encode engine, separate from the 3D SMs (the ADR-0007 density story). Low-latency
  config (`-tune ull`, no B-frames, CBR); `libx264` software fallback via `--sw`. Driven through
  `ffmpeg` for v0 — a codec *backend* (ADR-0008 vendor HAL), not the protocol; a native
  NVENC/Vulkan-Video FFI backend comes later.
- **Framing:** `FrameHeader` — an owned 32-byte little-endian header per access unit
  (magic/version/flags/codec/seq/w/h/pts/len), mirrored byte-for-byte by the JS client. An
  Annex-B AU splitter (AUD-delimited via `h264_metadata=aud=insert`) turns the encoder's NAL
  stream into one clean chunk per frame; keyframes are flagged so a client can start decoding.
- **Transport:** a pure-Rust WebSocket server (`tungstenite`) — the ADR's *mandatory
  browser-reachable fallback* rung. A `Hub` fans out to all clients and **primes each new client
  with the last keyframe** so its decoder starts immediately. (WebTransport/QUIC + datagrams/FEC
  is the v1 target.)
- **Client:** `client/infinipixel.html` — WebCodecs `VideoDecoder` (`optimizeForLatency`,
  prefer-hardware) → canvas, building the codec string from the SPS, with a live fps/kbps HUD.

**Validated end to end, headless** (`scripts/infinipixel-test.sh`, no browser): the demo streams
an animated pattern; a Node client (Node 22 global `WebSocket`) parses the infiniPixel protocol,
collects 60 access units (`keyframes=2, dims 960×540`), and **ffmpeg decodes all 60 frames** from
the collected stream — proving encode + own-protocol framing + WebSocket transport + a real client
parse + valid decodable H.264. `/tmp/infinipixel-frame.png` shows the recovered animated frame.
Plus 3 unit tests (header round-trip, AU-splitting incl. byte-at-a-time reads). The **browser
display itself** is unverified-in-browser here (no display), but the exact wire path the browser
uses is proven by the headless client.

**Deferred to v1 (all in the ADR, none change this datapath):** damage-aware hybrid (idle ⇒ ~0
bits — the big density win), intra-refresh/GDR (v0 uses periodic IDR for simple client start-up),
the perceptual/foveation layer, HEVC/AV1 negotiation, WebTransport/QUIC, adaptive control, local
cursor sprite, and wiring the encoder input to the live device present path (v0 uses a test
pattern; the frames already exist in `present_scanout`).

### 2026-07-16 — 🔗 device present → infiniPixel: a live guest desktop in a browser

The two Phase-1 halves are now one path. `infinigpu-device` depends on `infinigpu-pixel`;
when `INFINIGPU_PIXEL_PORT` is set, the backend binds a `PixelStreamer` (WebSocket server up
front, encoder created lazily) and feeds every **presented framebuffer** to it — so a real
guest's DRM/KMS console is NVENC-encoded and streamed to a browser, decoded with WebCodecs.

`scripts/guest-kms-pixel-test.sh` proves the whole stack end to end: boot a real Linux guest
→ its console renders through `infinigpu.ko` → the host device presents each framebuffer →
NVENC encodes it → infiniPixel streams it → a client decodes it. `/tmp/infinipixel-guest.png`
shows the **live guest console** (boot log, `INFINIGPU-KMS: PASS`, and the injected
`infiniPixel over DRM/KMS … the quick brown fox` line) recovered from the stream.

Two real fixes this milestone taught us:
- **`PixelStreamer` re-creates its encoder on a resolution change** (keeping the WS server
  bound), because the guest presents the 128×128 KMS self-test *then* the 1024×768 console —
  a fixed-size encoder would drop every real frame. This also handles live guest resolution
  changes.
- **The DRM driver needed `drm_gem_fb_create_with_dirty`** (not `drm_gem_fb_create`). A
  directly-scanned-out DMA framebuffer only presents on the boot modeset; fbcon's post-boot
  console writes go straight to the buffer with nothing telling the device to present. Wiring
  `drm_atomic_helper_dirtyfb` makes damage trigger a commit → a present. Effect: presents went
  from **5 (boot only) → 104 (continuous)**, and the live desktop actually updates. (The
  full-frame flush per damage is CPU-heavy — `drm_fb_helper_damage_work hogged CPU` — which is
  what the damage-aware idle-skip milestone optimizes.)

Known v0 limits (documented): the encoder holds the newest frame until the next one (fine for
a live stream, shows as a 1-frame lag on a frozen scene); one `INFINIGPU_PIXEL_PORT` per
process (per-VM ports come with the multi-VM streaming refinement).

### 2026-07-16 — 💤 infiniPixel damage-aware idle-skip (idle ⇒ ~0 bits)

The ADR-0009 common-case density win: `PixelStreamer` hashes each frame (fast FNV-1a over
64-bit words) and **skips encoding+sending a frame identical to the previous one**. A static
desktop presents identical framebuffers → they hash equal → ~0 encode, ~0 bytes. This is the
v0 proxy for the guest damage map (a real damage-rect path is v1). The guest test shows it
live (`56 encoded, N idle-skipped` — skip ratio approaches 100% as the desktop goes idle);
unit-tested by `idle_skip_drops_unchanged_frames_only`.

### 2026-07-16 — 🔌 vendor HAL: capability traits (ADR-0008)

`infinigpu-hal` — a **pure** trait crate (no `ash`/`ffmpeg`) that keeps the NVIDIA-specific
render + encode as *backends*, not the architecture. The stack selects by **capability**,
never by vendor name, so AMD (RADV Vulkan + VA-API/Vulkan-Video) or Intel is a new backend,
not a rewrite:
- `GpuBackend` → `GpuCaps { vendor, device/driver, vulkan_render, timestamp_queries,
  external_memory, global_priority }`. Implemented by `HostGpu` (maps `vk::DriverId` →
  `Vendor`). Live probe on the A5000: `NVIDIA — NVIDIA RTX A5000; render=true timestamp-qos=true
  dma-buf=true global-priority=true`.
- `MediaEncoder` → `CodecCaps { vendor, hardware, encode:[…], low_latency, max_sessions }`.
  Implemented by the infiniPixel `Encoder` (NVENC vs libx264; `max_sessions = Some(1)` encodes
  GA102's single NVENC block as the scarce ADR-0007 admission resource; GA102 can't AV1-encode).

The broker-demo prints the GPU HAL caps; unit-tested that consumers query by capability, not
vendor. This is the "vendors are backends" scaffold ADR-0008 wants from day one.

### Immediate next steps

- **Step 1 (device):** write the `infinigpu-device` vfio-user `ServerBackend` against
  v0.1.3 (config space, BAR0 regs, sparse-mmap index page, `dma_map` interval table,
  MSI-X). Testable **without QEMU** first via an in-process `Client`↔`Server` loopback,
  then against the real QEMU once built.
- **Step 5+ (replay):** add a shader triangle (SM execution) and export the rendered
  blob as a **dma-buf**; then wire the ring decoder so a `SUBMIT_CMD` payload drives it.
- **Step 3 (guest):** minimal C DRM/KMS driver, tested in a Fedora/Ubuntu Infinibay VM.
