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

echo ">> building a minimal initramfs (busybox + infinigpu.ko)"
IR="$WORK/initramfs"
mkdir -p "$IR"/{bin,proc,sys,dev}
cp "$BUSYBOX" "$IR/bin/busybox"
for a in sh mount insmod dmesg sleep poweroff cat grep; do ln -sf busybox "$IR/bin/$a"; done
cp "$KO" "$IR/infinigpu.ko"
cat >"$IR/init" <<EOF
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null
echo "GUEST: loading infinigpu.ko (ring_drainer=$RING_DRAINER)"
insmod /infinigpu.ko ring_drainer=$RING_DRAINER || echo "GUEST: insmod FAILED"
sleep 1
echo "==== INFINIGPU DMESG ===="
dmesg | grep -i infinigpu || echo "GUEST: no infinigpu dmesg lines"
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
