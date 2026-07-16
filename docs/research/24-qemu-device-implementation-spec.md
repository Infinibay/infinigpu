# 24 — infinigpu QEMU device: register-level implementation spec

**Scope:** the concrete, codeable device model for infinigpu's host seam — a **vfio-user
out-of-process PCI device in Rust** (ADR 0001), built on the `rust-vmm/vfio` `vfio-user`
crate, attached to QEMU ≥ 10.1.1 via `-device vfio-user-pci`. This is the register map,
message-handling table, argv, and lifecycle a developer starts a Rust crate against — not
architecture. It assumes the multi-ring protocol of doc 11 (1 control ring + N per-context
command rings, seqno completion via MSI-X) rides *on top* of these registers.

The seam gives us three primitives (doc 07): **direct BAR mmap** (a page the guest touches at
memory speed, backed by a server-passed fd), **ioeventfd doorbells** (a guest write is a bare
`eventfd` kick, no socket message), and **zero-copy memfd DMA** (guest RAM arrives as an
`SCM_RIGHTS` memfd the server `mmap`s). The register map is designed to put the hot path on
the first two and everything cold on the trapped socket path.

---

## 1. PCI configuration space

vfio-user exposes config space as **region 6** (the client forwards guest config loads/stores
as `VFIO_USER_REGION_READ`/`WRITE` on region index 6). Our server owns it, so these fields are
authoritative — but QEMU post-processes the class code, so we *also* pin it on the argv (§5).

| Field | Value | Rationale |
|---|---|---|
| Vendor ID | `0x1B36` (placeholder) | Red Hat/QEMU device vendor; no in-tree driver claims an arbitrary DEV under it, so it is a safe dev-time placeholder. Apply for a real PCI-SIG vendor ID before GA. (NEEDS VERIFICATION: registration path/cost.) |
| Device ID | `0x0100` (placeholder) | "infinigpu proto gen-0". Bump per hardware generation. |
| Revision ID | `0x01` | ABI/silicon-rev; guest driver may gate features on it. |
| Class code | `0x038000` **default**, `0x030000` when primary | Base class `0x03` = **Display controller**. See binding discussion below. |
| Subsystem Vendor ID | `0x1B36` | Encode Infinibay org. |
| Subsystem Device ID | `0x0001` | Encode model/tier (e.g. vGPU profile). |
| Interrupt Pin | `0x00` | No INTx — MSI-X only. |
| Capabilities ptr | → cap list | PM → MSI-X → PCIe Express → (optional VSC). |

**Capabilities list** (config space): **MSI-X capability** (64 vectors; Table BIR = 1,
offset `0x0000`; PBA BIR = 1, offset `0x2000`), **PCI Express capability** (device type =
*endpoint*, so it enumerates cleanly on a q35 root port), optional **Power Management**
capability, and an optional **vendor-specific capability** carrying the infinigpu ABI/build id.

**What makes the guest treat it as a display adapter and bind our driver.** Driver *binding*
is by **ID match**, not class: Windows matches the INF hardware id `PCI\VEN_1B36&DEV_0100` and
Linux DRM matches its `pci_device_id` table — so VEN/DEV is the load-bearing field. The
**class code** decides the *device category and VGA arbitration*, which is why it still
matters. Base class `0x03` puts the device in the **Display** setup class on Windows (GUID
`{4d36e968-…}`) and marks it a display device on Linux. The subclass is the real choice:

- **`0x030000` (VGA-compatible controller)** claims the legacy VGA/boot framebuffer and
  participates in `vgaarb` (Linux) / boot-VGA (Windows). Correct when infinigpu is the
  guest's **sole/primary** WDDM adapter.
- **`0x038000` (Other display controller)** is a non-VGA display device that does *not* fight
  over the boot framebuffer. Correct for our **IddCx display-first / secondary** Windows
  milestone (doc 03) where a std-vga/QXL console still owns the BIOS/boot display, and for a
  secondary Linux DRM adapter. **Default to `0x038000`**; switch to `0x030000` once infinigpu
  is the only adapter. (NEEDS VERIFICATION: exact Windows Basic-Display vs. IddCx binding
  nuance per subclass.)

QEMU 10.1.1 is mandatory here: the `x-pci-class-code` property (commit `a59d06305fff`) was
omitted from `vfio_user_pci_dev_properties` and backported into **Stable-10.1.1 (27/60)** —
without it a vfio-user device gets the wrong class code and class-bound guest logic misfires.

---

## 2. BAR layout + register map

Three BARs. BAR0 is deliberately split into a **trapped control page**, a **direct-mmap fast
index page**, and an **ioeventfd doorbell page**, because mmap and ioeventfd bindings are
page-granular and must not overlap the trapped control registers.

| BAR | Type | Size | Backing | Path |
|---|---|---|---|---|
| **BAR0** | 64-bit MMIO, non-prefetch | 64 KiB | mixed (see below) | control = trapped; indices = mmap; doorbells = ioeventfd |
| **BAR1** | 64-bit MMIO, non-prefetch | 16 KiB | QEMU-emulated MSI-X | MSI-X table + PBA |
| **BAR2** | 64-bit MMIO, **prefetchable** | configurable (e.g. 256 MiB) | host memfd via `mmap_fd` | direct mmap (optional; `DEV_CAPS` bit) |

**BAR0 register offset table.** Per-context registers use a stride; `i` = context/ring index
in `0..N` (N ≤ 63). Access column: **T** = trapped (`REGION_READ/WRITE` socket round-trip,
served by `region_read`/`region_write`); **M** = direct mmap (shared page, no trap); **E** =
ioeventfd (bare kick, value discarded).

| Offset | Reg | W | R/W | Path | Semantics |
|---|---|---|---|---|---|
| `0x0000` | `DEV_MAGIC` | 32 | RO | T | `0x49475055` ("IGPU"). |
| `0x0004` | `ABI_VERSION` | 32 | RO | T | `major<<16 \| minor`. |
| `0x0008` | `DEV_CAPS` | 32 | RO | T | bit0 IOEVENTFD_DOORBELL, bit1 BLOB_APERTURE(BAR2), bit2 MULTI_RING, bit3 64BIT_SEQNO. |
| `0x000C` | `NUM_CONTEXTS` | 32 | RO | T | N command rings supported. |
| `0x0010` | `MAX_RING_ENTRIES` | 32 | RO | T | power-of-two cap per ring. |
| `0x0014` | `BAR2_APERTURE_MB` | 32 | RO | T | 0 if BAR2 absent. |
| `0x0020` | `GLOBAL_CTRL` | 32 | RW | T | bit0 DEVICE_ENABLE, bit1 CTRL_RING_ENABLE. |
| `0x0024` | `GLOBAL_STATUS` | 32 | RO | T | bit0 READY, bit1 FATAL, bit2 NEEDS_RESET. |
| `0x0028` | `DEVICE_RESET` | 32 | WO | T | write `0x1` → soft reset (see §6). |
| `0x0030` | `IRQ_STATUS` | 32 | RO/W1C | T | pending-event mirror (real delivery is MSI-X). |
| `0x0034` | `IRQ_MASK` | 32 | RW | T | per-event mask. |
| `0x0040` | `CTRL_RING_BASE_LO/HI` | 64 | RW | T | control-ring guest-physical base. |
| `0x0048` | `CTRL_RING_SIZE` | 32 | RW | T | control-ring entries. |
| `0x0100 + i*0x40` | `CMD_RING_BASE_LO/HI` | 64 | RW | T | context `i` ring guest-physical base. |
| `+0x08` | `CMD_RING_SIZE` | 32 | RW | T | entries (≤ `MAX_RING_ENTRIES`). |
| `+0x0C` | `CMD_RING_CTRL` | 32 | RW | T | bit0 ENABLE, bit1 RESET. |
| `+0x10` | `CMD_RING_CAPSET` | 32 | RW | T | negotiated capset/api_type (VULKAN/D3D12/DISPLAY). |
| **`0x2000 + i*0x40`** | `CMD_RING_TAIL[i]` | 32 | RW(guest) | **M** | producer index; guest bumps after publishing descriptors. |
| `+0x04` | `CMD_RING_HEAD[i]` | 32 | RW(host) | **M** | consumer index; host advances. |
| `+0x08` | `CMD_RING_SEQNO_SUBMIT[i]` | 64 | RW(guest) | **M** | last submitted seqno. |
| `+0x10` | `CMD_RING_SEQNO_RETIRED[i]` | 64 | RW(host) | **M** | highest retired seqno; guest reads to resolve fences. |
| `+0x18` | `CMD_RING_STATUS[i]` | 32 | RO(guest) | **M** | per-ring error/backpressure bits. |
| `0x3000` | `CTRL_DOORBELL` | 32 | WO | **E** | write kicks the control-ring eventfd. |
| `0x3004 + i*4` | `CMD_RING_DOORBELL[i]` | 32 | WO | **E** | write kicks context-`i` eventfd. |

The `0x2000` page (all `CMD_RING_TAIL/HEAD/SEQNO/STATUS`) is declared as **one sparse-mmap
area** (`VFIO_REGION_INFO_CAP_SPARSE_MMAP`, offset `0x2000`, size `0x1000`) backed by a server
memfd — server and guest share the same physical pages, so index/seqno traffic is pure memory
access. The `0x0000–0x1FFF` control span and the `0x3000` doorbell page are **holes** in the
sparse set: control writes trap to the socket; doorbell writes are intercepted by KVM
ioeventfd. Ring **descriptors and payloads live in guest RAM** (programmed via
`CMD_RING_BASE`), reached zero-copy through the DMA memfd (§4) — BAR0 carries only the control
words.

**BAR1** hosts the MSI-X **Table** (64 × 16 B at `0x0000`) and **PBA** (`0x2000`). For vfio
devices the client (QEMU) emulates the MSI-X table and owns irqfd routing; our server only
**declares the vector count** (64) via `IrqInfo` and **receives the per-vector eventfds** via
`SET_IRQS`. Vector 0 = control-ring/device events; vectors `1..=63` = per-context completion.

**BAR2** (optional) is a single fully-mmap'd prefetchable aperture backed by a host memfd
(`mmap_fd`), into which **HOST3D blob resources** and the **scanout framebuffer** are windowed
at offsets assigned by `RESOURCE_MAP_BLOB` (doc 11 §3). Absent → guest-memory blobs via DMA
only.

---

## 3. vfio-user server message handling (→ `ServerBackend`)

The `rust-vmm/vfio` `vfio-user` crate answers negotiation/discovery from the arguments to
`Server::new(path, resettable, irqs: Vec<IrqInfo>, regions: Vec<ServerRegion>)` and dispatches
the live messages to a `&mut dyn ServerBackend`. The actual trait (crate `src/lib.rs`) is:

```rust
pub trait ServerBackend {
    fn region_read (&mut self, region: u32, offset: u64, data: &mut [u8]) -> io::Result<()>;
    fn region_write(&mut self, region: u32, offset: u64, data: &[u8])     -> io::Result<()>;
    fn dma_map  (&mut self, flags: DmaMapFlags, offset: u64, address: u64,
                 size: u64, fd: Option<File>) -> io::Result<()>;
    fn dma_unmap(&mut self, flags: DmaUnmapFlags, address: u64, size: u64) -> io::Result<()>;
    fn reset    (&mut self) -> io::Result<()>;
    fn set_irqs (&mut self, index: u32, flags: u32, start: u32, count: u32,
                 fds: Vec<File>) -> io::Result<()>;
}
```

| VFIO_USER message | Dir | Where we handle it | Our behavior |
|---|---|---|---|
| `VERSION` (1) | C→S | `Server` (crate) | Negotiate `max_msg_fds` (≥ #BARs + MSI-X + DMA fds), `max_data_xfer_size`, `pgsizes`. |
| `DEVICE_GET_INFO` (4) | C→S | `Server` from `new()` args | Reports `num_regions` = 7 (BAR0-2 + config 6), `num_irqs`, flags `PCI`+`RESET`. |
| `DEVICE_GET_REGION_INFO` (5) | C→S | `Server` from `ServerRegion` | Per region: `flags` (READ/WRITE/MMAP), `size`; **sparse-mmap** cap emitted from `sparse_areas`; mmap fd sent as SCM_RIGHTS from `mmap_fd`. |
| `DEVICE_GET_REGION_IO_FDS` (6) | C→S | `Server` (⚠ see §7) | Return `IOEVENTFD` sub-regions for each BAR0 doorbell offset (`0x3000`, `0x3004+i*4`), `fd_index` → eventfd in SCM_RIGHTS. |
| `REGION_READ` (9) | C→S | `region_read` | Serve BAR0 control regs (`0x0000–0x1FFF`) + **config space (region 6)**; return caps/status/ring-setup readback. |
| `REGION_WRITE` (10) | C→S | `region_write` | Apply `GLOBAL_CTRL`, `DEVICE_RESET`, `IRQ_MASK`, per-ring `BASE/SIZE/CTRL/CAPSET`, config-space writes (command reg, BAR sizing). |
| `DMA_MAP` (2) | C→S | `dma_map` | `mmap(fd, offset, size)` → host VA; record `[address, address+size) → hostVA` in an interval map (§4). |
| `DMA_UNMAP` (3) | C→S | `dma_unmap` | `munmap` + drop the interval; must release all refs before replying. |
| `DMA_READ/WRITE` (11/12) | S→C | `Server` (fallback) | Only if a range was **not** mmap-able; our memfd-backed RAM avoids this path entirely. |
| `DEVICE_GET_IRQ_INFO` (7) | C→S | `Server` from `IrqInfo` | Report MSI-X `count = 64`, `EVENTFD` flag. |
| `DEVICE_SET_IRQS` (8) | C→S | `set_irqs` | On `DATA_EVENTFD`+`ACTION_TRIGGER`: store the `Vec<File>` eventfds indexed by `start..start+count`. |
| `DEVICE_RESET` (13) | C→S | `reset` | Tear down contexts + replay threads, zero rings/registers, keep DMA maps (§6). |

`IrqInfo { index, flags, count }` and `ServerRegion { region_info, sparse_areas, mmap_fd }` are
the crate's declaration types; `Server::run(&self, backend)` drives the loop.

---

## 4. DMA + interrupt mechanics

**Zero-copy DMA.** Because QEMU is launched with `-object memory-backend-memfd,share=on`
(§5), the client sends one (or few) `DMA_MAP` covering all guest RAM, passing the memfd as
`SCM_RIGHTS`. Our `dma_map(flags, offset, address, size, fd)` does
`let hva = mmap(fd, offset, size)` and inserts `[address, address+size) → hva` into a sorted
interval tree. **IOVA → host-VA translation** is then `hva_base + (iova - address)` — O(1) for
the common single-map case. No copy, no socket transfer; a multi-hundred-MB texture upload is a
plain pointer read.

**Consuming a command ring** (context `i`):
1. Guest writes descriptors + payload into its ring buffer (guest RAM at `CMD_RING_BASE[i]`),
   `release`-barrier, bumps `CMD_RING_TAIL[i]` in the **shared BAR0 mmap page**, barrier, then
   writes `CMD_RING_DOORBELL[i]` (ioeventfd kick).
2. Server wakes on the eventfd, `acquire`-loads `CMD_RING_TAIL[i]` from the shared page,
   reads descriptors `HEAD..TAIL` from guest RAM via the translated host VA, and hands each
   `SUBMIT_CMD` payload to context `i`'s replay thread (doc 11's 1:1 model).

**Completion / MSI-X.** When the host retires seqno `S` for context `i`, it writes
`CMD_RING_SEQNO_RETIRED[i] = S` and advances `CMD_RING_HEAD[i]` in the shared BAR0 page
(`release`), then **raises MSI-X** by writing `1u64` (8 bytes) to the eventfd `File` stored for
vector `i+1` during `set_irqs`. QEMU's irqfd delivers the MSI-X interrupt; the guest ISR
`acquire`-loads `CMD_RING_SEQNO_RETIRED[i]` and resolves fences (Linux `sync_file` / DXGI
monitored fence, doc 11 §5). No socket message on either the submit or the completion hot path.

**Ordering caveat (NEEDS VERIFICATION):** the shared BAR0 index page is a QEMU RAM-device
mapping (`memory_region_init_ram_device_ptr`). Confirm the guest sees it write-back-coherent
(not UC) so `TAIL`/`SEQNO` reads/writes are cheap; either way explicit `smp_wmb`/`smp_rmb`
fences around index publication are required.

---

## 5. QEMU argv + infinization change

**argv fragment** (per VM; `<sock>` = `${INFINIZATION_SOCKET_DIR}/<vmId>.gpu.sock`):

```
-object memory-backend-memfd,id=ram0,size=8G,share=on
-machine q35,accel=kvm,memory-backend=ram0
-device {"driver":"vfio-user-pci",
         "socket":{"path":"<sock>","type":"unix"},
         "x-pci-class-code":"0x038000"}
```

`memory-backend-memfd,share=on` is the hard requirement that makes guest RAM a shareable fd
(without it DMA falls back to slow `DMA_READ/WRITE`). `x-pci-class-code` pins the display class
past QEMU's post-processing. Optionally add `x-pci-vendor-id`/`x-pci-device-id` to force
VEN/DEV, though our config-space (region 6) is already authoritative.

**`infinization/src/core/QemuCommandBuilder.ts` change.** Two additions, mirroring the
existing `addGpuPassthrough`/`-device` push pattern (the class already pushes `-device` lines
at lines ~273/473/529). Add a `memfd` mode to `setMemory` and a new `addInfinigpuDevice`:

```ts
// setMemory(sizeGB, { memfd = false }): when memfd, back RAM with a shared memfd so
// the vfio-user server can zero-copy-map guest RAM.
setMemory (sizeGB: number, opts: { memfd?: boolean } = {}): this {
  if (opts.memfd) {
    this.args.push('-object', `memory-backend-memfd,id=ram0,size=${sizeGB}G,share=on`)
    this.args.push('-machine', 'memory-backend=ram0')   // or fold into existing -machine
  } else {
    this.args.push('-m', `${sizeGB}G`)
  }
  return this
}

// New: attach the infinigpu vfio-user device. socketPath must live in the shared
// sockets dir; classCode defaults to secondary-display 0x038000.
addInfinigpuDevice (socketPath: string, classCode = '0x038000'): this {
  assertSafePath(socketPath, 'infinigpuSocket')
  assertInEnum(classCode, ['0x030000', '0x038000'], 'gpuClassCode')
  const dev = JSON.stringify({
    driver: 'vfio-user-pci',
    socket: { path: socketPath, type: 'unix' },
    'x-pci-class-code': classCode
  })
  this.args.push('-device', dev)
  return this
}
```

`VMLifecycle`/`InfinizationService` spawns the Rust server (`infinigpu-device --socket <sock>
--vm-id <id>`) **before** QEMU, ties its process lifetime to the QEMU process (kill on VM
stop), and reuses `INFINIZATION_SOCKET_DIR` for the socket so the existing shared-dir plumbing
applies.

---

## 6. Device lifecycle

1. **Bring-up / negotiation.** infinization allocates `<sock>`, spawns `infinigpu-device`,
   which builds `regions = [BAR0(sparse mmap fd for the 0x2000 index page), BAR1(MSI-X),
   BAR2(memfd aperture), config(region 6)]`, `irqs = [IrqInfo{ MSI-X, count:64 }]`, and calls
   `Server::new(sock, resettable=true, irqs, regions)` then `Server::run(backend)`. QEMU
   launches, connects, and runs `VERSION` → `GET_INFO` → `GET_REGION_INFO` (per region, picks
   up the sparse-mmap fd + BAR2 fd) → `GET_REGION_IO_FDS` (binds doorbell ioeventfds) →
   `GET_IRQ_INFO` → `SET_IRQS` (hands us the 64 completion eventfds) → `DMA_MAP` (guest RAM
   memfd → we mmap it).
2. **Guest enumeration + driver bind.** Guest enumerates PCI `VEN_1B36 DEV_0100`, class
   `0x03xx`. Windows loads our WDDM/IddCx KMD by INF hardware-id match; Linux DRM probes by
   `pci_device_id`. The driver reads `DEV_MAGIC`/`ABI_VERSION`/`DEV_CAPS`, allocates command
   rings in guest RAM, programs `CMD_RING_BASE/SIZE/CAPSET` (trapped writes), enables rings via
   `CMD_RING_CTRL` and the device via `GLOBAL_CTRL.DEVICE_ENABLE`.
3. **Steady state.** Submit = update `TAIL` (mmap) + ring doorbell (ioeventfd); complete =
   `SEQNO_RETIRED` (mmap) + MSI-X eventfd. Control ops (context/resource/scanout, doc 11) go
   through the control ring the same way on vector 0.
4. **Reset on reboot.** Guest reboot triggers a bus reset → `VFIO_USER_DEVICE_RESET` →
   `ServerBackend::reset`: stop and drain all replay threads, zero ring registers and shared
   index page, drop context/resource state, set `GLOBAL_STATUS.NEEDS_RESET` until re-enabled.
   DMA maps persist until the client re-sends `DMA_UNMAP`/`DMA_MAP`. A guest-initiated soft
   reset via the `DEVICE_RESET` register takes the same path. This addresses the reset-residual
   isolation requirement (doc 10).
5. **Teardown on VM stop.** QEMU exits → socket EOF → `Server::run` returns → server
   `munmap`s all DMA/BAR memfds, kills replay threads, and unlinks `<sock>`. infinization reaps
   the server process (tied to the QEMU process group in `VMLifecycle` stop) so no orphan
   remains — the same discipline already applied to QMP/console relays.

---

## 7. Open risks / NEEDS VERIFICATION before coding

- **`GET_REGION_IO_FDS` in the crate.** The `rust-vmm/vfio` `vfio-user` `ServerBackend` trait
  (v0.1.x) exposes `region_read/write`, `dma_map/unmap`, `reset`, `set_irqs` — but **no
  `get_region_io_fds` hook** is visible in the trait. Verify the crate actually emits
  `VFIO_USER_DEVICE_GET_REGION_IO_FDS` (ioeventfd bindings) or be prepared to patch it.
  **Fallback:** doorbell writes served as trapped `REGION_WRITE` — acceptable if the guest
  driver rings **one doorbell per submission batch**, not per command, keeping the socket
  round-trip off the per-command path.
- **MSI-X on the young client.** Confirm QEMU 10.1.1's vfio-user client emulates the MSI-X
  table + irqfd routing and delivers server eventfds (Levon's "immediate needs" caveat; SPDK
  NVMe is the best-tested reference — libvfio-user).
- **RAM-device coherence** of the shared BAR0 index page (§4) and **`memory-backend-memfd`
  interaction** with hugepages/NUMA/ballooning (ADR 0001).
- **Class-code default** (`0x038000` vs `0x030000`) against real Windows IddCx binding.

Reference device to mirror: **libvfio-user's SPDK NVMe server** (doorbell + BAR mmap + MSI-X +
DMA) and the `rust-vmm/vfio` `vfio-user/examples/gpio` PCI device.

## Sources

- QEMU — vfio-user Protocol Specification (message IDs, region indices, sparse-mmap cap, SET_IRQS/DMA_MAP fds): https://www.qemu.org/docs/master/interop/vfio-user.html
- QEMU — vfio-user client device doc (`-device vfio-user-pci` syntax): https://www.qemu.org/docs/master/system/devices/vfio-user.html
- QEMU — The memory API (RAM-device / BAR mmap semantics): https://www.qemu.org/docs/master/devel/memory.html
- rust-vmm/vfio — repo (100% Rust `vfio-user` crate, `ServerBackend`, `Server::new`, `ServerRegion`, `IrqInfo`): https://github.com/rust-vmm/vfio
- rust-vmm/vfio — `vfio-user/src/lib.rs` (trait + config types): https://raw.githubusercontent.com/rust-vmm/vfio/main/vfio-user/src/lib.rs
- rust-vmm/vfio — `vfio-user/examples/gpio` (reference PCI device): https://github.com/rust-vmm/vfio/tree/main/vfio-user/examples/gpio
- vfio-user crate API (docs.rs): https://docs.rs/vfio-user
- QEMU — "vfio/pci: Introduce x-pci-class-code option" (PATCH v2, `a59d06305fff`): https://www.mail-archive.com/qemu-devel@nongnu.org/msg1118137.html
- QEMU — "hw/vfio-user: add x-pci-class-code" (PULL 09/31): https://www.mail-archive.com/qemu-devel@nongnu.org/msg1137044.html
- QEMU — Stable-10.1.1 backport (27/60) of the class-code fix: https://www.mail-archive.com/qemu-devel@nongnu.org/msg1140654.html
- John Levon — "vfio-user client in QEMU 10.1" (ioeventfds/irqfds, immediate-needs caveat): https://movementarian.org/blog/posts/2025-08-27-vfio-user-client-in-qemu/
- nutanix/libvfio-user — SPDK NVMe reference server: https://github.com/nutanix/libvfio-user
- infinigpu prior docs: research/01 (device-model survey), 07 (seam verification, QEMU 10.1.1), 11 (multi-ring wire protocol); ADR 0001 (host device seam: vfio-user).
