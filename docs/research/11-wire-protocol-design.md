# 11 — OS-neutral wire protocol + command ring (shared no_std Rust crate)

**Scope:** design the on-the-wire protocol and command ring that serves **both** the Linux
guest (DRM/Vulkan) and the Windows guest (IddCx/WDDM/D3D), is implementable as one
`no_std`-clean shared Rust crate, and is versioned for decades of evolution. This builds on
Wave-1 docs 01 (vfio-user device seam) and 06 (API-remoting data plane) and deliberately
tries to **refute** the "one protocol serves both OSes" assumption.

## Verdict up front

**PARTIALLY-CONFIRMED.** A single OS-neutral protocol is real and buildable **at the
framing/resource/fence/present layer** — virtio-gpu already proves this exact envelope is
API- and OS-agnostic, and `zerocopy`+`postcard` make it cleanly `no_std`. But "the *same*
protocol carries both a Vulkan payload and a D3D payload" only holds if you accept that the
**command payload is an opaque, per-API sub-protocol** — the Linux Vulkan encoder and the
Windows D3D/DDI encoder share **zero** marshalling code. And the Windows arm is *unproven*:
Microsoft's GPU-PV marshals the WDDM DDI over VMBus, never over a KVM ring, so nobody has
shipped our Windows payload. Net: the **envelope is OS-neutral (confirmed); the payloads are
not shared (by design); the Windows payload is the standing risk.**

## 1. Layering — protocol sits *above* the transport

The single most important design decision: **the ring protocol is independent of the host
device seam.** Doc 01 chose vfio-user; doc 06 kept a virtio-gpu-style device as an
alternative. The wire protocol must not care. Both seams offer the same three primitives we
need, so we abstract exactly those:

| Primitive | vfio-user | virtio-style device |
|---|---|---|
| Shared ring memory | sparse **mmap-able BAR region** (fd passed by server) | virtqueue pages / `hostmem` BAR |
| Guest→host kick | **ioeventfd** on a doorbell sub-region | virtqueue notify |
| Host→guest signal | **MSI-X eventfd** | used-ring interrupt |

vfio-user explicitly supports "direct access to doorbells … while trapping accesses to
registers," plus `ioeventfd`/`ioregionfd` for hot sub-regions — precisely our doorbell/fast
path ([vfio-user spec](https://www.qemu.org/docs/master/interop/vfio-user.html)). So the
crate defines a `Transport` trait (`ring_memory() -> &mut [u8]`, `ring_doorbell(ring_id)`,
`completion_signal()`), implemented once per seam. Everything else in this doc is
seam-neutral.

## 2. Ring topology — one control ring + per-context command rings

Wave-1 doc 06 recommended "one command ring" for the MVP. **I partially refute that as an
ABI choice.** gfxstream's headline scalability fix over VirGL was moving from a single host
decode thread to a **1:1 thread model** — one guest encoder stream to one host decoder
thread — because a single ring serializes every guest's (and every app's) work behind the
slowest command ([gfxstream README](https://android.googlesource.com/platform/hardware/google/gfxstream/+/fbc9e43e236777dacf23c0d4bf71dc414df984a9/README.md)).
Venus similarly uses a per-context `vn_ring` for asynchronous transmission
([Venus deepwiki](https://deepwiki.com/arehnman/virtio-win-mesa/7.7-virtualized-and-layered-drivers-(virtio-gpu-venus-gfxstream))).
The MVP may *instantiate* one ring, but the **ABI must be multi-ring from day one**, or we
repeat VirGL's mistake.

Topology:

- **One control ring per device** (device-global, ordered): capability negotiation, context
  and resource lifecycle, memory attach/map, scanout/cursor, reset. Low frequency; ordering
  and reliability matter more than throughput. Modeled on virtio-gpu's `controlq`.
- **N command rings, one per guest context** (high frequency): opaque command-buffer
  submission + inline fences. Each maps 1:1 to a host decoder thread.
- **Completion is a seqno, not a ring of its own.** Each command ring carries a monotonically
  increasing **submission seqno**; a shared **completion word** (in the ring header, written
  by the host, MSI-X-signalled) publishes the highest retired seqno — the Venus
  `vn_ring` seqno model, which orders operations "without requiring synchronous stalls."
  Fences resolve by comparing the published seqno.

Each ring is a classic SPSC descriptor ring in shared memory: a fixed **ring header**
(magic, ABI version, capacity, producer index, consumer index, retired-seqno,
error word) followed by a power-of-two array of **descriptors**. The guest writes the
payload into a data region, publishes a descriptor, bumps the producer index, rings the
doorbell (ioeventfd). The host consumes, decodes, executes, publishes the retired seqno,
raises MSI-X.

## 3. Message classes (the concrete protocol skeleton)

Cribbed directly from the virtio-gpu command set (which is why those names appear below), but
they are **our** enum values in **our** device. All fixed structs are `#[repr(C)]` + zerocopy.

**Negotiation / capability (control ring)**
- `GET_DEVICE_INFO` → GPU name, VRAM budget, max contexts, max ring size.
- `GET_CAPSETS` → bitmap of supported **capsets**: `CAP_VULKAN`, `CAP_D3D12`, `CAP_DISPLAY_ONLY`,
  plus per-capset `(version, blob)` payloads. This is exactly virtio-gpu's *capset* model
  (`VIRTIO_GPU_CAPSET_{VENUS,VIRGL2,GFXSTREAM,DRM,CROSS_DOMAIN}`), where a context declares
  which protocol it speaks ([capset patch](https://www.mail-archive.com/dri-devel@lists.freedesktop.org/msg537318.html)).
- `NEGOTIATE {abi_major, abi_minor, requested_capsets}` → the host pins a mutually-supported
  ABI minor and capset set. Fail-closed on major mismatch.

**Context lifecycle (control ring)**
- `CTX_CREATE {ctx_id, capset_id, api_type, name}` — `api_type ∈ {VULKAN, D3D12, DISPLAY}`.
  Mirrors virtio-gpu `CTX_CREATE` + `context_init`.
- `CTX_ATTACH_RING {ctx_id, ring_id}` — binds a command ring to a context.
- `CTX_DESTROY {ctx_id}` — must tear down all host twins (see §5 lifetime).

**Resource lifecycle (control ring)** — the zero-copy primitive, straight from virtio-gpu blobs:
- `RESOURCE_CREATE_BLOB {res_id, ctx_id, blob_mem, blob_flags, size}` where
  `blob_mem ∈ {GUEST, HOST3D, HOST3D_GUEST}` — identical semantics to
  `VIRTIO_GPU_BLOB_MEM_*` ([blob commands](https://lists.gnu.org/archive/html/qemu-devel/2024-10/msg04716.html)).
- `RESOURCE_ATTACH_BACKING {res_id, guest_pages[]}` — for GUEST/HOST3D_GUEST blobs; host
  wraps in a udmabuf.
- `RESOURCE_MAP_BLOB {res_id, offset}` / `RESOURCE_UNMAP_BLOB` — maps HOST3D memory into the
  guest BAR window (virtio-gpu `RESOURCE_MAP_BLOB`).
- `RESOURCE_DESTROY {res_id}`.

**Command submission (command ring)** — the payload-agnostic channel:
- `SUBMIT_CMD {ctx_id, seqno, encoding, payload_len, [in_fence, out_fence]}` followed by
  `payload_len` **opaque** bytes. `encoding` tags the sub-protocol
  (`VULKAN_VENUSLIKE` | `D3D12_DDI` | `DXGI_PRESENT`). This is virtio-gpu `SUBMIT_3D`
  generalized. The framing layer never parses the payload; only the matching host decoder
  does. (See §4 — this is the crux of "one protocol, two APIs".)

**Fences / sync (command ring)**
- Inline `in_fence`/`out_fence` seqnos on `SUBMIT_CMD` cover the common case.
- `FENCE_WAIT {ctx_id, seqno}` / host-signalled retirement via the ring's retired-seqno word,
  exportable to a Linux `sync_file` / DXGI monitored fence on the guest side.

**Presentation (control ring + cursor sub-channel)**
- `SET_SCANOUT_BLOB {scanout_id, res_id, w, h, format, stride}` — virtio-gpu
  `SET_SCANOUT_BLOB`.
- `RESOURCE_FLUSH {res_id, rect}` — "present"; host imports the dma-buf, encodes, feeds the
  SPICE/VNC relay (doc 06 §3). Guest waits on the flush fence.
- `CURSOR_UPDATE` / `CURSOR_MOVE` — low-latency cursor, virtio-gpu's separate cursorq.

**Control / async (control ring, host→guest)**
- `RESET {ctx_id | DEVICE}` — reclaim on guest crash.
- `EVENT {kind, ...}` — hotplug, display change, host-side error, out-of-VRAM.

## 4. How ONE protocol carries Vulkan (Linux) and D3D/DXGI (Windows)

The honest answer: **the envelope is shared; the payload is not.** `SUBMIT_CMD` is a typed,
versioned frame; its body is an **opaque blob produced by a per-API encoder**. This is
already how the reference stacks work — Venus serializes Vulkan into a `vn_cs` command
stream that virtio-gpu carries verbatim; the transport does not understand Vulkan
([Venus docs](https://docs.mesa3d.org/drivers/venus.html)).

- **Linux/Vulkan.** The guest Mesa driver encodes Vulkan into a Venus-style stream
  (`vn_encode_vk*`), sets `encoding = VULKAN_VENUSLIKE`, and submits. The host arbiter's
  Vulkan decoder replays against the NVIDIA Vulkan driver.
- **Windows/D3D.** Microsoft's **GPU-PV marshals the WDDM *DDI*** — the UMD→KMD driver
  interface (`pfnRender`, allocation/patch/present DDIs), **not** high-level D3D — from the
  guest partition to the host KMD ([GPU paravirtualization](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization)).
  We do the same shape: a guest WDDM UMD/KMD pair encodes the D3D12 UMD DDI stream, sets
  `encoding = D3D12_DDI`, and submits over the **same ring, same framing, same fences**. Only
  the encoder/decoder pair differs.

So the protocol is genuinely OS-neutral *because* it is API-neutral: resource, blob, fence,
scanout, and cursor semantics are identical for both guests, and only the `SUBMIT_CMD` body
diverges. **Skeptic's caveat:** GPU-PV runs exclusively over Hyper-V VMBus and is unavailable
to KVM guests (doc 06 §2.4). No one has carried a D3D/DDI payload over a KVM command ring —
the Windows encoder/decoder is net-new and remains the program's biggest unknown.
`DISPLAY`-only contexts (IddCx pixels, zero 3D) are the safe Windows first milestone and need
*only* the blob + scanout + flush + cursor messages — a strict subset that the Linux path
already exercises.

## 5. Shared Rust crate layout

Four crates, layered by `no_std` strictness so the *same* code compiles into a Linux kernel
module, a Windows driver, and the `std` host backend.

```
infinigpu-abi     #![no_std], no alloc      — wire types only
infinigpu-ring    #![no_std], alloc optional — ring producer/consumer + seqno logic
infinigpu-proto   #![no_std] + alloc         — control-message (de)serialization
infinigpu-vk-encode / -d3d-encode            — codegen'd per-API encoders (siblings)
```

- **`infinigpu-abi`** — every wire struct is `#[repr(C)]` and derives zerocopy
  `FromBytes, IntoBytes, Immutable, KnownLayout` (+ `Unaligned` where packed). Ring header,
  descriptor, message header, fence record, all enums as fixed-width `u32`. No allocation, no
  `core::fmt` in the hot path. This crate is the ABI contract; it changes only via §6.
- **`infinigpu-ring`** — SPSC index math, doorbell abstraction (a `Doorbell` trait), seqno
  publish/observe, back-pressure. Pure data-structure code → **property-tested and
  `loom`-tested** for the memory-ordering fences. `alloc` only for host-side bookkeeping.
- **`infinigpu-proto`** — encodes the variable-shape control messages (capset lists, names,
  page arrays) via **postcard**; carries the codegen'd VK/D3D encoders as feature-gated deps.
- **Glue is feature-gated, not forked:** `feature = "kernel"` maps `Doorbell` to an MMIO
  `writel` and allocation to the kernel `alloc`; `feature = "windows"` maps `Doorbell` to
  `WRITE_REGISTER_ULONG` and uses `wdk-alloc`; `feature = "std"` is the host backend with
  Tokio + `ash`. The `#![no_std]` core guarantees the kernel/driver builds never pull `std`
  (doc 05 §1: Linux kernel Rust is `#![no_std]` with fallible `alloc`).

## 6. Serialization choice — a justified hybrid

**Reject "one serializer everywhere."** The three payload kinds have opposite requirements:

1. **Fixed framing (ring header, descriptors, message headers, fence records): `zerocopy`.**
   These are written and read *in place* in shared memory, across the VM boundary, at
   doorbell frequency, sometimes from a kernel context. A serde pass or a copy per descriptor
   is unaffordable. `zerocopy` 0.8 (current release **0.8.54**, Jul 2026) gives
   `FromBytes`/`IntoBytes`/`Ref<>` reinterpretation with **compile-time size+alignment
   validation**, is `no_std` by default, and its `unsafe` is **Miri- and Kani-verified**
   ([zerocopy docs](https://docs.rs/zerocopy/latest/zerocopy/)) — exactly the assurance you
   want for a cross-privilege shared-memory ABI where a malformed guest descriptor must never
   be UB on the host.
2. **Opaque API payload (`SUBMIT_CMD` body): neither.** It is produced by the codegen encoder
   and `memcpy`'d through verbatim, like Venus's `vn_cs` stream. The framing crate treats it
   as bytes.
3. **Low-frequency control/negotiation messages: `postcard`.** Capset lists, device info,
   variable page arrays benefit from serde-derive ergonomics, optional fields, and
   forward-compat far more than they cost. postcard is a **`no_std`-first serde format with a
   documented, stable 1.0 wire format** and varint encoding
   ([postcard docs](https://docs.rs/postcard/), [postcard 1.0](https://jamesmunns.com/blog/postcard-1-0-run/)).
   Its non-zero-copy varint cost is irrelevant on messages sent once per context.

Reject **bincode** (not `no_std`-first historically; wire-format churn between versions) as
the wire format, and reject **postcard in the descriptor hot path** (varint + a deserialize
step per descriptor defeats the point of a shared-memory ring).

**Versioning / forward-compat.** ABI `major.minor` in the ring header; capsets negotiated at
`NEGOTIATE`. Every fixed struct reserves trailing `reserved: [u32; N]` padding so fields can
be added without shifting layout. Every message header is `{type, length}` so an unknown
message can be length-skipped (TLV discipline). New commands are new enum values **gated
behind a negotiated capset/feature bit** — the Venus/virtio-gpu model where adding an
entrypoint is guarded by an extension version, never an unconditional wire change.

**Codegen (Venus/Cereal-style).** The framing crate (`infinigpu-abi`/`-ring`) is
hand-written and stable. The per-command marshalling in `-vk-encode`/`-d3d-encode` is
**generated from a registry** — reuse Vulkan's `vk.xml` for the VK path (as venus-protocol's
Python generator does, emitting `vn_encode_*`/`vn_decode_*`; gfxstream's **Cereal** emits the
same encoder/decoder pair). A `build.rs` proc-macro or an offline Python generator emits the
guest encoder and host decoder from one spec, so adding API coverage is a **registry edit,
not hand marshalling** — the property that let Venus track Vulkan 1.4
([Venus deepwiki](https://deepwiki.com/arehnman/virtio-win-mesa/7.7-virtualized-and-layered-drivers-(virtio-gpu-venus-gfxstream))).

## What actually holds up

- OS-neutral **framing/resource/fence/present** protocol: **confirmed** — virtio-gpu is the
  existence proof, and it is already API- and OS-agnostic.
- `no_std`-clean shared Rust crate with `zerocopy` framing + `postcard` control: **confirmed**
  — both crates are production `no_std` in 2026.
- "Same protocol carries Vulkan and D3D": **confirmed only as a payload-agnostic envelope** —
  the two encoders share nothing; "one protocol" is a framing claim, not a command claim.
- Windows D3D-over-KVM-ring payload: **unresolved / highest risk** — GPU-PV proves the *DDI
  marshalling shape* but only over VMBus; our KVM encoder/decoder is unbuilt. Ship
  `DISPLAY`-only (IddCx) first.
- Wave-1 "one command ring": **corrected** to a multi-ring-capable ABI (gfxstream 1:1).

## Sources

- QEMU vfio-user protocol spec (doorbell/ioeventfd/mmap BAR): https://www.qemu.org/docs/master/interop/vfio-user.html
- QEMU VirtIO-GPU device (control/cursor queues, blob, scanout): https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html
- virtio-gpu blob commands (RESOURCE_CREATE_BLOB / MAP_BLOB / SET_SCANOUT_BLOB): https://lists.gnu.org/archive/html/qemu-devel/2024-10/msg04716.html
- virtio-gpu capset definitions (VENUS/VIRGL2/GFXSTREAM/DRM/CROSS_DOMAIN): https://www.mail-archive.com/dri-devel@lists.freedesktop.org/msg537318.html
- OASIS virtio-spec GPU device type: https://github.com/oasis-tcs/virtio-spec/blob/master/device-types/gpu/description.tex
- Mesa Venus driver (vn_cs command stream, codegen, blob requirement): https://docs.mesa3d.org/drivers/venus.html
- Venus/gfxstream architecture (vn_ring, ResourceTracker, Cereal, 1:1 threads): https://deepwiki.com/arehnman/virtio-win-mesa/7.7-virtualized-and-layered-drivers-(virtio-gpu-venus-gfxstream)
- gfxstream README (io_uring-style ring, 1:1 thread model, Cereal codegen): https://android.googlesource.com/platform/hardware/google/gfxstream/+/fbc9e43e236777dacf23c0d4bf71dc414df984a9/README.md
- Microsoft WDDM GPU paravirtualization (DDI marshalling, VMBus): https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization
- zerocopy crate (FromBytes/IntoBytes/Ref, Miri+Kani, no_std, v0.8.54): https://docs.rs/zerocopy/latest/zerocopy/
- postcard crate (no_std serde, stable wire format, flavors): https://docs.rs/postcard/
- postcard 1.0 wire-format stability: https://jamesmunns.com/blog/postcard-1-0-run/
- rust-vmm/vfio (vfio-user device-side Rust crate): https://github.com/rust-vmm/vfio
- Collabora — state of GFX virtualization (native context, capsets): https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/
