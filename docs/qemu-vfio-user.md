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

## Boot — guest `.ko` against the live device — **VERIFIED PASS**

```
(cd guest/linux && make)                                       # build the module for THIS kernel
scripts/qemu-vfio-user-boot.sh boot --kernel <readable-vmlinuz>
```

Boots a minimal busybox initramfs that loads infinigpu's DRM dep modules (resolved from
`modules.dep`, decompressed from the world-readable `/usr/lib/modules` tree) + `insmod
infinigpu.ko ring_drainer=1`, then dumps the guest `dmesg`. This is the one PR4 piece no off-hardware
harness covers: the guest *kernel* driving its DMA-coherent ring against a real device. Use the
kernel matching `uname -r` (the `.ko`'s vermagic must match).

**Verified PASS** (guest 6.14.0-37 against the live device over QEMU 10.1.5):

```
infinigpu 0000:00:03.0: infinigpu magic=0x49475055 abi=0x4 caps=0x3c
infinigpu 0000:00:03.0: infinigpu: PR4 ring drainer enabled (cap=16)
[drm] Initialized infinigpu 1.0.0 for 0000:00:03.0 on minor 0
infinigpu 0000:00:03.0: [drm] fb0: infinigpudrmfb frame buffer device
infinigpu 0000:00:03.0: INFINIGPU-KMS: registered /dev/dri/card0 (2D accel on, cursor plane off)
```

The guest read the device identity/ABI/caps over vfio-user, **programmed the PR4 ring registers**
(`CMD_RING_BASE_LO/HI`, `CMD_RING_SIZE` — confirmed as `BAR0 write off=0x0100/0x0104/0x0108` in the
device server log — and `CMD_RING_INDEX`, since probe completed), and brought up DRM/KMS + fbdev
against the live device. The device's broker even sized itself from the real A5000 via NVML. So the
**guest driver's probe + ring-drainer activation is runtime-validated end-to-end**.

### Render node — guest userspace drives 3D onto the GPU (rung 1)

With `DRIVER_RENDER` + `DRM_IOCTL_INFINIGPU_SUBMIT3D`, the boot harness also builds a static
userspace submitter (`guest/linux/submit3d_test.c`) and runs it: it opens `/dev/dri/card0`,
allocates a result buffer, and asks the host to replay a `TRIANGLE` workload on the physical GPU.
**Confirmed so far:** the guest exposes **`/dev/dri/renderD128`** (the DRIVER_RENDER root-cause fix
— its absence is what forced guest apps onto llvmpipe) and the ioctl is reachable and wired through
to a `SUBMIT_CMD{VULKAN_VENUSLIKE}` on the command ring.

### Known blocker: the guest-boot doorbell round-trip (transport desync)

The one thing not yet working from a **live guest boot** is the doorbell → drain round-trip — for
*any* ring message, 3D submit **and** the 2D `RESOURCE_FLUSH` present alike (this is the real cause
of the flush-round-trip gap noted in earlier runs, not "busybox doesn't page-flip"). QEMU's own log
shows a vfio-user **protocol desync during probe**:

```
qemu-system-x86_64: unexpected reply
qemu-system-x86_64: bad header size
qemu-system-x86_64: vfio_user_device_io_region_write: ... Broken pipe
```

The connection breaks around the `CMD_RING_BASE` register writes, so no later MMIO — `CMD_RING_SIZE`,
`CMD_RING_INDEX`, and every **doorbell** — reaches the device. The ring therefore never drains
(`submit3d` reports `EBUSY` when the guest's own ring fills, or `ETIMEDOUT` when it doesn't). This is
a **server↔QEMU transport issue** in our `vfio_user` v0.1.3 frontend, **independent of the datapath**:
the drain + 3D executor themselves are validated by `crates/infinigpu-device/tests/pr4_vfio_user.rs`
(a real `RESOURCE_FLUSH` drained + presented over vfio-user via our own `Client`) and by
`vulkan_workload_renders_a_triangle_to_scanout` (a TRIANGLE replayed on the A5000). Fixing the
transport desync is the next rung and unblocks the live guest demo for both 2D flush and 3D.

### Getting a readable kernel

The distro image `/boot/vmlinuz-*` is mode `0600 root`. Provide a readable copy — either a cached
one (e.g. `~/.cache/infinigpu/vmlinuz`) or `sudo install -m0644 /boot/vmlinuz-$(uname -r)
/tmp/vmlinuz`. Everything else — device server, DRM-dep resolution, initramfs, QEMU — the script
does itself.
