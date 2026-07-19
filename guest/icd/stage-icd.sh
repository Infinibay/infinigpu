#!/usr/bin/env bash
# Host-build the Vulkan ICD and stage it under repos/infinigpu/target/icd/ so
# deploy/build-into.sh folds it (+ the validation app) into the guest bundle the
# backend serves to GPU VMs (they install it on first boot via guest/linux/install.sh).
#
# Mirrors the native viewer's "host-build then reuse" pattern — the ICD is a compiled
# Mesa-tree artifact, not in-guest buildable. After staging, run `iby gpu build` (which
# runs deploy/build-into.sh) then `iby up --gpu`.
#
# glibc note: the .so needs only glibc >= 2.38 + libdrm2, so a host build serves any
# reasonably-recent guest (Ubuntu 24.04+/Fedora). Build on a host no newer than the guest.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"

bash "$HERE/build.sh"

BUILD="${MESA_SRC:-$HOME/.cache/infinigpu/mesa}/build-infinigpu/src/infinigpu/vulkan"
so="$BUILD/libvulkan_infinigpu.so"
json="$BUILD/infinigpu_icd.$(uname -m).json"
[ -f "$so" ] && [ -f "$json" ] || { echo "!! ICD build outputs missing under $BUILD" >&2; exit 1; }

mkdir -p "$REPO/target/icd"
install -m0644 "$so"   "$REPO/target/icd/libvulkan_infinigpu.so"
install -m0644 "$json" "$REPO/target/icd/infinigpu_icd.json"
echo ">> staged ICD → $REPO/target/icd/  (next: iby gpu build && iby up --gpu)"
