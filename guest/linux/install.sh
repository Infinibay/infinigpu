#!/usr/bin/env bash
# infinigpu guest DRM driver installer — Fedora/RHEL + Ubuntu/Debian.
#
# Bundled with the driver source (infinigpu.c, Makefile, dkms.conf) into the tarball
# the backend serves at GET /gpu-driver/linux/source. The OS auto-install
# (kickstart %post / cloud-init late-commands) fetches the tarball, extracts it, and
# runs this script inside the freshly-installed system.
#
# IMPORTANT: it runs under the INSTALLER's kernel (chroot into the target), which is
# NOT the kernel that will boot. So we do NOT build here — we register the module
# with DKMS and let DKMS build it on FIRST BOOT against the real running kernel
# (AUTOINSTALL=yes + the dkms service). Idempotent; safe to re-run.
set -uo pipefail

VERSION="0.1.0"
SRC_DIR="$(cd "$(dirname "$0")" && pwd)"
DEST="/usr/src/infinigpu-${VERSION}"
LOG="/var/log/infinigpu-driver-install.log"
log() { echo "[infinigpu-driver] $*" | tee -a "$LOG" 2>/dev/null || echo "[infinigpu-driver] $*"; }

log "=== infinigpu guest driver install (v${VERSION}) ==="

# 1. package manager / distro family
if command -v dnf >/dev/null 2>&1; then PM=dnf
elif command -v apt-get >/dev/null 2>&1; then PM=apt
else log "ERROR: no dnf/apt found — unsupported distro"; exit 0; fi

# 2. build prerequisites: DKMS + a kernel-headers METAPACKAGE (tracks the installed
#    kernel, unlike uname -r which is the installer's kernel) + a toolchain.
if [ "$PM" = dnf ]; then
  dnf install -y dkms kernel-devel kernel-headers gcc make 2>&1 | tee -a "$LOG" || \
    log "[WARN] dnf dep install had errors (see log)"
else
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -y 2>&1 | tee -a "$LOG" || true
  apt-get install -y dkms linux-headers-generic build-essential 2>&1 | tee -a "$LOG" || \
    log "[WARN] apt dep install had errors (see log)"
fi

# 3. register the module source with DKMS (build happens on first boot).
if command -v dkms >/dev/null 2>&1; then
  rm -rf "$DEST"; mkdir -p "$DEST"
  cp -f "$SRC_DIR/infinigpu.c" "$SRC_DIR/Makefile" "$SRC_DIR/dkms.conf" "$DEST/" 2>&1 | tee -a "$LOG"
  dkms remove -m infinigpu -v "$VERSION" --all 2>/dev/null || true
  if dkms add -m infinigpu -v "$VERSION" 2>&1 | tee -a "$LOG"; then
    # Best-effort build now (only succeeds if headers for the running kernel exist);
    # AUTOINSTALL=yes + the dkms service guarantees a build on first boot regardless.
    dkms autoinstall 2>&1 | tee -a "$LOG" || log "[INFO] deferred DKMS build to first boot"
    systemctl enable dkms.service 2>/dev/null || true
    log "[OK] registered with DKMS (builds against the installed kernel on first boot)"
  else
    log "[WARN] dkms add failed — see $LOG"
  fi
else
  log "[WARN] dkms not available; the driver will not auto-build. Install dkms + headers, then: dkms add/build/install in $DEST"
fi

# 4. autoload on every boot. infinigpu depends on drm_dma_helper (loaded first).
mkdir -p /etc/modules-load.d
printf 'drm_dma_helper\ninfinigpu\n' > /etc/modules-load.d/infinigpu.conf
log "[OK] /etc/modules-load.d/infinigpu.conf written"

# 5. Vulkan ICD (userspace driver): unlike the DKMS kernel module, the ICD is a
#    compiled Mesa-tree artifact shipped PREBUILT in this bundle. Install the .so where
#    the manifest's library_path points and drop the manifest in the loader search path.
if [ -f "$SRC_DIR/libvulkan_infinigpu.so" ] && [ -f "$SRC_DIR/infinigpu_icd.json" ]; then
  LIBDIR="/usr/local/lib/x86_64-linux-gnu"   # == the manifest library_path
  ICDDIR="/usr/share/vulkan/icd.d"
  mkdir -p "$LIBDIR" "$ICDDIR"
  install -m0644 "$SRC_DIR/libvulkan_infinigpu.so" "$LIBDIR/libvulkan_infinigpu.so"
  install -m0644 "$SRC_DIR/infinigpu_icd.json" "$ICDDIR/infinigpu_icd.x86_64.json"
  ldconfig 2>/dev/null || true
  # Vulkan loader + libdrm at runtime; headers/tools for the validation app (best-effort).
  if [ "$PM" = apt ]; then
    apt-get install -y libvulkan1 libdrm2 mesa-vulkan-drivers vulkan-tools libvulkan-dev 2>&1 | tee -a "$LOG" || true
  else
    dnf install -y vulkan-loader libdrm vulkan-tools vulkan-headers 2>&1 | tee -a "$LOG" || true
  fi
  log "[OK] installed Vulkan ICD → $ICDDIR/infinigpu_icd.x86_64.json (lib in $LIBDIR)"

  # Ship + best-effort build the end-to-end validation app (run it after the GPU attaches).
  if [ -f "$SRC_DIR/infinigpu_tri_test.c" ]; then
    SHARE=/usr/local/share/infinigpu; mkdir -p "$SHARE"
    cp -f "$SRC_DIR/infinigpu_tri_test.c" "$SRC_DIR/infinigpu_tri_spv.h" "$SHARE/" 2>/dev/null || true
    if command -v cc >/dev/null 2>&1 && \
       cc -O2 -o /usr/local/bin/infinigpu-tri-test "$SHARE/infinigpu_tri_test.c" -I"$SHARE" -lvulkan 2>>"$LOG"; then
      log "[OK] built validation app — after the GPU attaches, run: infinigpu-tri-test"
    else
      log "[INFO] validation app source in $SHARE (build: cc -O2 -o tri $SHARE/infinigpu_tri_test.c -I$SHARE -lvulkan)"
    fi
  fi
else
  log "[INFO] no Vulkan ICD in bundle — DRM display only, no guest Vulkan"
fi

log "=== done (module builds + loads on next boot as the guest's DRM display) ==="
exit 0
