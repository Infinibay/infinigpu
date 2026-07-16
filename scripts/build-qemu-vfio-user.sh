#!/usr/bin/env bash
# Build a QEMU with the upstream `vfio-user-pci` client device into a private
# prefix, WITHOUT touching the system QEMU. Needed for Phase-0 Step 1 (the seam
# spike): the vfio-user client landed upstream in QEMU 10.1 (use >= 10.1.1).
#
#   Verified 2026-07-16. Property is `socket=` (SocketAddress); `share=on` is
#   mandatory so the out-of-process device can map guest RAM for DMA.
#
# Usage:  ./scripts/build-qemu-vfio-user.sh          # 10.1.5 -> /opt/qemu-vfio-user
#         QEMU_VER=11.0.2 PREFIX=/opt/qemu ./scripts/build-qemu-vfio-user.sh
#
# Runs the build as your user; uses sudo only for `apt install` and `make install`.
set -euo pipefail

QEMU_VER="${QEMU_VER:-10.1.5}"
PREFIX="${PREFIX:-/opt/qemu-vfio-user}"
BUILD_DIR="${BUILD_DIR:-$HOME/qemu-build}"

echo ">> QEMU ${QEMU_VER} -> ${PREFIX}"

echo ">> [sudo] installing build dependencies…"
sudo apt-get update
sudo apt-get install -y git build-essential ninja-build meson pkg-config \
    python3 python3-venv flex bison \
    libglib2.0-dev libpixman-1-dev zlib1g-dev libslirp-dev

mkdir -p "${BUILD_DIR}"
cd "${BUILD_DIR}"
if [[ ! -f "qemu-${QEMU_VER}.tar.xz" ]]; then
    echo ">> downloading qemu-${QEMU_VER}.tar.xz"
    wget -q "https://download.qemu.org/qemu-${QEMU_VER}.tar.xz"
fi
rm -rf "qemu-${QEMU_VER}"
tar xf "qemu-${QEMU_VER}.tar.xz"
cd "qemu-${QEMU_VER}"

# vfio-user is Kconfig `default y, depends on VFIO_PCI` — no special flag needed.
./configure \
    --prefix="${PREFIX}" \
    --target-list=x86_64-softmmu \
    --enable-slirp

make -j"$(nproc)"

echo ">> [sudo] installing to ${PREFIX}…"
sudo make install

echo ">> verifying vfio-user-pci is present:"
"${PREFIX}/bin/qemu-system-x86_64" -device vfio-user-pci,help | head -20
echo ">> done. Point infinization / test scripts at ${PREFIX}/bin/qemu-system-x86_64"
