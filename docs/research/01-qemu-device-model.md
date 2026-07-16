# 01 — Presenting an OWNED virtual GPU device to a guest from QEMU

**Focus:** How do we make QEMU hand a guest a PCI device *we* fully control, ideally
without forking/patching QEMU? This surveys the five host-side mechanisms, judges each
on (a) can we own it in Rust, (b) does it avoid patching QEMU, (c) maturity/risk, and
ends with a ranked recommendation.

Scope note: this document is only about the **host-device presentation seam** — the
socket/protocol/qdev surface that makes a guest enumerate a PCI GPU. The *guest driver*
(WDDM/DRM) and the *rendering engine* are separate research tracks. A mechanism that
makes the guest see "a PCI device with config space, BARs, MSI-X and DMA" is a success
here even though a real GPU still needs a guest driver on top.

---

## 1. Out-of-process PCI device via vfio-user + libvfio-user

**What it is.** vfio-user lets you implement an *arbitrary* PCI device in a separate
userspace process (the "server"), talking a binary protocol over a UNIX socket to a
client inside the VMM. It is deliberately "similar to vhost-user … but can emulate
arbitrary PCI devices, not just virtio"
([QEMU vfio-user docs](https://www.qemu.org/docs/master/system/devices/vfio-user.html)).
The guest sees a **completely normal PCI device**: QEMU exposes a generic VFIO device to
the guest and forwards all interactions to your server.

**Upstream status — this is the big 2025 change.** The vfio-user **client is merged into
upstream QEMU as of QEMU 10.1** (released Aug 2025). Before that it lived out-of-tree for
years in Oracle's `oracle/qemu` fork (branches like `vfio-user-rfc3`), and John Levon
(Nutanix) drove the upstreaming
([Levon blog, 2025-08-27](https://movementarian.org/blog/posts/2025-08-27-vfio-user-client-in-qemu/);
[qemu-devel patch v3 thread](https://www.mail-archive.com/qemu-devel@nongnu.org/msg1122546.html)).
Invocation is a plain device line, **no QEMU patch required**:

```
-device '{"driver":"vfio-user-pci","socket":{"path":"/tmp/vfio-user.sock","type":"unix"}}'
```

([QEMU vfio-user docs](https://www.qemu.org/docs/master/system/devices/vfio-user.html)).

Two honest caveats: Levon notes a **"late-breaking regression"** so you want QEMU
*slightly newer than the 10.1 tag*, and that the client only has "enough implemented to
cover our immediate needs" (NVMe/SPDK) — "undoubtedly there are other implementations
that need extensions"
([Levon blog](https://movementarian.org/blog/posts/2025-08-27-vfio-user-client-in-qemu/)).
Also note: a *server-inside-QEMU* RFC series existed but was **not** the thing that
merged — only the client did
([vfio-user server RFC](https://mail-archive.com/qemu-devel@nongnu.org/msg825021.html)).
Our server is always our own process, which is exactly what we want.

**What the server must implement** (from the
[vfio-user protocol spec](https://www.qemu.org/docs/master/interop/vfio-user.html)):

- **Negotiation / discovery:** `VFIO_USER_VERSION`, `DEVICE_GET_INFO`,
  `DEVICE_GET_REGION_INFO`, `DEVICE_GET_IRQ_INFO`.
- **Config space + BAR/MMIO:** the client forwards guest loads/stores to unmapped device
  regions as `VFIO_USER_REGION_READ` / `REGION_WRITE`; the server replies with data or an
  ack. PCI config space is just region 7.
- **Guest DMA:** the client sends `VFIO_USER_DMA_MAP` / `DMA_UNMAP` describing the valid
  guest-RAM ranges the device may touch (typically passing **memfds** so the server can
  `mmap` guest RAM directly). Where a direct mapping isn't available, the server reads/writes
  guest memory with `VFIO_USER_DMA_READ` / `DMA_WRITE` over the socket.
- **Interrupts / MSI-X:** `VFIO_USER_DEVICE_SET_IRQS` wires **eventfds** passed from the
  client; the server raises an IRQ by writing the eventfd
  ([libvfio-user README](https://github.com/nutanix/libvfio-user/blob/master/README.md)).
- **Optional:** `DEVICE_RESET`, `REGION_WRITE_MULTI` (coalescing), `GET_REGION_IO_FDS`
  (ioeventfd/ioregionfd fast-path), and migration (`DEVICE_FEATURE`, `MIG_DATA_READ/WRITE`).

**Performance reality (matters a lot for a GPU).** A naive BAR access is a **socket
round-trip per MMIO** — fine for an NVMe doorbell, catastrophic for a GPU's register/aperture
traffic. The escape hatches are the same as real VFIO: expose **mmap-able BAR regions** (backed
by shared-memory fds, so the guest maps them straight to the server's memory — no round-trip)
for framebuffers/ring pages, and use **ioeventfd** for doorbells so a guest write kicks an
eventfd instead of a socket message. The protocol supports this; **NEEDS VERIFICATION** that the
young QEMU 10.1 *client* implements server-provided region-mmap fds and IO-fds fully (Levon's
"immediate needs" caveat suggests SPDK's paths are best-tested).

**Can we own it in Rust?** Yes. Two routes: the C **libvfio-user** (Nutanix, the mature
reference, drives SPDK's NVMe server) called via FFI; or **pure Rust** via the
**rust-vmm `vfio-user` crate** — "safe wrappers to implement vfio-user devices"
([rust-vmm/vfio](https://github.com/rust-vmm/vfio)). The Rust crate is real but **less
battle-tested as a *server*** than C libvfio-user (rust-vmm's heaviest vfio-user use is
Cloud Hypervisor's *client* side —
[CH vfio-user docs](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/vfio-user.md)).
**NEEDS VERIFICATION:** a production-grade pure-Rust vfio-user *server* interoperating with
the upstream QEMU 10.1 client.

**Verdict:** owns-in-Rust ✅, avoids QEMU patch ✅ (client is upstream), maturity **young but
real and improving fast**. This is the only mechanism that gives us a fully-owned *arbitrary*
PCI device with real DMA/MSI-X and **no fork**.

---

## 2. vhost-user / vhost-user-gpu

**What it standardizes.** vhost-user runs a **virtio** device's dataplane in a separate
process, sharing guest memory over memfds and using virtqueues + eventfd "kicks". A
**vhost-user-gpu** backend specifically renders a **virtio-gpu** and ships the result to the
QEMU frontend
([vhost-user-gpu protocol](https://www.qemu.org/docs/master/interop/vhost-user-gpu.html)).
The QEMU frontends `vhost-user-gpu-pci` / `vhost-user-vga` are **already upstream** — no patch.

**Rust backend exists.** rust-vmm's **`vhost-device-gpu`** implements virtio-gpu over
vhost-user in Rust, using `rutabaga_gfx` with `--gpu-mode virglrenderer` (OpenGL) or
`gfxstream` (Vulkan, "partial support only")
([vhost-device-gpu man page](https://www.mankier.com/1/vhost-device-gpu)). It is **v0.1.0,
early-stage**, and today "only sharing the display output to QEMU … is supported" with
several VIRTIO GPU commands unimplemented
([mankier](https://www.mankier.com/1/vhost-device-gpu)).

**Why it's the wrong core for us.** vhost-user-gpu is **hard-wired to the virtio-gpu device
model and its guest driver stack** (virtio-gpu DRM / virgl / venus / gfxstream). Adopting it
*is* adopting virtio-gpu — which the project has explicitly ruled out as the solution. We'd
inherit virtio-gpu's guest-side requirements and its Windows story is weak. It's an excellent
**reference architecture** for the out-of-process + memfd + eventfd pattern (and confirms the
pattern is productionizable in Rust), but not a device we can call "ours."

**Verdict:** owns-in-Rust ✅, avoids patch ✅, but **locks us into virtio-gpu** → fails hard
constraint (a). Reference only.

---

## 3. Custom PCI/PCIe device model *inside* QEMU (qdev, C)

**What it is.** Write the device as a C `TypeInfo` extending `TYPE_PCI_DEVICE`, with
`class_init` setting `PCIDeviceClass` fields (`realize`, `vendor_id`, `device_id`, `class_id`),
`realize` calling `memory_region_init_io()` + `pci_register_bar()` for BARs, `MemoryRegionOps`
read/write callbacks, `msi_init`/`msix_init` for interrupts, and `pci_dma_read/write` for guest
DMA
([Airbus SecLab QEMU internals](https://airbus-seclab.github.io/qemu_blog/pci_slave.html);
[davidv.dev PCIe emulation](https://blog.davidv.dev/posts/learning-pcie/);
[QEMU qdev API](https://www.qemu.org/docs/master/devel/qdev-api.html)).

**The cost.** It is **C, compiled into QEMU**, and requires editing `meson.build`/`Kconfig`
and building QEMU from source
([davidv.dev](https://blog.davidv.dev/posts/learning-pcie/)). That means a **permanent QEMU
fork**: rebasing our device on every QEMU release, tracking API churn in the memory/PCI/qdev
subsystems, shipping and signing custom `qemu-system-x86_64` binaries, and reconciling with
distro packaging. The device APIs themselves are the **most mature** of any option here — this
is how every builtin QEMU device works — and you get in-process speed with no socket seam. But
it directly violates our "no forking/patching QEMU" preference and puts the whole GPU in C, not
Rust (Rust-in-QEMU exists but is nascent and not a supported device-authoring path — **NEEDS
VERIFICATION** of any usable Rust device-model binding).

**Verdict:** owns-in-Rust ❌ (C), avoids patch ❌ (**is** a fork), maturity of APIs ✅ but
**maintenance risk high**. Keep as the fallback only.

---

## 4. ivshmem (Inter-VM Shared Memory)

**What it is.** An **already-upstream, fixed** QEMU PCI device that exposes a host
shared-memory region to the guest as **BAR2**, plus (in `ivshmem-doorbell` mode via an
`ivshmem-server` chardev) a small register BAR and MSI-X vectors for guest↔guest/host
**doorbell interrupts**
([ivshmem device docs](https://www.qemu.org/docs/master/system/devices/ivshmem.html);
[ivshmem spec](https://www.qemu.org/docs/master/specs/ivshmem-spec.html)). No fork needed.

**Why it's a transport, not a device model.** ivshmem is a *dumb window*: its config space,
BARs and register layout are **fixed by QEMU** — you cannot define arbitrary BARs, arbitrary
config-space behavior, or device-initiated DMA semantics. The "device" can't DMA into guest
RAM the way a real PCI master does; instead guest and host **share one memory region** and you
build your own ring protocol over it, with doorbells for signaling. The guest still needs a
**custom driver** to treat that window as a GPU command channel. It could serve as a
**building-block transport** (e.g. a command/DMA-staging ring) *behind* a real device model,
but on its own it cannot present something a GPU driver would recognize as a GPU.

**Verdict:** owns-in-Rust ✅ (host side is just shared memory + socket), avoids patch ✅, but
**too limited to be the presentation mechanism**. Possible internal transport, not the device
seam.

---

## 5. QEMU TCG plugins

**Dead end for device emulation.** The TCG plugin API is explicitly **passive**: plugins
"are unable to change the system state, only monitor it," querying instructions and config
solely through exported `qemu_plugin_*` functions
([QEMU TCG plugins docs](https://www.qemu.org/docs/master/devel/tcg-plugins.html)). They
register callbacks for instruction/memory events for tracing/analysis — there is **no API to
register a PCI device, a BAR, an IRQ, or MMIO regions**. Worse, plugins hook the **TCG**
(software emulation) translation path; our target is **KVM** hardware acceleration, where TCG
isn't even in play. Cannot be used to add a GPU.

**Verdict:** owns-in-Rust ❌ (C ABI, and irrelevant), avoids patch ✅, but **cannot add
devices at all**. Excluded.

---

## Ranked recommendation

| # | Mechanism | Own in Rust | No QEMU patch | Maturity / risk |
|---|-----------|:---:|:---:|---|
| **1** | **vfio-user out-of-proc PCI server** | ✅ (rust-vmm crate) or C FFI | ✅ client upstream in **QEMU 10.1** | Young client, API not frozen; needs ≥10.1; Rust server less proven than C — but the **only fully-owned arbitrary-PCI + no-fork** path |
| **2 (fallback)** | **Custom in-QEMU PCI device (C)** | ❌ C | ❌ **is a fork** | APIs very mature; **perpetual rebase/maintenance** + all-C GPU |
| 3 | vhost-user-gpu | ✅ (rust-vmm) | ✅ frontends upstream | Locks us into **virtio-gpu** → violates constraint (a); v0.1.0. Reference only |
| 4 | ivshmem | ✅ | ✅ | Fixed dumb window; **transport building-block**, not a device model |
| 5 | TCG plugins | ❌ | ✅ | **Cannot add devices**; KVM-irrelevant. Excluded |

**Build on vfio-user.** It is the one mechanism that satisfies **all three** hard
requirements simultaneously: (a) we own the entire device server (config space, BARs, MSI-X,
DMA) in **our own process, in Rust**; (b) it needs **no QEMU fork** — the client is upstream
as of QEMU 10.1; (c) the guest enumerates a **normal, arbitrary PCI device** we define,
Windows- and Linux-agnostic at the QEMU seam. It also cleanly matches Infinibay's existing
"infinization spawns qemu argv" model — we add one `-device vfio-user-pci` line and run our
Rust GPU-device server alongside each VM, socket in the shared sockets dir.

**Adopt with eyes open.** Pin **QEMU ≥ 10.1** (ideally a post-10.1 commit past Levon's
regression) and treat the QEMU version as a hard dependency of the whole product. Prototype
early against the two performance escape hatches — **mmap-able BAR regions** (shared-memory
fds) and **ioeventfd doorbells** — because a GPU cannot afford a socket round-trip per MMIO;
verify the QEMU 10.1 client actually honors server region-mmap/IO-fds before committing.
Decide server language deliberately: **C libvfio-user** is the most proven server (SPDK), the
**rust-vmm vfio-user crate** keeps us in Rust but needs a real QEMU-interop spike first.

**Fallback is the in-QEMU C device** — only if vfio-user's client maturity or MMIO
performance proves inadequate. It is the most capable and mature device-model surface, but the
price is a maintained QEMU fork in C, which is exactly what this project set out to avoid; take
it only under duress. ivshmem stays in the toolbox as a possible internal shared-memory
transport, never as the device presentation layer.

---

## Sources

- QEMU — vfio-user device docs: https://www.qemu.org/docs/master/system/devices/vfio-user.html
- QEMU — vfio-user protocol spec: https://www.qemu.org/docs/master/interop/vfio-user.html
- John Levon — "vfio-user client in QEMU 10.1" (2025-08-27): https://movementarian.org/blog/posts/2025-08-27-vfio-user-client-in-qemu/
- qemu-devel — vfio-user client patch v3 thread: https://www.mail-archive.com/qemu-devel@nongnu.org/msg1122546.html
- qemu-devel — vfio-user *server* in QEMU RFC (not merged): https://mail-archive.com/qemu-devel@nongnu.org/msg825021.html
- nutanix/libvfio-user README: https://github.com/nutanix/libvfio-user/blob/master/README.md
- rust-vmm/vfio (vfio-bindings, vfio-ioctls, vfio-user crates): https://github.com/rust-vmm/vfio
- Cloud Hypervisor — vfio-user docs: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/vfio-user.md
- QEMU — vhost-user-gpu protocol: https://www.qemu.org/docs/master/interop/vhost-user-gpu.html
- QEMU — VirtIO GPU device docs: https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html
- rust-vmm vhost-device-gpu man page: https://www.mankier.com/1/vhost-device-gpu
- Airbus SecLab — QEMU internals, PCI slave devices: https://airbus-seclab.github.io/qemu_blog/pci_slave.html
- davidv.dev — Learning about PCI-e: emulating a custom device: https://blog.davidv.dev/posts/learning-pcie/
- QEMU — qdev API reference: https://www.qemu.org/docs/master/devel/qdev-api.html
- QEMU — ivshmem device docs: https://www.qemu.org/docs/master/system/devices/ivshmem.html
- QEMU — ivshmem device spec: https://www.qemu.org/docs/master/specs/ivshmem-spec.html
- QEMU — TCG plugins docs: https://www.qemu.org/docs/master/devel/tcg-plugins.html
