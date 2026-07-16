#!/usr/bin/env bash
# Phase-0 Step-1 integration smoke: boot REAL QEMU (>=10.1.1) with our
# infinigpu-device attached over vfio-user, with no guest OS. SeaBIOS enumerates
# PCI (reads our config space) and QEMU maps guest RAM into our device (DMA_MAP) —
# proving our Rust device server interoperates with the actual QEMU vfio-user
# client, not just the in-process loopback Client.
set -euo pipefail
cd "$(dirname "$0")/.."

QEMU="${QEMU:-/opt/qemu-vfio-user/bin/qemu-system-x86_64}"
[[ -x "$QEMU" ]] || { echo "!! $QEMU not found — run scripts/build-qemu-vfio-user.sh"; exit 1; }

cargo build -q -p infinigpu-device
DEV=target/debug/infinigpu-device
SOCK="$(mktemp -u /tmp/infinigpu-smoke-XXXXXX.sock)"
DEVLOG="$(mktemp /tmp/infinigpu-devlog-XXXXXX.txt)"

echo ">> starting device server on $SOCK"
RUST_LOG=info "$DEV" --socket "$SOCK" --vm-id smoke >"$DEVLOG" 2>&1 &
DEVPID=$!
trap 'kill $DEVPID 2>/dev/null || true; rm -f "$SOCK"' EXIT

for _ in $(seq 1 50); do [[ -S "$SOCK" ]] && break; sleep 0.1; done
[[ -S "$SOCK" ]] || { echo "!! device socket never appeared"; cat "$DEVLOG"; exit 1; }

echo ">> booting QEMU with the infinigpu device attached (headless, no disk, ~6s)"
# socket is a SocketAddress union → must use the JSON -device form. class 0x038000 = 229376.
DEVICE_JSON="$(printf '{"driver":"vfio-user-pci","socket":{"path":"%s","type":"unix"},"x-pci-class-code":229376,"x-no-posted-writes":true}' "$SOCK")"
timeout 6 "$QEMU" \
    -machine q35,accel=kvm,memory-backend=mem0 \
    -object memory-backend-memfd,id=mem0,share=on,size=1G \
    -m 1G -display none -vga none -no-reboot \
    -device "$DEVICE_JSON" \
    -serial null -monitor none 2>/tmp/infinigpu-qemu-err.txt || true

echo
echo ">> ===== infinigpu-device server log ====="
cat "$DEVLOG"
echo ">> ======================================="
echo
if grep -q "DMA_MAP" "$DEVLOG" && grep -q "config read @0x00" "$DEVLOG"; then
    echo "PASS: real QEMU completed the vfio-user handshake, mapped guest RAM into our"
    echo "      device, and SeaBIOS enumerated our PCI config space."
else
    echo "FAIL: expected handshake evidence not found. QEMU stderr:"
    cat /tmp/infinigpu-qemu-err.txt
    exit 1
fi
