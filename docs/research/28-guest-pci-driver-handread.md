# 28 — Guest PCI driver bring-up, verified first-hand against the archive

> Hand-read (directly, by page) from **Madieu, *Mastering Linux Device Driver Development*,
> Ch. 11 "Writing PCI Device Drivers", pp. 535–553** (PDF `9781789342048`). This verifies and
> sharpens the **guest-side** of the device spec (doc 24) with the exact kernel APIs and the required
> call ORDER. Every fact below was read from the book page cited, not summarized by an agent.

## Why this matters for infinigpu

Our vfio-user device (doc 24) makes the guest enumerate a normal PCI device; the guest binds **our
own** Linux DRM driver to it. This is the concrete bring-up that driver's `probe()` must do.

## Verified: driver binding is by VEN/DEV id-match (confirms doc 24)

- The guest driver declares a `struct pci_device_id` table via `PCI_DEVICE(vendor, device)` and
  exports it with `MODULE_DEVICE_TABLE(pci, tbl)`; the PCI core calls `probe()` when a device matches
  by **vendor/product IDs** (or class ID) (p537–538). → our guest DRM driver matches the placeholder
  **VEN `0x1B36` / DEV `0x0100`** from doc 24; class `0x03` is *not* what binds the driver (p537), it
  governs device category / VGA arbitration — exactly doc 24's reasoning for defaulting to `0x038000`.
- `struct pci_driver { name; id_table; probe; remove; suspend/resume/shutdown }` registered with
  `pci_register_driver()` / `module_pci_driver()` (p538–540).

## Verified: the required `probe()` call ORDER (this is the load-bearing sequence)

1. **`pci_enable_device(pdev)`** — must be called before *any* access, even config reads; initializes
   memory + I/O BARs; `_mem`/`_io` variants init only one (p541–542). Ref-counted via `.enable_cnt`.
2. **`pci_set_master(pdev)`** — **MUST be called because our device does DMA** (sets the bus-master
   bit; without it the device cannot initiate DMA transactions to guest RAM) (p542). ← a concrete
   requirement doc 24's DMA path implies but didn't state.
3. **`pci_request_regions(pdev, "infinigpu")`** — claim the BARs (p546–548).
4. **Map BAR0**: `pci_iomap(pdev, 0, 0)` for the whole BAR, **or `pci_iomap_range(pdev, bar, offset,
   maxlen)` to map only a sub-range** (p547) — directly useful for doc 24's split BAR0: the guest can
   map the **direct-mmap fast index page (0x2000)** for at-memory-speed ring head/tail/seqno while the
   trapped control page (0x0000) is touched via `ioread32/iowrite32` (which trap to a vfio-user
   `region_read/write` socket round-trip). Access with `ioread32()/iowrite32()` (p548).
   `pci_resource_start/len/flags(pdev, bar)` give base/size and `IORESOURCE_MEM` vs `IORESOURCE_IO`
   (p545); our BARs are all `IORESOURCE_MEM`.
5. **MSI-X**: `pci_alloc_irq_vectors(pdev, min, max, PCI_IRQ_MSIX)` returns the count allocated
   (≥ min or `-ENOSPC`) (p550–551). In MSI-X mode **`pci_dev->irq` is invalid** — get each vector's
   Linux IRQ with **`pci_irq_vector(pdev, nr)`** (0-based) and `request_irq()` it (p551–552). → doc
   24's "MSI-X vector N ⇒ completion ring N" maps to `request_irq(pci_irq_vector(pdev, N), isr_N, …)`.
6. **Config reads** use `pci_read_config_{byte,word,dword}(dev, offset, &val)`; word/dword auto-convert
   little-endian → CPU endianness (p543, p543 note). Offsets from `include/uapi/linux/pci_regs.h`.

Teardown reverses it: `free_irq` per vector → `pci_free_irq_vectors` → `pci_iounmap` →
`pci_release_regions` → `pci_clear_master` → `pci_disable_device`.

## Two facts that refine doc 24

- **`pci_set_master()` is mandatory** for the guest driver (DMA won't work without it) — add to the
  Phase-0 checklist.
- **`pci_iomap_range()` exists** → the guest can map *just* the fast/doorbell sub-pages of BAR0 with
  the right cache attributes and keep the control page trapped, matching doc 24's per-page access-path
  split without needing separate BARs. (Coherence of the mmap'd page remains the open item from
  doc 24 / MLDDD-2nd p380's `pgprot_noncached` finding — index/doorbell pages want uncached.)

## Sources (hand-read pages)

- Madieu, *Mastering Linux Device Driver Development* (`9781789342048`), Ch. 11: pp. 535–553 —
  `struct pci_device_id`/`pci_driver` (535–540), enable + bus-master (541–542), config access (543),
  BAR mapping `pci_iomap`/`pci_iomap_range`/`pci_resource_*` (544–548), MSI-X `pci_alloc_irq_vectors`/
  `pci_irq_vector` (549–552), INTx assignment (553). Cross-refs doc 24 (device spec) and doc 25.
