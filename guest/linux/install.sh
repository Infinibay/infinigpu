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

log "=== done (module builds + loads on next boot as the guest's DRM display) ==="
exit 0
