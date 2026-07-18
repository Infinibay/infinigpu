# Runtime validation against real QEMU (vfio-user)

Our off-hardware tests validate the device two ways *against our own client*: unit tests drive
`InfinigpuBackend` in-process, and `crates/infinigpu-device/tests/pr4_vfio_user.rs` drives it through
the `vfio_user` crate's `Client` over a socket. This page is the next rung up: driving the device
with **real QEMU's `vfio-user-pci` frontend** (QEMU ≥ 10.1), so the wire protocol is validated
against the actual consumer, not our library.

Run with `scripts/qemu-vfio-user-boot.sh`.

## Smoke — device ↔ real QEMU handshake (no guest OS, unprivileged)

```
scripts/qemu-vfio-user-boot.sh smoke
```

Realizes the device inside QEMU with no bootable media. QEMU's device realization performs the full
vfio-user handshake at startup, which is what this checks. **Verified PASS** on this host
(QEMU 10.1.5 at `/opt/qemu-vfio-user`):

- QEMU enumerated the PCI device — config reads return our identity `0x1b36:0x0110`.
- QEMU mapped the entire guest DMA topology into the device; the device mapped each fd-backed guest
  RAM region **zero-copy** and correctly ignored the fd-less MMIO/BAR windows.
- The device realized cleanly and stayed connected for the whole run.

That covers the device's config space + DMA-map/unmap + region setup against real QEMU. The
`share=on` memfd machine backend is required (vfio-user maps the guest RAM fd for zero-copy DMA);
the device is passed as JSON so the nested `SocketAddress` parses:
`-device '{"driver":"vfio-user-pci","socket":{"type":"unix","path":"<sock>"}}'`.

## Boot — guest `.ko` against the live device (needs a readable kernel)

```
(cd guest/linux && make)                                  # build the module for THIS kernel
sudo install -m0644 /boot/vmlinuz-$(uname -r) /tmp/vmlinuz   # a readable copy (distro image is root-0600)
scripts/qemu-vfio-user-boot.sh boot --kernel /tmp/vmlinuz
```

Boots a minimal busybox initramfs that `insmod infinigpu.ko ring_drainer=1` and dumps the guest
`dmesg`, so the **guest driver's probe** (ring alloc + `CMD_RING_BASE/SIZE/INDEX` programming) and —
once fbcon attaches — the `RESOURCE_FLUSH` present path run against the live device. This is the one
PR4 piece no off-hardware harness covers: the guest *kernel* driving its DMA-coherent ring against a
real device. Use the kernel matching `uname -r` (the `.ko` is built against it) so the module loads.

**The only thing blocking a fully-autonomous run is read access to the distro kernel image**
(`/boot/vmlinuz-*` is mode `0600 root`); the `sudo install` above is the single privileged step.
Everything else — the device server, the initramfs, the QEMU invocation — the script does itself,
and the smoke path proves the device end works with real QEMU.
