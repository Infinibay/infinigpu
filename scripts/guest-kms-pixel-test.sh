#!/usr/bin/env bash
# The full stack in one shot: a real Linux guest renders its console through our
# DRM/KMS driver → the host device presents each framebuffer → NVENC encodes it →
# infiniPixel streams it over WebSocket → a client decodes it. This ties DRM/KMS
# (guest display) + infiniPixel (remote protocol): the guest's actual console
# becomes a browser-decodable H.264 stream.
set -euo pipefail
cd "$(dirname "$0")/.."

QEMU="${QEMU:-/opt/qemu-vfio-user/bin/qemu-system-x86_64}"
KREL="$(uname -r)"
BUSYBOX="$(command -v busybox)"
KCOPY="${KERNEL:-$HOME/.cache/infinigpu/vmlinuz}"
PIXEL_PORT="${PIXEL_PORT:-8092}"
if [[ -r "/boot/vmlinuz-${KREL}" ]]; then VMLINUZ="/boot/vmlinuz-${KREL}"
elif [[ -r "$KCOPY" ]]; then VMLINUZ="$KCOPY"
else echo "!! No readable kernel (see guest-kms-test.sh)"; exit 1; fi
for t in "$QEMU" "$BUSYBOX"; do [[ -x "$t" ]] || { echo "!! missing $t"; exit 1; }; done
command -v ffmpeg >/dev/null || { echo "!! ffmpeg required"; exit 1; }
command -v node   >/dev/null || { echo "!! node (>=21) required"; exit 1; }

DMA_KO_SRC="$(modinfo -n drm_dma_helper 2>/dev/null || true)"
[[ -n "$DMA_KO_SRC" ]] || { echo "!! drm_dma_helper not found"; exit 1; }

make -C guest/linux >/dev/null
cargo build -q -p infinigpu-device
DEV=target/debug/infinigpu-device
KO=guest/linux/infinigpu.ko

WORK="$(mktemp -d /tmp/infinigpu-kmspix-XXXXXX)"
SOCK="$(mktemp -u /tmp/infinigpu-kmspix-XXXXXX.sock)"
trap 'kill ${DEVPID:-0} ${NODEPID:-0} 2>/dev/null || true; rm -rf "$WORK" "$SOCK"' EXIT

# ---- initramfs (busybox + drm_dma_helper + our module) ----
IR="$WORK/ir"; mkdir -p "$IR"/{bin,proc,sys,dev}
cp "$BUSYBOX" "$IR/bin/busybox"; cp "$KO" "$IR/infinigpu.ko"
if [[ "$DMA_KO_SRC" == *.zst ]]; then zstd -q -d -o "$IR/drm_dma_helper.ko" "$DMA_KO_SRC"; else cp "$DMA_KO_SRC" "$IR/drm_dma_helper.ko"; fi
cat >"$IR/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox --install -s /bin
export PATH=/bin
mount -t proc none /proc; mount -t sysfs none /sys; mount -t devtmpfs none /dev
insmod /drm_dma_helper.ko; insmod /infinigpu.ko
dmesg | grep -iE 'infinigpu|drm' | tail -5
# Drive content onto the framebuffer console via the KERNEL log (/dev/kmsg → console
# → fbcon renders it), which is what actually paints our fb — writes to /dev/tty0 do
# not. One line per second so fbcon's deferred-io flushes each as its own content-rich
# present. Kept alive long enough for the client to capture many content frames.
i=1; while [ $i -le 16 ]; do
  echo "infiniPixel over DRM/KMS -- live guest console frame $i -- the quick brown fox 0123456789" > /dev/kmsg 2>/dev/null || true
  sleep 1
  i=$((i+1))
done
poweroff -f
INIT
chmod +x "$IR/init"
( cd "$IR" && find . | cpio -o -H newc 2>/dev/null | gzip ) > "$WORK/initramfs.cpio.gz"

# ---- Node capture client: retry-connect, collect access units, write Annex-B ----
cat >"$WORK/capture.mjs" <<'JS'
import fs from 'node:fs';
const [port, want, out] = [process.argv[2], parseInt(process.argv[3]), process.argv[4]];
const MAGIC = 0x49504958; const chunks = []; let got = 0, kf = 0, w = 0, h = 0, ended = false;
const deadline = Date.now() + 30000;
function finish() {
  if (ended) return; ended = true;
  if (got >= 1) {
    fs.writeFileSync(out, Buffer.concat(chunks));
    console.log(`OK frames=${got} keyframes=${kf} dims=${w}x${h}`);
    process.exit(0);
  }
  console.error('no frames captured'); process.exit(5);
}
setTimeout(finish, 30000);
function connect() {
  if (Date.now() > deadline) return finish();
  const ws = new WebSocket(`ws://127.0.0.1:${port}`);
  ws.binaryType = 'arraybuffer';
  ws.onerror = () => setTimeout(connect, 300);                 // streamer not up yet → retry
  ws.onclose = () => { if (!ended) (got >= 1 ? finish() : setTimeout(connect, 300)); };
  ws.onmessage = (ev) => {
    const dv = new DataView(ev.data), u8 = new Uint8Array(ev.data);
    if (dv.getUint32(0, true) !== MAGIC) return;
    if (u8[5] & 1) kf++;
    w = dv.getUint16(12, true); h = dv.getUint16(14, true);
    const plen = dv.getUint32(24, true);
    chunks.push(Buffer.from(u8.subarray(32, 32 + plen)));
    if (++got >= want) finish();
  };
}
connect();
JS

# ---- device server WITH infiniPixel streaming ----
INFINIGPU_PIXEL_PORT="$PIXEL_PORT" RUST_LOG=info "$DEV" --socket "$SOCK" --vm-id guest 2>"$WORK/dev.log" &
DEVPID=$!
for _ in $(seq 1 50); do [[ -S "$SOCK" ]] && break; sleep 0.1; done

# ---- start the capture client (retries until the streamer starts on first present) ----
node "$WORK/capture.mjs" "$PIXEL_PORT" 10 "$WORK/guest-stream.h264" >"$WORK/capture.log" 2>&1 &
NODEPID=$!

# ---- boot the guest ----
DEVICE_JSON="$(printf '{"driver":"vfio-user-pci","socket":{"path":"%s","type":"unix"},"x-pci-class-code":229376,"x-no-posted-writes":true}' "$SOCK")"
echo ">> booting guest ${KREL} (DRM/KMS + live infiniPixel on :$PIXEL_PORT)…"
timeout 60 "$QEMU" \
    -machine q35,accel=kvm,memory-backend=mem0 \
    -object memory-backend-memfd,id=mem0,share=on,size=1G \
    -m 1G -display none -vga none -no-reboot \
    -kernel "$VMLINUZ" -initrd "$WORK/initramfs.cpio.gz" \
    -append "console=tty0 console=ttyS0 rdinit=/init panic=1 loglevel=7" \
    -device "$DEVICE_JSON" -nographic >"$WORK/serial.log" 2>"$WORK/qemu.log" || true

# ---- wait for the capture client to finish (or its own timeout) ----
wait "$NODEPID" 2>/dev/null || true
CAP="$(cat "$WORK/capture.log" 2>/dev/null || true)"

echo ">> capture: $CAP"
echo ">> presents: $(grep -c 'present: frame' "$WORK/dev.log") total; $(grep 'idle-skipped' "$WORK/dev.log" | tail -1 | sed 's/.*infinigpu_device\] //')"

DIMS=""; NF=0
if [[ -s "$WORK/guest-stream.h264" ]]; then
    DIMS="$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$WORK/guest-stream.h264" 2>/dev/null | tr ',' 'x')"
    NF="$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$WORK/guest-stream.h264" 2>/dev/null || echo 0)"
    ffmpeg -hide_banner -loglevel error -f h264 -i "$WORK/guest-stream.h264" -update 1 -y /tmp/infinipixel-guest.png 2>/dev/null || true
fi

echo
if echo "$CAP" | grep -q 'keyframes=[1-9]' && [[ "${NF:-0}" -ge 1 ]]; then
    echo "PASS: the guest's DRM/KMS console was NVENC-encoded and streamed over infiniPixel."
    echo "      captured ${NF}+ decodable H.264 frame(s) at $DIMS from a live guest."
    echo "      Viewable: /tmp/infinipixel-guest.png"
else
    echo "FAIL: no decodable guest stream captured. capture='$CAP' decoded=$NF"
    echo "--- device log ---"; grep -iE 'infiniPixel|present|error' "$WORK/dev.log" | tail -15
    exit 1
fi
