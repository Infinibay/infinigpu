# 07 — Verifying the Host Device Seam: vfio-user (A) vs. our-own virtio-style vhost-user (B)

**Central claim under test:** *"vfio-user is the right host device seam for a GPU."*

**Verdict: PARTIALLY-CONFIRMED** — every fast-path primitive vfio-user needs (direct
BAR mmap, ioeventfd doorbells, zero-copy memfd DMA, device reset) is real and shipping
in QEMU 10.1.1, and a **complete pure-Rust server crate already exists**; the one genuine
gap (device-state migration/`savevm`) does **not** bite Infinibay because Infinibay uses
cold migration + disk snapshots, not live device-state migration. The claim holds; the
"vfio-user is *the* seam" absolutism is what needs qualifying, since option B is a viable
fallback that reuses more of the guest's in-tree stack.

---

## 1. Does the QEMU vfio-user *client* direct-map BARs, or round-trip every MMIO?

**Direct-maps them.** The vfio-user protocol reuses the ordinary VFIO region model:
a region carries `VFIO_REGION_INFO_FLAG_MMAP`, and when set "the reply will include a
file descriptor in its meta-data" that the client `mmap()`s
([spec](https://www.qemu.org/docs/master/interop/vfio-user.html)). QEMU's client then
does exactly what native `vfio-pci` does — `memory_region_init_ram_device_ptr()` over the
mmap'd pointer, `memory_region_add_subregion` + `pci_register_bar`
([memory API](https://www.qemu.org/docs/master/devel/memory.html)). The upshot: any BAR
the *server* chooses to back with a shareable fd becomes a **RAM-device mapping the guest
touches directly**, with no socket round-trip per access. Only regions the server does
*not* expose as fds fall back to `VFIO_USER_REGION_READ/WRITE` over the socket. For our
design this is ideal: put the command-ring doorbell + a small status/register page in an
mmap-able BAR and the guest writes it at memory speed. *(Client-side reality confirmed
against the shared VFIO region code path; **NEEDS VERIFICATION** only in the narrow sense
that I read the spec + memory-API docs, not a line-by-line audit of `hw/vfio-user/`.)*

## 2. ioeventfd-style doorbells?

**Yes.** `VFIO_USER_DEVICE_GET_REGION_IO_FDS` lets the server return sub-regions each
bound to an **ioeventfd** or **ioregionfd** — "an optional feature intended for
performance improvements where an underlying sub-system (such as KVM) supports
communication across such file descriptors ... without needing to round-trip through the
client" ([spec](https://www.qemu.org/docs/master/interop/vfio-user.html)). John Levon's
own launch write-up confirms the 10.1 client wired this up:
"memory sharing between VM and device, **ioeventfds and irqfds** for performance
optimization" ([Levon blog](https://movementarian.org/blog/posts/2025-08-27-vfio-user-client-in-qemu/)).
So a doorbell write in a BAR is delivered to our server as a bare `eventfd` kick via KVM —
**no socket message on the hot path** — which is precisely the submit primitive an
API-remoting arbiter wants.

## 3. DMA: zero-copy memfd, or DMA_READ/WRITE over the socket for GPU-sized transfers?

**Zero-copy by default; socket transfer only as fallback.** The client sends
`VFIO_USER_DMA_MAP`; "if the DMA region ... can be directly mapped by the server, a file
descriptor must be sent as part of the message meta-data" (over `AF_UNIX` as `SCM_RIGHTS`
ancillary data) "and the region can be mapped via the `mmap()` system call." Only "if the
DMA region cannot be directly mapped ... the DMA region can be accessed by the server
using `VFIO_USER_DMA_READ` and `VFIO_USER_DMA_WRITE` messages"
([spec](https://www.qemu.org/docs/master/interop/vfio-user.html)). The `DMA_READ/WRITE`
path (a dedicated "message-based DMA" series,
[PATCH v11](https://www.mail-archive.com/qemu-devel@nongnu.org/msg1061509.html)) is the
degraded mode, not the norm.

**The catch — and it's a config requirement, not a blocker:** zero-copy only works if
guest RAM is backed by a shareable fd. "QEMU needs to allocate the backing memory for all
the guest RAM as shared memory. No host setup is required when using the Linux **memfd**
memory backend" ([search corpus / QEMU docs](https://www.qemu.org/docs/master/interop/vfio-user.html)).
So Infinibay's `infinization` `QemuCommandBuilder` must add
`-object memory-backend-memfd,share=on` (identical to the vhost-user requirement). With
that, a multi-hundred-MB framebuffer/texture upload is a plain `mmap` on the server side —
no copy, no socket. This is the single most important perf property for a GPU seam and it
holds.

## 4. Reset / snapshot / migration — relevant because Infinibay snapshots & migrates VMs

- **Reset:** `VFIO_USER_DEVICE_RESET` exists — "sent from the client to the server to
  reset the device" ([spec](https://www.qemu.org/docs/master/interop/vfio-user.html)).
  Guest reboot / bus reset is covered.
- **Live migration / `savevm` of device state:** the *protocol* defines the full VFIO
  migration v2 state machine (`RUNNING/STOP/STOP_COPY/PRE_COPY/RESUMING`,
  `DMA_LOGGING_START/REPORT`, `MIG_DATA_READ/WRITE`), but the **QEMU 10.1 client is a
  minimal first cut** and Levon explicitly frames it as covering "our immediate needs"
  with "an awful lot more we could be doing"
  ([Levon blog](https://movementarian.org/blog/posts/2025-08-27-vfio-user-client-in-qemu/)).
  Generically, a VFIO device that does not implement migration is marked **unmigratable**,
  and "you cannot snapshot a VM if it has a passed-through GPU" — the same class of error
  Proxmox users hit ("VFIO migration is not supported")
  ([QEMU VFIO migration docs](https://www.qemu.org/docs/master/devel/migration/vfio.html)).
  So a vfio-user GPU device would, out of the box, make a VM **non-`savevm`-snapshottable
  and non-live-migratable** unless *we* implement the migration state machine in our
  server.

  **Why this is not fatal for Infinibay:** Infinibay does **qcow2 disk snapshots** (the
  `infinization` `SnapshotManager`) and **cold migration only** — "only cold VM migration
  exists; live is unimplemented" (project memory). In both flows the VM is off or the disk
  is snapshotted independently of live device state; the GPU device is torn down and
  re-created on start/destination. There is no live device-state to preserve. The one thing
  to verify in the spike is that attaching a vfio-user device does not make QEMU refuse a
  *disk* snapshot of a running VM (it should not — disk snapshots don't serialize device
  state), and that a graceful `savevm` attempt fails cleanly rather than corrupting.

## 5. The Rust angle — is rust-vmm's `vfio-user` a complete pure-Rust server?

**Yes, complete and pure-Rust.** The crate lives in
[rust-vmm/vfio](https://github.com/rust-vmm/vfio) (repo is "100% Rust") and its API
([docs.rs](https://docs.rs/vfio-user)) exposes a full **server**: a `Server` struct, a
`ServerBackend` trait for custom device logic, `ServerRegion` for BARs, `IrqInfo` for
MSI-X/interrupt handling, `DmaMapFlags`/`DmaUnmapFlags` for DMA-region management, plus
region mmap — and a `Client` too. Its dependencies are `vfio-bindings` (constants),
`vm-memory`, `serde`, `bitflags` — **no `-sys` crate, no C libvfio-user**. It was spun out
of cloud-hypervisor's minimal-dependency vfio-user implementation precisely because it was
a "good candidate for a rust-vmm crate"
([CH issue #5123](https://github.com/cloud-hypervisor/cloud-hypervisor/issues/5123)).
This directly answers the task's Rust question: we can implement our GPU arbiter as a
**custom `ServerBackend`** — BARs, MSI-X, and DMA regions all in Rust — and never touch C.
This is the strongest single point in favour of A.

## 6. Option B — our own virtio-style device over rust-vmm vhost-user

Could we instead define a **new virtio device type** (our own virtio device ID) fronted by
QEMU and served by a rust-vmm `vhost-user-backend`, and thereby get "the mature
blob/udmabuf/dma-fence transport without adopting upstream virtio-gpu"?

**Two of the three assumptions in that framing are wrong:**

1. **The blob/udmabuf/dma-fence maturity is NOT in generic virtio/vhost-user — it lives in
   the virtio-gpu *device model* + `rutabaga_gfx`/virglrenderer.** `resource_create_blob`,
   `udmabuf_driver`, guest-vs-host blob handling, and fence creation are all in crosvm's
   virtio-gpu device and `rutabaga_gfx`
   ([crosvm virtio_gpu](https://crosvm.dev/doc/devices/virtio/gpu/virtio_gpu/struct.VirtioGpu.html),
   [rutabaga_gfx](https://crosvm.dev/doc/rutabaga_gfx/index.html)), and blob resources were
   a dedicated virtio-**gpu** feature ([QEMU blob series](https://patchew.org/QEMU/20220913105022.81953-1-antonio.caggiano@collabora.com/20220913105022.81953-5-antonio.caggiano@collabora.com/)).
   If we do **not** adopt virtio-gpu, we do **not** inherit any of it — we reimplement blob
   resource management ourselves either way. **This claim is REFUTED.**

2. **QEMU's generic front-end for a bring-your-own virtio device is dev-only.** The
   `vhost-user-device`/`vhost-user-device-pci` is documented as "a generic development
   device intended for expert use while developing new backends ... **not recommended for
   production use**"
   ([QEMU vhost-user docs](https://www.qemu.org/docs/master/system/devices/virtio/vhost-user.html)).
   By contrast `vfio-user-pci` is a real, shipping, production-intended device. So B's QEMU
   attach story is *weaker*, not stronger, than A's.

**What B genuinely gives us:** the transport *primitives* are equivalent — vhost-user
shares guest RAM as memfd regions and uses kick/call **eventfds** (doorbells + interrupts),
mirroring vfio-user's DMA-fd + ioeventfd/irqfd. And rust-vmm's `vhost-user-backend` +
`vhost-device` are **more battle-tested** than the `vfio-user` crate (many production
backends: i2c, gpio, sound, vsock, scsi —
[Linaro](https://www.linaro.org/blog/rust-device-backends-for-every-hypervisor/),
[rust-vmm/vhost-device](https://github.com/rust-vmm/vhost-device)). B's other real win is
**guest-side reuse**: the guest binds our device through the in-tree `virtio_pci` +
`virtio_ring` bus and the virtio DMA API, so we write a *virtio driver* rather than a *raw
PCI driver*.

**What B costs:** we still write a from-scratch guest UMD/KMD (there is no in-tree driver
for our custom device ID — using virtio-gpu's driver *would* be the disallowed adoption),
we lose the clean "we are a real GPU PCI device" model that WDDM (Windows expects a PCI
display adapter) and Linux DRM's PCI-device model both assume, and we accept a dev-only
QEMU front-end. On the ownership constraint, **both A and B satisfy it** (neither uses
virtio-gpu / vhost-user-gpu); a *new* virtio device ID is still our own code.

## 7. Recommendation — A (vfio-user), with our own protocol crate on top

Adopt **vfio-user (A)** as the host device seam:

- It models "we are a real GPU PCI device," which is exactly what a from-scratch WDDM KMD
  and a Linux DRM PCI driver both expect — and a from-scratch guest driver is unavoidable
  in *either* option, so B's virtio-bus reuse is a modest convenience, not a decisive win.
- `vfio-user-pci` is production-intended in QEMU 10.1.1; B's generic front-end is dev-only.
- A **complete pure-Rust server** (`rust-vmm/vfio` `vfio-user`) already gives us BARs +
  MSI-X + memfd DMA + region mmap with no C dependency.
- Direct BAR mmap gives a natural home for the command-ring doorbell/register file;
  ioeventfd + memfd DMA give the zero-copy submit/transfer path a GPU needs.
- Fully custom PCI ABI = total ownership, no virtio-gpu adoption.

This is really a **hybrid in the useful sense**: use vfio-user for the *device seam*, but
keep the command-ring / blob / fence protocol as **our own shared `no_std` crate** (not
virtio-gpu's) — the maturity we'd "reuse" in B doesn't exist for free anyway. The
presentation dma-buf is produced host-side by the arbiter, independent of the seam.

**Fallback:** if the young vfio-user client bites us (MSI-X quirks, hotplug gaps, an
immaturity the crate can't paper over), fall back to B — the same Rust `ServerBackend`
logic re-homed onto the more-mature `vhost-user-backend` behind QEMU's generic
`vhost-user-device-pci`, accepting its dev-only status and writing a custom virtio driver
in the guest.

## 8. The 1–2 week de-risking spike

Build a **throwaway custom PCI device** with `rust-vmm/vfio`'s `Server`/`ServerBackend`:

- **Device:** BAR0 (mmap-able region fd) exposing a doorbell + status register; one MSI-X
  vector; handle `VFIO_USER_DMA_MAP` by mmap'ing the memfd; read a command struct the guest
  writes into DMA-mapped guest RAM; signal completion via MSI-X.
- **Launch:** QEMU **10.1.1** (or master — see §9) with
  `-object memory-backend-memfd,share=on` + `-device vfio-user-pci,socket=...`. Guest =
  Linux with a ~200-line out-of-tree PCI driver (or in-guest `vfio-pci` from userspace) that
  maps BAR0, writes the doorbell, and parks a buffer in DMA-mapped RAM.
- **Measure / prove:** (a) doorbell arrives as an **eventfd kick, not a socket message**;
  (b) a **256 MB** buffer is visible to the server **zero-copy via mmap** (not `DMA_READ`);
  (c) MSI-X interrupt round-trips back to the guest and its latency; (d) `VFIO_USER_DEVICE_RESET`
  fires on guest reboot; (e) a **qcow2 disk snapshot of the running VM succeeds** with the
  device attached (the Infinibay-relevant path); (f) a `savevm` attempt fails *cleanly*
  (document the migration limitation).
- **Success criteria:** (a) eventfd doorbell, (b) zero-copy 256 MB, (c) working interrupt,
  (e) disk snapshot unaffected. If any hard-fails and can't be worked around, run the
  mirror-image B spike (same backend logic on `vhost-user-backend` +
  `vhost-user-device-pci`) before committing.

## 9. The post-10.1 regression (found)

Levon's launch note warns you need "something a little bit more recent than the actual
10.1 release" ([blog](https://movementarian.org/blog/posts/2025-08-27-vfio-user-client-in-qemu/)).
The fix is **`hw/vfio-user: add x-pci-class-code`** (John Levon, 2025-08-27,
`20250827190810.1645340-1-john.levon@nutanix.com`): the new `x-pci-class-code` option added
in commit **`a59d06305fff`** ("vfio/pci: Introduce x-pci-class-code option") was omitted
from `vfio_user_pci_dev_properties`, giving vfio-user devices an **incorrect PCI class
code** — which breaks guest drivers that bind by class. It landed via
[PULL 09/31](https://www.mail-archive.com/qemu-devel@nongnu.org/msg1137044.html) and was
backported to **[Stable-10.1.1](https://www.mail-archive.com/qemu-devel@nongnu.org/msg1140654.html)**
(27/60). **Action: build against QEMU ≥ 10.1.1 (or master).** A GPU presents a class code
(0x030000, VGA/display), so this fix is directly load-bearing for us.

## Sources

- QEMU — vfio-user Protocol Specification: https://www.qemu.org/docs/master/interop/vfio-user.html
- QEMU — vfio-user client (system device doc): https://www.qemu.org/docs/master/system/devices/vfio-user.html
- QEMU — The memory API (RAM-device / BAR mmap): https://www.qemu.org/docs/master/devel/memory.html
- QEMU — VFIO device migration: https://www.qemu.org/docs/master/devel/migration/vfio.html
- John Levon — "vfio-user client in QEMU 10.1": https://movementarian.org/blog/posts/2025-08-27-vfio-user-client-in-qemu/
- Regression fix "hw/vfio-user: add x-pci-class-code" (PULL): https://www.mail-archive.com/qemu-devel@nongnu.org/msg1137044.html
- Stable-10.1.1 backport of the fix: https://www.mail-archive.com/qemu-devel@nongnu.org/msg1140654.html
- Message-based DMA support (fallback path), PATCH v11: https://www.mail-archive.com/qemu-devel@nongnu.org/msg1061509.html
- rust-vmm/vfio (repo, 100% Rust): https://github.com/rust-vmm/vfio
- rust-vmm vfio-user crate API (docs.rs): https://docs.rs/vfio-user
- cloud-hypervisor issue #5123 — spin out vfio-user as rust-vmm crate: https://github.com/cloud-hypervisor/cloud-hypervisor/issues/5123
- cloud-hypervisor vfio-user doc (experimental status): https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/vfio-user.md
- QEMU vhost-user back ends (generic vhost-user-device = dev-only): https://www.qemu.org/docs/master/system/devices/virtio/vhost-user.html
- rust-vmm/vhost-device (mature vhost-user backends): https://github.com/rust-vmm/vhost-device
- Linaro — Rust device backends for every hypervisor: https://www.linaro.org/blog/rust-device-backends-for-every-hypervisor/
- crosvm virtio-gpu (blob/udmabuf lives in the device model): https://crosvm.dev/doc/devices/virtio/gpu/virtio_gpu/struct.VirtioGpu.html
- crosvm rutabaga_gfx (blob/fence maturity): https://crosvm.dev/doc/rutabaga_gfx/index.html
- QEMU virtio-gpu blob resources series: https://patchew.org/QEMU/20220913105022.81953-1-antonio.caggiano@collabora.com/20220913105022.81953-5-antonio.caggiano@collabora.com/
