#!/usr/bin/env bash
# Build the infinigpu Vulkan ICD (Phase 0) — docs/adr/GUEST-ICD-IMPLEMENTATION.md.
#
# Mesa's Vulkan runtime is static-only / never installed, so there is NO out-of-tree
# build. This injects our driver source (this dir) into a pinned Mesa checkout as
# src/infinigpu/vulkan/, registers -Dvulkan-drivers=infinigpu, and builds
# libvulkan_infinigpu.so + its ICD manifest. Our source lives HERE; Mesa is a build
# substrate cloned into ~/.cache/infinigpu/mesa.
#
#   env knobs:
#     MESA_SRC   pinned Mesa checkout (default ~/.cache/infinigpu/mesa, tag mesa-25.0.7)
#     PKG_CONFIG_PATH  must resolve libdrm (apt install libdrm-dev, or extract the deb)
#   after a successful build, smoke-test on the host (no infinigpu DRM node here):
#     VK_DRIVER_FILES=$MESA_SRC/build/src/infinigpu/vulkan/infinigpu_devenv_icd.*.json \
#     INFINIGPU_SMOKE_ANY_NODE=1 vulkaninfo | grep -E 'driverName|deviceName'
set -euo pipefail

ICD_SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MESA_SRC="${MESA_SRC:-$HOME/.cache/infinigpu/mesa}"
MESA_TAG="mesa-25.0.7"
DST="$MESA_SRC/src/infinigpu"

die() { echo "!! $*" >&2; exit 1; }
[ -d "$MESA_SRC/.git" ] || die "Mesa checkout not at $MESA_SRC — clone tag $MESA_TAG first (see README)"

echo ">> injecting driver source into $DST/vulkan"
mkdir -p "$DST/vulkan"
cp "$ICD_SRC"/infinigpu_*.c "$ICD_SRC"/infinigpu_*.h "$ICD_SRC/meson.build" "$DST/vulkan/"
cp "$ICD_SRC/meson.wrapper.build" "$DST/meson.build"
# The shared guest headers the ICD consumes: the wire ABI (infinigpu_abi.h — used by the forwarded
# encoder) and the render-node uAPI (infinigpu_drm.h — the SUBMIT_FORWARDED ioctl + dumb-buffer path).
# They live in guest/include; copy them alongside the driver so `#include "infinigpu_abi.h"` resolves.
cp "$ICD_SRC/../include/infinigpu_abi.h" "$ICD_SRC/../include/infinigpu_drm.h" "$DST/vulkan/"

# --- idempotent registration edits into the Mesa tree ---------------------------------
opts="$MESA_SRC/meson_options.txt"
top="$MESA_SRC/meson.build"
srcm="$MESA_SRC/src/meson.build"

if ! grep -q "'infinigpu'" "$opts"; then
  echo ">> meson_options.txt: add infinigpu to vulkan-drivers choices"
  # The vulkan-drivers choices line ending in `'gfxstream',` is unique to that
  # option (gallium-drivers has no gfxstream); the top meson.build list ends in
  # `'gfxstream']` (no comma), so this anchor is unambiguous.
  sed -i "s/'nouveau', 'asahi', 'gfxstream',\$/'nouveau', 'asahi', 'gfxstream', 'infinigpu',/" "$opts"
  grep -q "'infinigpu'" "$opts" || die "meson_options.txt vulkan-drivers edit failed"
fi

if ! grep -q "with_infinigpu_vk" "$top"; then
  echo ">> meson.build: add infinigpu to _vulkan_drivers 'all' + with_infinigpu_vk"
  sed -i "s/'nouveau', 'asahi', 'gfxstream'\]/'nouveau', 'asahi', 'gfxstream', 'infinigpu']/" "$top"
  sed -i "s/^with_virtio_vk = _vulkan_drivers.contains('virtio')/&\nwith_infinigpu_vk = _vulkan_drivers.contains('infinigpu')/" "$top"
fi

if ! grep -q "subdir('infinigpu')" "$srcm"; then
  echo ">> src/meson.build: subdir('infinigpu') gate"
  # append the gate after the virtio subdir block
  printf "\nif with_infinigpu_vk\n  subdir('infinigpu')\nendif\n" >> "$srcm"
fi

# --- configure + build ----------------------------------------------------------------
BUILD="$MESA_SRC/build-infinigpu"
if [ ! -d "$BUILD" ]; then
  echo ">> meson setup (minimal, vulkan-only)"
  meson setup "$BUILD" "$MESA_SRC" \
    -Dvulkan-drivers=infinigpu \
    -Dgallium-drivers= \
    -Dplatforms= \
    -Dglx=disabled -Degl=disabled -Dgbm=disabled -Dopengl=false \
    -Dllvm=disabled \
    -Dvulkan-layers= \
    -Dvideo-codecs= \
    -Dbuildtype=debugoptimized
else
  echo ">> reusing $BUILD (delete it to reconfigure)"
fi

echo ">> ninja (driver .so + ICD manifests)"
ninja -C "$BUILD" \
  src/infinigpu/vulkan/libvulkan_infinigpu.so \
  "src/infinigpu/vulkan/infinigpu_devenv_icd.$(uname -m).json" \
  "src/infinigpu/vulkan/infinigpu_icd.$(uname -m).json"

echo
echo ">> built:"
ls -la "$BUILD"/src/infinigpu/vulkan/libvulkan_infinigpu.so "$BUILD"/src/infinigpu/vulkan/infinigpu_*icd*.json 2>/dev/null || true
echo
echo ">> host smoke (renderD128 is NOT infinigpu here → needs INFINIGPU_SMOKE_ANY_NODE=1):"
echo "   VK_DRIVER_FILES=$BUILD/src/infinigpu/vulkan/infinigpu_devenv_icd.$(uname -m).json \\"
echo "   INFINIGPU_SMOKE_ANY_NODE=1 vulkaninfo | grep -E 'driverName|deviceName|driverID'"
