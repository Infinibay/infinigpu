# 25 — Device Mechanics: Book Grounding for the QEMU Device Spec (doc 24) + Guest PCI/DRM Driver

**Scope.** This doc mines the extracted driver books (doc 25 method) for the *register-level,
codeable* mechanics that ground two artifacts: the **QEMU vfio-user device model** (doc 24) and
the **guest PCI/DRM driver** (docs 03/04). It is the archive-grounding layer: every mechanic below
is a real, verifiable kernel API with a page cite, followed by a one-line cross-ref to the part of
the device spec or guest driver it informs. Where the books do **not** cover something (vfio-user
itself, Vulkan external memory, cross-process dma-buf), that is flagged as **web/spec territory**
and handled by doc 24.

**Citation convention.** Page numbers are the extractor's PDF page index (the `===== page N =====`
value from `extract_pdf.py --pages`), so a developer can re-open the exact page. Book short codes:

| Code | Book | ISBN |
|------|------|------|
| **MLDDD** | Mastering Linux Device Driver Development (Madieu) — *primary, PCI ch.* | 9781789342048 |
| **LDDD** | Linux Device Driver Development (Madieu, 2nd) — *mmap/DMA/char* | 9781803240060 |
| **LDDC** | Linux Device Driver Development Cookbook (Giometti) — *mmap/ioctl* | 9781838558802 |
| **LKP2** | Linux Kernel Programming Part 2 (Billimoria) — *IO mem/interrupts* | 9781801079518 |
| **LKP1** | Linux Kernel Programming (Billimoria) — *kernel memory/DMA* | 9781789953435 |
| **MKVM** | Mastering KVM Virtualization | 9781838828714 |

---

## A. PCI config space, BAR discovery & sizing, guest enumeration

The whole enumeration model our guest driver relies on is in **MLDDD Ch11 (PDF p517–556)**.

- **A device is memory-mapped, addressed by BDF, identified by config registers.** A PCI target
  exposes three address spaces — *configuration*, *memory*, and *I/O* — of which config and memory
  are memory-mapped into the CPU address space (MLDDD p518, p524). Config space is 256 B on PCI,
  **4 KB on PCIe**; the first 64 bytes are the standardized header carrying **Vendor ID, Device ID,
  Revision, Class Code, Header Type** (MLDDD p525). Type-0 headers are endpoints, type-1 are bridges
  (MLDDD p520). Devices are located by **Bus:Device:Function** (MLDDD p521).
- **BARs are how a device tells the host how much memory it needs and of what type.** A device has
  **up to six BARs**; each is a window "grabbed from the system memory map, not actual physical RAM,"
  and the BIOS/OS assigns the physical/bus address (MLDDD p526–527). The device's real registers are
  internal and local; the BAR is an indirection into them (MLDDD p527).
- **Guest driver skeleton.** `struct pci_driver { .name, .id_table, .probe, .remove, ... }` matched
  against a `struct pci_device_id[]` table built with `PCI_DEVICE(vendor, device)` and exported via
  `MODULE_DEVICE_TABLE(pci, tbl)`; registered with `pci_register_driver()` / `module_pci_driver()`
  (MLDDD p535–540). In `probe()`: `pci_enable_device()` first (initializes BARs) (MLDDD p541), then
  `pci_set_master()` to **enable bus-mastering = enable DMA** (MLDDD p542).
- **BAR sizing/mapping.** Query a BAR with `pci_resource_start(dev,bar)`, `pci_resource_len(dev,bar)`,
  `pci_resource_flags(dev,bar)` (test `IORESOURCE_MEM` vs `IORESOURCE_IO`); claim + map with
  `pci_request_regions()` then `pci_iomap(dev, bar, 0)` (or `pci_ioremap_bar()`), then access with
  `ioread32()`/`iowrite32()` (MLDDD p544–548). Config registers themselves are read with
  `pci_read_config_dword(dev, PCI_VENDOR_ID, &v)` etc.; offsets live in `uapi/linux/pci_regs.h`
  (MLDDD p543–544).

> **Cross-ref → doc 24:** this *is* the register/BAR layout the device model must present. In
> vfio-user the client enumerates exactly these via **`VFIO_USER_DEVICE_GET_REGION_INFO`** (region
> index, size, `FLAG_READ/WRITE/MMAP`) and reads config space via **`VFIO_USER_REGION_READ/WRITE`**;
> the server must serve a synthetic config header (our Vendor/Device ID, class = display controller)
> and BAR sizes. Doc 24 §"BAR map" defines our layout: **BAR0 = MMIO doorbell/register block**,
> **BAR2 = the ring/queue region (mmap-able, sparse)**. **Cross-ref → guest driver (doc 04):** the
> Linux DRM driver's `probe()` follows the MLDDD skeleton verbatim; the Windows WDDM miniport does
> the equivalent through `DxgkDdiStartDevice`/BAR resource-list translation (NEEDS VERIFICATION —
> not covered by these Linux books).

---

## B. Mapping BAR/device memory into the kernel and out to userspace

Two mappings matter: (1) BAR → kernel virtual (driver's own register access), and (2) BAR/buffer →
userspace (so the guest *renderer* can write commands into a ring without a syscall per submit).

- **BAR → kernel.** `request_mem_region()` reserves, `ioremap()` builds page tables and returns a
  `__iomem` cookie; managed `devm_ioremap()` preferred (LDDD p372–374). `ioremap` "does not allocate
  memory but returns a special virtual address" — pure address-space mapping (LDDD p373). Corroborated
  by **LKP2 Ch3 "Working with Hardware I/O Memory" (p136)**.
- **BAR/buffer → userspace via the `mmap` file operation.** The driver implements
  `int (*mmap)(struct file *filp, struct vm_area_struct *vma)` (LDDD p381). The kernel hands it a
  pre-built VMA; the callback calls **`remap_pfn_range(vma, vma->vm_start, pfn, size, vma->vm_page_prot)`**
  which "updates the VMA and derives the kernel's PTE... so the kernel and user space both point to the
  same physical memory region" with **no copy** (LDDD p376). The PFN is derived per allocation origin:
  `virt_to_phys(k)>>PAGE_SHIFT` for kmalloc, `page_to_pfn(page)` for `alloc_pages`,
  `vmalloc_to_pfn()` for vmalloc (LDDD p377). Flags of interest: **`VM_IO`, `VM_PFNMAP`** (raw PFN, no
  backing `struct page` — the normal case for device memory), `VM_DONTCOPY/DONTEXPAND/DONTDUMP`
  (LDDD p377). For **I/O memory** specifically use **`io_remap_pfn_range()`** or the simplified
  **`vm_iomap_memory(vma, start, len)`** (LDDD p379–380).
- **Coherency/caching is a correctness issue, not a perf nicety.** By default the kernel maps to
  userspace *cached*; for a device that must see writes immediately, set
  **`vma->vm_page_prot = pgprot_noncached(vma->vm_page_prot)`** — the author measured **~20 ms of
  register-visibility latency with caching vs <200 µs without** (LDDD p380). Full 6-step `mmap`
  implementation with the offset/size checks at LDDD p382.
- **Concrete userspace shape.** LDDC shows the end-to-end pattern: a char device whose `chrdev_fops`
  wires `.mmap = chrdev_mmap`, and a userspace program that `open()`s `/dev/...` then `mmap()`s to get
  a shared buffer address it writes through directly (LDDC p227–230, p328). LKP2 Ch2 "User-Kernel
  Communication Pathways" (p69) covers the same mmap pathway.

> **Cross-ref → doc 24:** the **command rings live in a BAR the client mmaps** — in vfio-user this is a
> region with `VFIO_REGION_INFO_FLAG_MMAP` (and `VFIO_REGION_INFO_CAP_SPARSE_MMAP` to expose only the
> ring subrange). The `pgprot_noncached` finding is the direct evidence for doc 24's rule that
> **doorbell/head-tail index pages must be uncached (or explicitly flushed)** so the host device sees
> producer-index updates without a stale-cache stall. **Cross-ref → guest DRM UAPI (doc 04):** our
> `DRM_INFINIGPU_MMAP_RING` / GEM-object `mmap` handler is a `remap_pfn_range`/`vm_iomap_memory` call
> over BAR2 — the guest renderer maps its per-context ring the way LDDC's test program maps `/dev/cdev`.

---

## C. DMA — coherent vs streaming, scatter-gather, dma_addr_t/IOVA, IOMMU, barriers

DMA is the model for how large payloads (vertex/texture uploads, framebuffers) move without copying.
**MLDDD p557–565** is the anchor.

- **Bus address vs IOVA.** DMA buffers are handed to the device as **`dma_addr_t` bus addresses**,
  which "I/O devices view through the lens of the bus controller and any intervening **IOMMU**"
  (MLDDD p557). Declare width with `dma_set_mask(dev, DMA_BIT_MASK(64))` (MLDDD p557). This is exactly
  the guest-physical-vs-IOVA distinction the device model must honor.
- **Coherent (consistent) mapping.** `dma_alloc_coherent(dev, size, &dma_handle, GFP_KERNEL)` returns
  a CPU virtual address **and** a bus address for **uncached, unbuffered** memory — "a write by either
  the device or the CPU can be immediately read by either without worrying about cache coherency"
  (MLDDD p558). Minimum one page, power-of-two order; use it for **buffers that last the device's
  lifetime** (MLDDD p559). (`pci_alloc_consistent` = `dma_alloc_coherent` with `GFP_ATOMIC`.) Also
  noted in **LKP1 p498–499 "A word on DMA and CMA."**
- **Streaming mapping.** `dma_map_single(dev, ptr, size, dir)` maps an already-allocated buffer; the
  buffer **belongs to the device until `dma_unmap_single()`** — the map cleans/invalidates caches, and
  the CPU must not touch it until unmap (MLDDD p560–561). Direction (`DMA_TO_DEVICE`/`FROM_DEVICE`/
  `BIDIRECTIONAL`) must match real data flow (MLDDD p561). Cheap to run, more code discipline.
- **Scatter-gather.** Build a `struct scatterlist[]` (`sg_init_table`, `sg_set_page`), call
  `dma_map_sg(dev, sgl, nents, dir)` to map **multiple non-contiguous page-sized buffers in one shot**;
  read back per-entry bus address/len with `sg_dma_address(sg)` / `sg_dma_len(sg)` (MLDDD p563–564).
  Entries must be page-sized except the last (MLDDD p564).
- **Explicit coherency barriers between transfers.** When the CPU needs to peek at a streaming buffer
  mid-flight, bracket with **`dma_sync_single_for_cpu()` / `dma_sync_single_for_device()`** (or the
  `_sg_` variants) (MLDDD p565). Rule of thumb: "streaming when you can, coherent when you must"
  (MLDDD p565).

> **Cross-ref → doc 24:** the vfio-user client publishes guest RAM to the server with
> **`VFIO_USER_DMA_MAP`** (address/size + a passed **memfd** fd for zero-copy) and revokes with
> `VFIO_USER_DMA_UNMAP`; the server DMAs into that shared mapping (or falls back to
> `VFIO_USER_DMA_READ/WRITE`). Doc 24's **`memory-backend-memfd,share=on`** decision is the coherent-
> mapping analog: one shared region both sides read/write without a copy. **The `dma_sync_*` +
> `pgprot_noncached` evidence grounds doc 24 §"ring memory ordering":** producer writes payload, then
> a release barrier, then bumps the ring index; the host issues an acquire barrier before reading — the
> books show *why* (cache ownership handoff), the wire spec must state the exact barrier placement
> (**web/spec territory** — the books do not cover multi-ring lock-free ordering).

---

## D. MSI/MSI-X allocation, ISR, and "MSI carries no payload"

This is the load-bearing insight for the completion path.

- **Allocation.** `pci_alloc_irq_vectors(dev, min_vecs, max_vecs, flags)` with `PCI_IRQ_MSIX`
  (or `PCI_IRQ_ALL_TYPES` to fall back MSI-X → MSI → legacy) enables the capability and allocates
  vectors; get the Linux IRQ number for vector *n* with `pci_irq_vector(dev, n)` and hand it to
  `request_irq()` (MLDDD p550–552). **MSI-X supports 64–2048 vectors, each with its own address/data
  pair** (MLDDD p530–531), so we can dedicate one vector per command ring. `request_irq()` / managed
  `devm_request_irq()` and ISR authoring are detailed in **LKP2 Ch4 (p164–209)**: `request_irq()`
  p168, interrupt flags/level-vs-edge p173–174, ISR guidelines p179, managed IRQ p188, threaded
  interrupts p190.
- **MSI carries NO payload (the key fact).** MLDDD is explicit: *"the data that is sent as part of the
  memory write transaction is exclusively used by the chipset (the root complex) to determine which
  interrupt to trigger on which processor; that data is not available for the device to communicate
  additional information to the interrupt handler"* (MLDDD p529). An MSI is a bare edge — a doorbell,
  not a message.

> **Cross-ref → doc 24 + doc 14 (fence/sync):** because the interrupt itself carries no data, **the
> completion information must live in shared memory** — a per-ring **completion queue / seqno slot in
> BAR2**. The device fires MSI-X vector *k* purely to wake the guest; the guest ISR then reads the ring
> tail + seqno to learn *what* completed. In vfio-user the server raises this via
> **`VFIO_USER_DEVICE_SET_IRQS`** (client associates an **eventfd** per vector; server writes the
> eventfd to signal). Doc 24 §"completion" = "MSI-X vector N ⇒ read completion ring N's seqno," which
> is the register-level restatement of MLDDD p529. **Cross-ref → guest driver:** the DRM driver's
> seqno→`dma_fence` signal path and the WDDM `DxgkDdiNotifyInterrupt`/`DxgkDdiInterruptRoutine` +
> DPC both consume the ring, never the interrupt payload.

---

## E. ioctl + char-device `file_operations` UAPI shape (informs the DRM UAPI)

Our guest DRM driver's ioctl surface is a specialization of the generic char-device pattern.

- **The `file_operations` table.** `struct file_operations { .owner, .open, .release, .read, .write,
  .llseek, .unlocked_ioctl, .compat_ioctl, .mmap }` (LDDC p227, p325). `unlocked_ioctl` has signature
  `long (*)(struct file *, unsigned int cmd, unsigned long arg)` (LDDC p325); `compat_ioctl` is the
  32-on-64 shim. The method is added to the existing chrdev fops and dispatches on `cmd` (LDDC p220).
  Madieu's 2nd edition covers the same `ioctl` method and char-device registration in **LDDD Ch4
  (p162–193, ioctl at p189)**.
- **Command encoding.** ioctl command numbers are built with the `_IO/_IOR/_IOW/_IOWR(type, nr, size)`
  macros so the direction and argument size are self-describing (NEEDS VERIFICATION of exact page —
  standard UAPI convention; LDDC/LDDD present `cmd`/`arg` dispatch without belaboring the macros).

> **Cross-ref → guest DRM UAPI (doc 04):** DRM ioctls are exactly this `unlocked_ioctl` mechanism with
> a fixed `DRM_IOCTL_*` numbering and per-driver `DRM_IOCTL_INFINIGPU_*` (GEM create, context create,
> submit, wait-seqno). The books ground the *mechanism* (fops table, cmd/arg dispatch, mmap coexisting
> in the same fops); the DRM-specific `drm_ioctl` demux and GEM handle semantics are **DRM-subsystem
> territory** the books don't cover — see doc 04. The Windows equivalent (D3DKMT escapes / IddCx) is
> **web/spec territory**.

---

## F. How QEMU/KVM builds guest memory + the vhost handshake — the out-of-process device model

**MKVM Ch2 "QEMU–KVM internals" (p64–72)** is the template for why an out-of-process device works at
all, and vhost is the closest in-tree precedent for our split.

- **Guest RAM lives in the QEMU process address space.** *"The guest RAM is assigned inside the QEMU
  process's virtual address space... the physical RAM of the guest is inside the QEMU process address
  space"* (MKVM p71). QEMU registers it into KVM with **`kvm_vm_ioctl(s, KVM_SET_USER_MEMORY_REGION,
  &mem)`** (MKVM p67) after `KVM_CREATE_VM` (p67–68); vCPUs are POSIX threads each running
  `kvm_vcpu_ioctl(cpu, KVM_RUN, 0)` (MKVM p68–69).
- **The trap-and-emulate loop is the device seam.** When guest code touches an emulated device
  register, KVM exits back to userspace with **`KVM_EXIT_MMIO`** (or `KVM_EXIT_IO`); QEMU emulates the
  access and re-enters `KVM_RUN` (MKVM p70). A device model *is* a handler for these exits over a
  region of the shared guest address space. Threading: one main `iothread` event loop + worker/IO
  threads + one thread per vCPU (MKVM p71–72).
- **vhost = config channel + shared-memory datapath, offloaded out of the main emulator.** The vhost
  control interface is a **char device `/dev/vhost-net`** used to *configure* an instance (MKVM p124);
  the actual virtqueue processing runs elsewhere (in-kernel `vhost_net`), which "reduces copy
  operations, lowers latency and CPU usage" with **no change to the guest frontend driver**
  (MKVM p624–625), and supports multi-queue (`queues='M'`, up to 8) (MKVM p627).

> **Cross-ref → doc 24 (the out-of-process device):** vfio-user is the *generalization* of this exact
> split — **the device is a separate process, given the guest's memory via fd-passing
> (`VFIO_USER_DMA_MAP`), configured over a control socket (`VFIO_USER_REGION_READ/WRITE`,
> `GET_REGION_INFO`), and signaling completions via eventfd (`SET_IRQS`)** — precisely the vhost
> pattern (config channel + shared RAM + offloaded datapath) but for an arbitrary PCI device instead of
> only virtio-net. MKVM p71's "guest RAM in the QEMU process address space" is why
> `memory-backend-memfd,share=on` lets our external server mmap guest pages for zero-copy DMA. The
> multi-queue vhost model (p627) is the ancestor of our **N per-context command rings**. **The books
> stop here:** vfio-user's actual message framing, capability negotiation, and the libvfio-user/
> rust-vmm server API are **web/spec territory** (doc 24 sources below).

---

## Honest gaps — what the books do NOT ground (all web/spec territory, → doc 24)

- **vfio-user itself.** No book in the corpus mentions vfio-user, libvfio-user, or the rust-vmm
  `vfio` crate. The protocol messages, socket framing, and server lifecycle are 100% from the qemu.org
  spec / libvfio-user sources. The books ground only the *shapes* vfio-user re-exposes (regions≈BARs,
  DMA_MAP≈dma_alloc, SET_IRQS≈MSI-X).
- **Cross-process / cross-API buffer sharing.** `dma-buf`, `memfd` fd-passing between the guest driver,
  the host replay process, and the GPU, and **Vulkan external memory** (`VK_KHR_external_memory_fd` /
  Win32 handles) are not in these books — they are the host-side data-plane spec (docs 06/20) and web.
- **Multi-ring lock-free ordering & fences.** The books give cache-ownership handoff (`dma_sync_*`,
  `pgprot_noncached`) but not lock-free SPSC ring ordering, seqno timelines, or `dma_fence`/`sync_file`
  wiring — see doc 14 and Vulkan/DRM docs.
- **Windows guest side.** WDDM/IddCx/D3DKMT DDIs are absent (Linux-only corpus) — docs 03/08/15.

---

## Sources

**doc 25 — archived books (extractor PDF page indices, verify with `extract_pdf.py <isbn>.pdf --pages N`):**

- **MLDDD** *Mastering Linux Device Driver Development* (9781789342048), Ch11 "Writing PCI Device
  Drivers": PCI/PCIe & address spaces p517–527; interrupt distribution & **MSI-no-payload p529**;
  MSI-X p530–531; `struct pci_dev`/`pci_device_id`/`pci_driver` p533–540; `pci_enable_device`/
  `pci_set_master`/config access p541–544; BAR map (`pci_resource_*`, `pci_iomap`, `ioread/iowrite`)
  p544–548; **`pci_alloc_irq_vectors`/`pci_irq_vector` p550–552**; DMA (coherent/streaming/SG/sync)
  p557–565.
- **LDDD** *Linux Device Driver Development*, 2nd (9781803240060): I/O memory & `ioremap` p369–374;
  **`remap_pfn_range`/`io_remap_pfn_range`/`vm_iomap_memory` p376–380**; `pgprot_noncached` latency
  p380; **`mmap` file-op implementation p381–383**; char device + ioctl Ch4 p162–193; DMA Ch11
  p384–396.
- **LDDC** *LDD Cookbook* (Giometti): `unlocked_ioctl` how-to p220; **`file_operations` table with
  `.mmap`/`.unlocked_ioctl` p227, p325**; userspace `mmap` test program p227–230, p328.
- **LKP2** *Linux Kernel Programming Part 2* (9781801079518): Hardware I/O Memory Ch3 p136;
  **Handling Hardware Interrupts Ch4 — `request_irq` p168, flags p173, ISR p179, managed IRQ p188,
  threaded p190**; user-kernel pathways Ch2 p69.
- **LKP1** *Linux Kernel Programming* (9781789953435): DMA/CMA & `dma_alloc_coherent` p498–499.
- **MKVM** *Mastering KVM Virtualization* (9781838828714): **QEMU–KVM internals p64–72** —
  `/dev/kvm`, `KVM_CREATE_VM`, **`KVM_SET_USER_MEMORY_REGION` p67**, `KVM_RUN`/`KVM_EXIT_MMIO` p68–70,
  **guest RAM in QEMU address space p71**; vhost char-dev `/dev/vhost-net` p124; `vhost_net`
  offloaded datapath & multi-queue p624–627.

**doc 24 — device seam (web/spec, cross-referenced above):**

- vfio-user Protocol Specification — QEMU docs: <https://www.qemu.org/docs/master/interop/vfio-user.html>
  (`VFIO_USER_DEVICE_GET_INFO`, `GET_REGION_INFO` incl. `FLAG_MMAP`/`CAP_SPARSE_MMAP`,
  `REGION_READ/WRITE`, `DMA_MAP/UNMAP` + fd passing, `DMA_READ/WRITE`, `GET_IRQ_INFO`, `SET_IRQS`).
- VFIO — Linux Kernel docs: <https://docs.kernel.org/driver-api/vfio.html>
- Linux UAPI `vfio.h` (region/IRQ structs): <https://github.com/torvalds/linux/blob/master/include/uapi/linux/vfio.h>
