#!/usr/bin/env bash
# Phase-0 Step-3 (part 1): boot a REAL Linux guest kernel under our QEMU with the
# infinigpu device attached, and confirm the guest kernel enumerates it on the PCI
# bus (1b36:0110, class Display). Uses the host's own kernel + a tiny busybox
# initramfs — no distro image needed. This is the first proof that the whole
# device ↔ QEMU ↔ guest-kernel path works with an actual OS.
set -euo pipefail
cd "$(dirname "$0")/.."

QEMU="${QEMU:-/opt/qemu-vfio-user/bin/qemu-system-x86_64}"
KREL="$(uname -r)"
BUSYBOX="$(command -v busybox)"
# The distro kernel is root-only readable; use a readable copy. Create it once with:
#   sudo install -m0644 /boot/vmlinuz-$(uname -r) ~/.cache/infinigpu/vmlinuz
KCOPY="${KERNEL:-$HOME/.cache/infinigpu/vmlinuz}"
if [[ -r "/boot/vmlinuz-${KREL}" ]]; then
    VMLINUZ="/boot/vmlinuz-${KREL}"
elif [[ -r "$KCOPY" ]]; then
    VMLINUZ="$KCOPY"
else
    echo "!! No readable kernel. Run this once (needs sudo), then re-run:"
    echo "     mkdir -p ~/.cache/infinigpu && sudo install -m0644 /boot/vmlinuz-${KREL} $KCOPY"
    exit 1
fi
[[ -x "$QEMU" ]]    || { echo "!! $QEMU missing (run build-qemu-vfio-user.sh)"; exit 1; }
[[ -x "$BUSYBOX" ]] || { echo "!! busybox missing"; exit 1; }

cargo build -q -p infinigpu-device
DEV=target/debug/infinigpu-device

WORK="$(mktemp -d /tmp/infinigpu-guest-XXXXXX)"
SOCK="$(mktemp -u /tmp/infinigpu-guest-XXXXXX.sock)"
trap 'kill ${DEVPID:-0} 2>/dev/null || true; rm -rf "$WORK" "$SOCK"' EXIT

# ---- build a minimal busybox initramfs ----
IR="$WORK/ir"
mkdir -p "$IR"/{bin,proc,sys,dev}
cp "$BUSYBOX" "$IR/bin/busybox"
cat >"$IR/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox --install -s /bin
export PATH=/bin
mount -t proc  none /proc
mount -t sysfs none /sys
echo
echo "=== infinigpu: guest kernel PCI enumeration ==="
found=no
for d in /sys/bus/pci/devices/*; do
    v=$(cat "$d/vendor"); dev=$(cat "$d/device"); c=$(cat "$d/class")
    echo "  $(basename "$d")  vendor=$v device=$dev class=$c"
    if [ "$v" = "0x1b36" ] && [ "$dev" = "0x0110" ]; then
        echo "  >>> INFINIGPU-FOUND at $(basename "$d") class=$c"
        found=yes
    fi
done
echo "=== enumeration result: $found ==="
poweroff -f
INIT
chmod +x "$IR/init"
( cd "$IR" && find . | cpio -o -H newc 2>/dev/null | gzip ) > "$WORK/initramfs.cpio.gz"

# ---- device server ----
RUST_LOG=warn "$DEV" --socket "$SOCK" --vm-id guest &
DEVPID=$!
for _ in $(seq 1 50); do [[ -S "$SOCK" ]] && break; sleep 0.1; done

# ---- boot the guest ----
DEVICE_JSON="$(printf '{"driver":"vfio-user-pci","socket":{"path":"%s","type":"unix"},"x-pci-class-code":229376,"x-no-posted-writes":true}' "$SOCK")"
echo ">> booting guest kernel ${KREL} with the infinigpu device…"
LOG="$WORK/serial.log"
timeout 40 "$QEMU" \
    -machine q35,accel=kvm,memory-backend=mem0 \
    -object memory-backend-memfd,id=mem0,share=on,size=1G \
    -m 1G -display none -no-reboot \
    -kernel "$VMLINUZ" -initrd "$WORK/initramfs.cpio.gz" \
    -append "console=ttyS0 rdinit=/init panic=1 loglevel=4" \
    -device "$DEVICE_JSON" \
    -nographic 2>/dev/null | tee "$LOG" || true

echo
if grep -q "INFINIGPU-FOUND" "$LOG"; then
    echo "PASS: the guest Linux kernel enumerated the infinigpu device on its PCI bus."
    grep "INFINIGPU-FOUND\|enumeration result" "$LOG"
else
    echo "FAIL: device not found in guest enumeration."
    exit 1
fi
