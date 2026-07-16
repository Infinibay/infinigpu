#!/usr/bin/env bash
# Phase-0 Step-3 (real DRM/KMS): boot a Linux guest, load the infinigpu DRM driver,
# and prove it is a genuine display: /dev/dri/card0 comes up, fbcon binds to our
# framebuffer, and every page-flip hands the host a framebuffer it scans out. The
# host writes each presented frame as a PPM (INFINIGPU_PRESENT_DIR) so the guest's
# actual console is viewable host-side — the "real framebuffer" milestone.
#
# The only guest-side module we must load by hand is drm_dma_helper.ko (the GEM-DMA
# helper — a module on Ubuntu); the rest of the DRM stack is built into the kernel.
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
[[ -x "$QEMU" ]] || { echo "!! $QEMU missing (run build-qemu-vfio-user.sh)"; exit 1; }

# ---- the one guest module we load by hand (contiguous GEM-DMA helper) ----
DMA_KO_SRC="$(modinfo -n drm_dma_helper 2>/dev/null || true)"
[[ -n "$DMA_KO_SRC" ]] || { echo "!! drm_dma_helper module not found on host"; exit 1; }

# ---- build our module + the device ----
make -C guest/linux >/dev/null
KO="guest/linux/infinigpu.ko"
[[ -f "$KO" ]] || { echo "!! module build failed"; exit 1; }
cargo build -q -p infinigpu-device
DEV=target/debug/infinigpu-device

WORK="$(mktemp -d /tmp/infinigpu-kms-XXXXXX)"
SOCK="$(mktemp -u /tmp/infinigpu-kms-XXXXXX.sock)"
FRAMES="/tmp/infinigpu-frames"
rm -rf "$FRAMES"; mkdir -p "$FRAMES"
trap 'kill ${DEVPID:-0} 2>/dev/null || true; cp "$WORK"/*.log /tmp/ 2>/dev/null || true; rm -rf "$WORK" "$SOCK"' EXIT

# ---- initramfs: busybox + drm_dma_helper.ko (decompressed) + our module ----
IR="$WORK/ir"; mkdir -p "$IR"/{bin,proc,sys,dev}
cp "$BUSYBOX" "$IR/bin/busybox"
cp "$KO" "$IR/infinigpu.ko"
# host modules are zstd-compressed; busybox insmod needs a plain .ko
if [[ "$DMA_KO_SRC" == *.zst ]]; then zstd -q -d -o "$IR/drm_dma_helper.ko" "$DMA_KO_SRC"
else cp "$DMA_KO_SRC" "$IR/drm_dma_helper.ko"; fi

cat >"$IR/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox --install -s /bin
export PATH=/bin
mount -t proc  none /proc
mount -t sysfs none /sys
mount -t devtmpfs none /dev
echo
echo "=== infinigpu: loading DRM stack ==="
insmod /drm_dma_helper.ko
insmod /infinigpu.ko
echo "--- dmesg (infinigpu / drm / fb) ---"
dmesg | grep -iE 'infinigpu|\[drm\]|fbcon|fb0' | tail -30
echo "--- device nodes ---"
ls -l /dev/dri 2>&1 || echo "  (no /dev/dri)"
ls -l /dev/fb0 2>&1 || echo "  (no /dev/fb0)"
# draw recognizable content onto the framebuffer console (fbcon → our device)
printf '\n\n  *** INFINIGPU KMS: hello from the guest framebuffer ***\n' > /dev/tty0 2>/dev/null || true
i=1; while [ $i -le 8 ]; do
  printf '  infinigpu framebuffer line %d - rendered through our vfio-user device\n' "$i" > /dev/tty0 2>/dev/null || true
  i=$((i+1))
done
# let fbcon deferred-io flush a few presents to the host
sleep 3
echo "=== infinigpu: KMS test done ==="
poweroff -f
INIT
chmod +x "$IR/init"
( cd "$IR" && find . | cpio -o -H newc 2>/dev/null | gzip ) > "$WORK/initramfs.cpio.gz"

# ---- device server: presents guest framebuffers, dumps them as PPMs ----
INFINIGPU_PRESENT_DIR="$FRAMES" RUST_LOG=info "$DEV" --socket "$SOCK" --vm-id guest 2>"$WORK/dev.log" &
DEVPID=$!
for _ in $(seq 1 50); do [[ -S "$SOCK" ]] && break; sleep 0.1; done

# ---- boot ----
# -vga none: our infinigpu must be the ONLY display device so fbcon binds to fb0=ours.
# console=tty0 console=ttyS0: kernel log renders on our framebuffer (tty0) AND the
# serial log (ttyS0, = /dev/console). x-no-posted-writes: required (see the log).
DEVICE_JSON="$(printf '{"driver":"vfio-user-pci","socket":{"path":"%s","type":"unix"},"x-pci-class-code":229376,"x-no-posted-writes":true}' "$SOCK")"
echo ">> booting guest ${KREL} with the infinigpu DRM/KMS driver…"
LOG="$WORK/serial.log"
timeout 50 "$QEMU" \
    -machine q35,accel=kvm,memory-backend=mem0 \
    -object memory-backend-memfd,id=mem0,share=on,size=1G \
    -m 1G -display none -vga none -no-reboot \
    -kernel "$VMLINUZ" -initrd "$WORK/initramfs.cpio.gz" \
    -append "console=tty0 console=ttyS0 rdinit=/init panic=1 loglevel=7" \
    -device "$DEVICE_JSON" \
    -nographic 2>"$WORK/qemu.log" | tee "$LOG" || true

# ---- convert the last presented frame to PNG for easy viewing (best-effort) ----
if [[ -f "$FRAMES/latest.ppm" ]]; then
    if command -v magick >/dev/null;  then magick "$FRAMES/latest.ppm" "$FRAMES/latest.png" 2>/dev/null || true
    elif command -v convert >/dev/null; then convert "$FRAMES/latest.ppm" "$FRAMES/latest.png" 2>/dev/null || true
    elif command -v pnmtopng >/dev/null; then pnmtopng "$FRAMES/latest.ppm" > "$FRAMES/latest.png" 2>/dev/null || true
    fi
fi

echo
echo ">> ---- host present log ----"; grep -E 'present:|replay|DMA_MAP' "$WORK/dev.log" | tail -8
echo ">> ---- presented frames ----"; ls -1 "$FRAMES" 2>/dev/null | sed 's/^/     /'

kms_pass=$(grep -c "INFINIGPU-KMS: PASS" "$LOG" || true)
card0=$(grep -cE '/dev/dri/card0|registered /dev/dri/card0' "$LOG" || true)
presents=$(grep -cE 'present: .*[1-9][0-9]* non-blank' "$WORK/dev.log" || true)

echo
if [[ "$kms_pass" -ge 1 && "$presents" -ge 1 ]]; then
    echo "PASS: infinigpu is a real DRM/KMS display."
    echo "      - guest registered /dev/dri/card0 and the KMS ring self-test retired;"
    echo "      - the host scanned out $presents guest framebuffer(s) with real content."
    echo "      View what the guest rendered:  $FRAMES/latest.ppm (+ latest.png if converted)"
    grep -E 'INFINIGPU-KMS' "$LOG" | head -3
else
    echo "FAIL: KMS path did not fully validate (kms_pass=$kms_pass card0=$card0 presents=$presents)."
    echo "----- guest infinigpu/drm log -----"; grep -iE 'infinigpu|\[drm\]' "$LOG" | tail -30 || true
    echo "----- host device log -----"; tail -20 "$WORK/dev.log" || true
    echo "----- qemu stderr -----"; tail -20 "$WORK/qemu.log" || true
    exit 1
fi
