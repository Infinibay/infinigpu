#!/usr/bin/env bash
# Runtime validation of the infinigpu vfio-user device against REAL QEMU (not our own vfio_user
# Client). Two modes:
#
#   smoke  (default) — realize the device in QEMU with no guest OS. Proves the full vfio-user
#                      handshake: PCI config enumeration + the guest DMA topology mapped zero-copy.
#                      Needs NO kernel; runs unprivileged. This is what tests/pr4_vfio_user.rs
#                      exercises against our client — here it's the *real QEMU vfio-user-pci frontend*.
#
#   boot   (--kernel <vmlinuz>) — boot a minimal busybox initramfs that insmods infinigpu.ko and
#                      dumps the guest dmesg, so the GUEST driver's probe (ring alloc + register
#                      programming) and, with fbcon, the flush path run against the live device.
#                      This is the one PR4 piece no off-hardware harness covers. It needs a READABLE
#                      guest kernel; the distro image is usually root-0600, so provide a copy, e.g.:
#                          sudo install -m0644 /boot/vmlinuz-$(uname -r) /tmp/vmlinuz
#                          scripts/qemu-vfio-user-boot.sh boot --kernel /tmp/vmlinuz
#                      Use the SAME kernel the .ko was built against (uname -r) so the module loads.
#
# Requires: a QEMU with vfio-user-pci support (QEMU >= 10.1; e.g. /opt/qemu-vfio-user), /dev/kvm,
#           busybox (static), cpio, gzip.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
QEMU="${QEMU_BIN:-/opt/qemu-vfio-user/bin/qemu-system-x86_64}"
BUSYBOX="${BUSYBOX:-/usr/bin/busybox}"
WORK="${WORK_DIR:-$(mktemp -d)}"
mkdir -p "$WORK"
SOCK="$WORK/igpu.sock"
SRV_LOG="$WORK/server.log"
MEM_MB="${MEM_MB:-256}"
RING_DRAINER="${RING_DRAINER:-1}"   # boot mode: infinigpu.ring_drainer=

MODE="${1:-smoke}"; shift || true
KERNEL=""
while [ $# -gt 0 ]; do
  case "$1" in
    --kernel) KERNEL="$2"; shift 2;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done

die() { echo "!! $*" >&2; exit 1; }
command -v "$QEMU" >/dev/null || die "$QEMU not found (set QEMU_BIN=; needs vfio-user-pci, QEMU>=10.1)"
"$QEMU" -device vfio-user-pci,help >/dev/null 2>&1 || die "$QEMU has no vfio-user-pci device"

echo ">> building the device server"
( cd "$ROOT" && cargo build --quiet --bin infinigpu-device )
SERVER="$ROOT/target/debug/infinigpu-device"

echo ">> starting the vfio-user device server on $SOCK"
RUST_LOG="${RUST_LOG:-info}" "$SERVER" --socket "$SOCK" --vm-id qemu-validate >"$SRV_LOG" 2>&1 &
SRV_PID=$!
cleanup() { kill "$SRV_PID" 2>/dev/null || true; }
trap cleanup EXIT
for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { cat "$SRV_LOG"; die "server never bound the socket"; }

# The share=on memfd backend is REQUIRED: vfio-user maps the guest RAM fd into the device for
# zero-copy DMA. The device is given as JSON so the nested SocketAddress parses.
qemu_common=(
  -machine "q35,memory-backend=mem0" -m "$MEM_MB"
  -object "memory-backend-memfd,id=mem0,size=${MEM_MB}M,share=on"
  -display none -no-reboot
  -device "{\"driver\":\"vfio-user-pci\",\"socket\":{\"type\":\"unix\",\"path\":\"$SOCK\"}}"
)
[ -e /dev/kvm ] && qemu_common+=(-enable-kvm -cpu host)

if [ "$MODE" = smoke ]; then
  echo ">> smoke: realizing the device in QEMU (no guest OS; 8s)"
  timeout 8 "$QEMU" "${qemu_common[@]}" -serial null -monitor none 2>"$WORK/qemu.log" || true
  echo "---- device server saw ----"
  grep -E "config read @0x00|guest RAM mapped zero-copy|Connection (closed|established)" "$SRV_LOG" | head -8 || true
  maps=$(grep -c "guest RAM mapped zero-copy" "$SRV_LOG" || true)
  ident=$(grep -c "0x1b36:0x0110" "$SRV_LOG" || true)
  echo "---- result ----"
  if [ "${ident:-0}" -ge 1 ] && [ "${maps:-0}" -ge 1 ]; then
    echo "PASS: real QEMU enumerated the device (0x1b36:0x0110) and mapped guest RAM ($maps regions) over vfio-user."
  else
    cat "$WORK/qemu.log"; die "handshake not observed (ident=$ident maps=$maps) — see $SRV_LOG"
  fi
  exit 0
fi

[ "$MODE" = boot ] || die "unknown mode '$MODE' (use: smoke | boot --kernel <vmlinuz>)"
[ -n "$KERNEL" ] || die "boot mode needs --kernel <vmlinuz> (a READABLE copy; see the header)"
[ -r "$KERNEL" ] || die "kernel not readable: $KERNEL"
[ -x "$BUSYBOX" ] || die "need a static busybox (set BUSYBOX=)"
KO="$ROOT/guest/linux/infinigpu.ko"
[ -r "$KO" ] || die "build the guest module first: (cd guest/linux && make)"

echo ">> building a minimal initramfs (busybox + infinigpu.ko + DRM deps)"
IR="$WORK/initramfs"
mkdir -p "$IR"/{bin,proc,sys,dev,lib}
cp "$BUSYBOX" "$IR/bin/busybox"
for a in sh mount insmod dmesg sleep poweroff cat grep; do ln -sf busybox "$IR/bin/$a"; done
cp "$KO" "$IR/infinigpu.ko"

# infinigpu.ko depends on out-of-kernel-module DRM helpers (drm_dma_helper etc.); the rest of DRM
# is built into this kernel. Resolve infinigpu's module deps + their modules.dep closure, decompress
# each into the initramfs, and record the load order (deepest deps first). Only decompressible
# (readable) modules are included — the distro tree is world-readable even when vmlinuz is not.
MODDIR="/usr/lib/modules/$(uname -r)"
DEPFILE="$MODDIR/modules.dep"
declare -A SEEN=()
LOAD_ORDER=()
resolve() {  # recursively append $1's deps then $1 to LOAD_ORDER (dedup)
  local m="$1"
  [ -n "${SEEN[$m]:-}" ] && return
  SEEN[$m]=1
  local line rel deps d
  line="$(grep -E "/(${m})\.ko(\.zst)?:" "$DEPFILE" | head -1 || true)"
  rel="${line%%:*}"
  deps="${line#*:}"
  for d in $deps; do resolve "$(basename "$d" | sed 's/\.ko.*//')"; done
  [ -n "$rel" ] && LOAD_ORDER+=("$MODDIR/$rel")
}
for dep in $(modinfo "$KO" 2>/dev/null | sed -n 's/^depends: *//p' | tr ',' ' '); do
  resolve "$dep"
done
LOAD_LINES=""
for zf in "${LOAD_ORDER[@]}"; do
  base="$(basename "$zf" | sed 's/\.ko.*//')"
  if zstd -dcq "$zf" >"$IR/lib/$base.ko" 2>/dev/null; then
    LOAD_LINES="${LOAD_LINES}insmod /lib/$base.ko || echo \"GUEST: dep $base failed\""$'\n'
    echo "   + dep module: $base"
  fi
done

cat >"$IR/init" <<EOF
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null
echo "GUEST: loading DRM dep modules"
${LOAD_LINES}
echo "GUEST: loading infinigpu.ko (ring_drainer=$RING_DRAINER)"
insmod /infinigpu.ko ring_drainer=$RING_DRAINER || echo "GUEST: insmod FAILED"
sleep 1
echo "GUEST: forcing framebuffer flips (exercises the present/RESOURCE_FLUSH path)"
if [ -e /dev/fb0 ]; then
  # Interleave fb writes with waits so the drm_fbdev_dma deferred-IO worker flushes the dirty
  # pages (→ igpu_flush_damaged → RESOURCE_FLUSH) before we power down.
  for n in 1 2 3 4 5; do
    dd if=/dev/zero of=/dev/fb0 bs=8192 count=64 conv=notrunc 2>/dev/null
    dd if=/dev/urandom of=/dev/fb0 bs=8192 count=64 conv=notrunc 2>/dev/null
    sync; sleep 1
  done
else
  echo "GUEST: no /dev/fb0"
fi
sleep 2
echo "==== INFINIGPU DMESG ===="
dmesg | grep -iE "infinigpu|drm" | tail -40 || echo "GUEST: no infinigpu dmesg lines"
echo "==== DRI NODES ===="
ls -l /dev/dri /dev/fb0 2>/dev/null || echo "(no /dev/dri)"
echo "==== END ===="
poweroff -f
EOF
chmod +x "$IR/init"
( cd "$IR" && find . | cpio -o -H newc 2>/dev/null | gzip -9 >"$WORK/initramfs.cpio.gz" )

echo ">> booting the guest against the live device (serial → stdout; 40s cap)"
timeout 40 "$QEMU" "${qemu_common[@]}" \
  -kernel "$KERNEL" -initrd "$WORK/initramfs.cpio.gz" \
  -append "console=ttyS0 quiet" -serial mon:stdio 2>>"$WORK/qemu.log" | tee "$WORK/guest.log" || true

echo "---- result ----"
if grep -qiE "INFINIGPU-KMS: registered|infinigpu magic=" "$WORK/guest.log"; then
  echo "PASS: guest probed the live device."
  grep -iE "PR4 ring drainer enabled|INFINIGPU-KMS: registered|infinigpu " "$WORK/guest.log" | head
else
  echo "INCOMPLETE: no infinigpu probe line in the guest log. Server side:"; tail -5 "$SRV_LOG"
  echo "(check the kernel matches uname -r so the module loads; see $WORK/guest.log)"
fi
