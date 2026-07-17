#!/usr/bin/env bash
# Runs inside the infinigpu-builder container. Builds the device server from the
# mounted source (/src) and publishes it + the baked vfio-user QEMU into the shared
# /out volume that the backend mounts read-only at /opt/qemu-vfio-user.
#
# QEMU dynamically links glib/pixman/slirp/zlib, which the backend image
# (node:20-bookworm) does NOT ship. Rather than modify the backend image, we bundle
# that shared-lib closure (EXCLUDING glibc — the container's own must be used) into
# /out/lib and rpath-patch the binaries to $ORIGIN/../lib, so they are fully
# self-contained with no LD_LIBRARY_PATH leaking into other backend child processes.
set -euo pipefail

SRC=/src
OUT=/out
QEMU_PREFIX=/opt/qemu-vfio-user

echo ">> publishing vfio-user QEMU → ${OUT}"
mkdir -p "${OUT}/bin" "${OUT}/lib"
cp -a "${QEMU_PREFIX}/." "${OUT}/"

echo ">> building infinigpu-device (release, bookworm ABI)…"
cd "${SRC}"
export CARGO_TARGET_DIR=/target
cargo build --release -p infinigpu-device
install -Dm0755 /target/release/infinigpu-device "${OUT}/bin/infinigpu-device"

echo ">> bundling non-glibc shared-lib closure → ${OUT}/lib"
{ ldd "${OUT}/bin/qemu-system-x86_64" || true; ldd "${OUT}/bin/infinigpu-device" || true; } \
  | awk '/=> \//{print $3}' | sort -u | while read -r so; do
    case "${so}" in
      # glibc + the runtime loader: always resolve from the container, never bundle.
      */libc.so.*|*/libm.so.*|*/libpthread.so.*|*/libdl.so.*|*/librt.so.*|*/libresolv.so.*|*/ld-linux*) : ;;
      *) cp -Lv "${so}" "${OUT}/lib/" || true ;;
    esac
  done

echo ">> rpath-patching binaries to \$ORIGIN/../lib (self-contained, no LD_LIBRARY_PATH)"
patchelf --set-rpath '$ORIGIN/../lib' "${OUT}/bin/qemu-system-x86_64" || true
patchelf --set-rpath '$ORIGIN/../lib' "${OUT}/bin/infinigpu-device" || true

test -x "${OUT}/bin/qemu-system-x86_64" || { echo "!! QEMU missing in output"; exit 1; }
test -x "${OUT}/bin/infinigpu-device"   || { echo "!! device binary missing in output"; exit 1; }
echo ">> done: $("${OUT}/bin/qemu-system-x86_64" --version | head -1)"
echo ">> device binary published → ${OUT}/bin/infinigpu-device"
