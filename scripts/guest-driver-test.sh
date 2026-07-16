#!/usr/bin/env bash
# Phase-0 Step-3 (part 2): boot a REAL Linux guest, load the infinigpu.ko driver,
# and let it run its in-kernel self-test — which submits a DISPLAY_CLEAR through our
# device, has the host render it on the physical GPU, and verifies the frame came
# back into guest DMA memory. Uses the host kernel + a busybox initramfs carrying
# the module (built against the same kernel).
set -euo pipefail
cd "$(dirname "$0")/.."

QEMU="${QEMU:-/opt/qemu-vfio-user/bin/qemu-system-x86_64}"
KREL="$(uname -r)"
BUSYBOX="$(command -v busybox)"
KCOPY="${KERNEL:-$HOME/.cache/infinigpu/vmlinuz}"
if [[ -r "/boot/vmlinuz-${KREL}" ]]; then VMLINUZ="/boot/vmlinuz-${KREL}"
elif [[ -r "$KCOPY" ]]; then VMLINUZ="$KCOPY"
else
    echo "!! No readable kernel. Run once (needs sudo), then re-run:"
    echo "     mkdir -p ~/.cache/infinigpu && sudo install -m0644 /boot/vmlinuz-${KREL} $KCOPY"
    exit 1
fi
[[ -x "$QEMU" ]] || { echo "!! $QEMU missing"; exit 1; }

# ---- build the module + device ----
make -C guest/linux >/dev/null
KO="guest/linux/infinigpu.ko"
[[ -f "$KO" ]] || { echo "!! module build failed"; exit 1; }
cargo build -q -p infinigpu-device
DEV=target/debug/infinigpu-device

WORK="$(mktemp -d /tmp/infinigpu-drv-XXXXXX)"
SOCK="$(mktemp -u /tmp/infinigpu-drv-XXXXXX.sock)"
trap 'kill ${DEVPID:-0} 2>/dev/null || true; cp "$WORK"/*.log /tmp/ 2>/dev/null || true; rm -rf "$WORK" "$SOCK"' EXIT

# ---- initramfs with busybox + the driver ----
IR="$WORK/ir"; mkdir -p "$IR"/{bin,proc,sys,dev}
cp "$BUSYBOX" "$IR/bin/busybox"
cp "$KO" "$IR/infinigpu.ko"
cat >"$IR/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox --install -s /bin
export PATH=/bin
mount -t proc none /proc
mount -t sysfs none /sys
mount -t devtmpfs none /dev 2>/dev/null
echo "=== infinigpu: loading guest driver ==="
insmod /infinigpu.ko
dmesg | grep -i infinigpu
echo "=== infinigpu: driver test done ==="
poweroff -f
INIT
chmod +x "$IR/init"
( cd "$IR" && find . | cpio -o -H newc 2>/dev/null | gzip ) > "$WORK/initramfs.cpio.gz"

# ---- device server (renders on the real GPU) ----
RUST_LOG=info "$DEV" --socket "$SOCK" --vm-id guest 2>"$WORK/dev.log" &
DEVPID=$!
for _ in $(seq 1 50); do [[ -S "$SOCK" ]] && break; sleep 0.1; done

# ---- boot ----
# x-no-posted-writes: the vfio_user v0.1.3 crate always replies to REGION_WRITE, but
# QEMU posts MMIO writes by default (expects no reply) → protocol desync. Force
# non-posted writes so every write waits for the crate's reply.
DEVICE_JSON="$(printf '{"driver":"vfio-user-pci","socket":{"path":"%s","type":"unix"},"x-pci-class-code":229376,"x-no-posted-writes":true}' "$SOCK")"
echo ">> booting guest ${KREL}, loading infinigpu.ko…"
LOG="$WORK/serial.log"
timeout 40 "$QEMU" \
    -machine q35,accel=kvm,memory-backend=mem0 \
    -object memory-backend-memfd,id=mem0,share=on,size=1G \
    -m 1G -display none -no-reboot \
    -kernel "$VMLINUZ" -initrd "$WORK/initramfs.cpio.gz" \
    -append "console=ttyS0 rdinit=/init panic=1 loglevel=7" \
    -device "$DEVICE_JSON" \
    -nographic 2>"$WORK/qemu.log" | tee "$LOG" || true

echo
echo ">> ---- host device log ----"; grep -E 'rendered|replay GPU|DMA_MAP' "$WORK/dev.log" | tail -4
echo
if grep -q "INFINIGPU-SELFTEST: PASS" "$LOG"; then
    echo "PASS: the guest kernel driver submitted a frame that the host rendered on the"
    echo "      physical GPU and returned into guest memory. Full guest→GPU loop works."
    grep "INFINIGPU-SELFTEST" "$LOG"
else
    echo "FAIL: self-test did not pass. Guest infinigpu log:"
    grep -i infinigpu "$LOG" || true
    exit 1
fi
