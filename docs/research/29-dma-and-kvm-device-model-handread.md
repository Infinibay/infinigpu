# 29 — DMA & KVM device-model, verified first-hand against the archive

> Hand-read (directly, by page) from **Madieu, *Mastering Linux Device Driver Development*, Ch. 11
> "PCI and DMA", pp. 554–566** (`9781789342048`) and **Dakić et al., *Mastering KVM Virtualization*
> 2nd Ed, "QEMU–KVM internals", pp. 64–73** (`9781838828714`). Verifies the **host device model** and
> the **guest ring/DMA** paths of doc 24. Every fact is from the cited page, read directly.

## A. Guest-side DMA — grounds doc 24's command rings (Madieu pp. 554–566)

- **DMA mask:** the guest driver must call `dma_set_mask(dev, DMA_BIT_MASK(64))` for our 64-bit PCIe
  device before any DMA (p557–558). The device receives a **`dma_addr_t` bus address (the IOVA)** for
  each buffer; with an IOMMU the device sees buffers through it (p558).
- **Coherent vs streaming (the decisive choice for us) (p558, p565):**
  - **Coherent** (`dma_alloc_coherent`, wraps `pci_alloc_consistent`): uncached, synchronous — a write
    by CPU or device is immediately visible to the other, no cache-sync needed; expensive; min one
    page; **"to be used for buffers that last the lifetime of the device."** → **our command/control
    RINGS use coherent DMA** (both the guest CPU and our device touch them continuously).
  - **Streaming** (`dma_map_single` / `dma_map_sg`): buffer belongs to the device until unmapped;
    needs `dma_sync_single_for_{cpu,device}()` if the CPU touches it mid-transfer; cheaper per-op,
    for one-shot transfers. → the model for **large resource/blob backing** transfers (though blob +
    udmabuf zero-copy, ADR-0004, mostly removes explicit transfers).
  - Rule of thumb (p565): *"use streaming when you can and coherent when you must."*
- **The ring model, verified concretely (p559–560):** allocate the DMA buffer → **write its
  `dma_addr_t` into a device register via `iowrite32(dma_pa, bar + OFFSET)`** → write size → kick a
  command register → device DMAs → raises an interrupt. This is **exactly** doc 24's flow: the guest
  allocates the ring with `dma_alloc_coherent`, writes the resulting `dma_addr_t` into BAR0's
  `CMD_RING_BASE` (trapped) register, and our vfio-user server reads the ring from the memfd-mapped
  guest RAM at that IOVA.
- **Scatter-gather (p563–564):** `sg_alloc_table` + `sg_set_page` + `dma_map_sg`, then
  `sg_dma_address(sg)`/`sg_dma_len(sg)` per entry — the pattern for backing a guest resource with a
  page list (our `ATTACH_BACKING`, doc 24/ADR-0004).

## B. Host-side device model — grounds doc 24's vfio-user server (Mastering KVM pp. 64–73)

- **Guest RAM is inside the QEMU process address space** (p71) and is registered with KVM via
  `kvm_vm_ioctl(s, KVM_SET_USER_MEMORY_REGION, &mem)` (p67). → **this is *why* our out-of-process
  vfio-user server can `mmap` the guest RAM** (delivered as a memfd over `DMA_MAP`, doc 24) and DMA
  into it with zero copy — the guest's "physical" RAM is just host memory shared by fd.
- **Device access = trap-and-emulate via VM exit** (p70): when the guest touches an emulated device
  register, KVM exits to userspace with **`KVM_EXIT_MMIO`** (or `KVM_EXIT_IO`) and QEMU emulates it.
  → **this is the real cost of doc 24's *trapped* BAR0 control page:** each `iowrite32`/`ioread32` to a
  trapped register is a VM-exit → QEMU → a `region_write`/`region_read` vfio-user socket round-trip.
  It is exactly why doc 24 puts ring head/tail/seqno on the **direct-mmap page** and doorbells on
  **ioeventfd** — both bypass the `KVM_EXIT_MMIO` userspace exit on the hot path. (`KVMState` even
  carries a `coalesced_mmio_ring`, p66, the same family of MMIO-exit-reduction as ioeventfd.)
- **Out-of-process placement:** QEMU is main thread + iothread (`main_loop_wait`) + one thread/vCPU,
  with in-tree device emulation under `hw/` (p71–72). Our device is **not** in `hw/` — it runs in our
  own Rust process over vfio-user, so a device bug can't crash the VMM (matches ADR-0003 isolation).
- **vhost is the in-tree precedent** for this split (doc 25, MKVM p124/624–627): a config channel +
  fd-passed guest memory + eventfd signaling — vfio-user generalizes exactly that shape.

## C. What this refines in doc 24

1. **Ring memory = `dma_alloc_coherent` on the guest** (uncached, lifetime-of-device), its `dma_addr_t`
   written to `CMD_RING_BASE`; large backings use streaming/`dma_map_sg`.
2. **Every trapped BAR register access is a `KVM_EXIT_MMIO`** userspace round-trip — quantifies the cost
   and re-confirms keeping doorbells/indices on the mmap+ioeventfd fast path (doc 24 §BAR0, and the
   open coherence item from doc 28).
3. **`dma_set_mask(DMA_BIT_MASK(64))` is required** on the guest driver (add to the Phase-0 checklist,
   alongside `pci_set_master` from doc 28).

## Sources (hand-read pages)

- Madieu, *Mastering Linux Device Driver Development* (`9781789342048`), Ch. 11: DMA masks (557–558),
  coherent (558–560), streaming + single (561–562), scatter-gather + `dma_sync_*` (563–565), summary (566).
- Dakić, Chirammal, Mukhedkar, Vettathu, *Mastering KVM Virtualization* 2nd Ed (`9781838828714`):
  QEMU-KVM ioctls incl. `KVM_SET_USER_MEMORY_REGION` (67), vCPU/`kvm_run` mmap (68–69), VM-exit reasons
  incl. `KVM_EXIT_MMIO` (70), guest RAM in QEMU address space + threading (71–72), KVM device emulation
  (72–73). Cross-refs doc 24 (device spec), doc 25, doc 28.
