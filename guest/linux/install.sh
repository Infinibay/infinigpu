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
  # infinigpu.c does #include "infinigpu_drm.h"; the Makefile's -I../include does NOT resolve
  # in the flat DKMS build dir, so ship the header FLAT next to the .c — the quote-include then
  # finds it locally. Omitting it is the classic `fatal error: infinigpu_drm.h` build failure.
  cp -f "$SRC_DIR/infinigpu.c" "$SRC_DIR/Makefile" "$SRC_DIR/dkms.conf" "$DEST/" 2>&1 | tee -a "$LOG"
  if [ -f "$SRC_DIR/infinigpu_drm.h" ]; then
    cp -f "$SRC_DIR/infinigpu_drm.h" "$DEST/" 2>&1 | tee -a "$LOG"
  else
    log "[WARN] infinigpu_drm.h missing from bundle — the DKMS build will fail; re-stage with a fixed build-into.sh"
  fi
  dkms remove -m infinigpu -v "$VERSION" --all 2>/dev/null || true
  if dkms add -m infinigpu -v "$VERSION" 2>&1 | tee -a "$LOG"; then
    # Best-effort build now (only succeeds if headers for the running kernel exist);
    # AUTOINSTALL=yes + the dkms service guarantees a build on first boot regardless.
    dkms autoinstall 2>&1 | tee -a "$LOG" || log "[INFO] deferred DKMS build to first boot"
    systemctl enable dkms.service 2>/dev/null || true
    # Verify the build actually produced a loadable module — DKMS 'added' but a failed compile
    # (e.g. a missing header) leaves NO .ko and the guest silently boots with no /dev/dri.
    if dkms status -m infinigpu -v "$VERSION" 2>/dev/null | grep -qiE "installed|built"; then
      log "[OK] DKMS built + installed infinigpu.ko"
    else
      log "[WARN] DKMS registered but NOT yet built (deferred to first boot, or the build failed)."
      log "       Check: dkms status; and /var/lib/dkms/infinigpu/$VERSION/build/make.log"
    fi
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

# 4b. Load with ring_drainer=1. The 3D forwarded-submit ioctl (the Vulkan render path) is
# gated behind this module param — without it every vkQueueSubmit returns DEVICE_LOST
# (KMD ioctl -ENODEV). The display/2D path also uses the real ring (RESOURCE_* blob scanout)
# when it's on. Default-off in the module is a dev safeguard; a GPU VM always wants it on.
mkdir -p /etc/modprobe.d
printf 'options infinigpu ring_drainer=1\n' > /etc/modprobe.d/infinigpu.conf
log "[OK] /etc/modprobe.d/infinigpu.conf written (ring_drainer=1 → 3D submit enabled)"

# 4b'. Reserve a CMA pool so the KMD can allocate LARGE contiguous scanout framebuffers.
# The infinigpu KMD backs dumb/scanout buffers with GEM-DMA (physically-contiguous
# dma_alloc_coherent). With no CMA reservation the kernel caps a single contiguous
# allocation at MAX_ORDER (~4 MiB), so any framebuffer past ~1024x768 (1920x1080 = 8 MiB,
# 4K = 33 MiB) fails DRM_IOCTL_MODE_CREATE_DUMB with -ENOMEM. gnome-shell then can't lock
# a front buffer ("gbm_surface_lock_front_buffer failed") and the GPU console is BLACK at
# any real resolution. cma=512M comfortably covers multi-buffered 4K. CMA is reserved at
# boot, so this takes effect on the next boot (the GPU cold-boot after setup).
if [ -f /etc/default/grub ] && command -v update-grub >/dev/null 2>&1; then
  if grep -q 'cma=' /etc/default/grub; then
    log "[OK] cma= already present in GRUB cmdline"
  else
    if grep -q '^GRUB_CMDLINE_LINUX_DEFAULT=' /etc/default/grub; then
      sed -i 's/^GRUB_CMDLINE_LINUX_DEFAULT="\(.*\)"/GRUB_CMDLINE_LINUX_DEFAULT="\1 cma=512M"/' /etc/default/grub
    else
      echo 'GRUB_CMDLINE_LINUX_DEFAULT="cma=512M"' >> /etc/default/grub
    fi
    # normalise a leading space left when the prior value was empty ("" → " cma=512M")
    sed -i 's/GRUB_CMDLINE_LINUX_DEFAULT=" cma=/GRUB_CMDLINE_LINUX_DEFAULT="cma=/' /etc/default/grub
    if update-grub 2>&1 | tee -a "$LOG"; then
      log "[OK] reserved cma=512M for large scanout framebuffers (effective next boot)"
    else
      log "[WARN] update-grub failed — large framebuffers may -ENOMEM; set cma=512M manually"
    fi
  fi
else
  log "[INFO] no GRUB/update-grub — ensure the kernel gets cma=512M for large framebuffers"
fi

# 4c. Load the module AFTER DKMS has built it — closes a first-boot race.
# On the FIRST boot into the DKMS-target kernel (the guest apt/dnf-upgrades the kernel
# during install, so the GPU cold-boot lands on a kernel DKMS hasn't built for yet),
# systemd-modules-load.service runs EARLY — before dkms.service compiles the .ko — so the
# modules-load.d entry above silently no-ops and the guest boots with NO /dev/dri (blank
# GPU console + a manual `modprobe` needed). This oneshot orders After=dkms.service, so the
# .ko is guaranteed present, then modprobes it (ring_drainer=1 comes from modprobe.d).
# Idempotent: a no-op on later boots where modules-load.d already loaded it.
if command -v systemctl >/dev/null 2>&1; then
  cat > /etc/systemd/system/infinigpu-load.service <<'UNIT'
[Unit]
Description=Load the infinigpu KMD (after DKMS builds it on first boot)
After=dkms.service systemd-modules-load.service
Wants=dkms.service
ConditionPathExists=/sys/bus/pci/devices

[Service]
Type=oneshot
RemainAfterExit=yes
# ExitCode is intentionally ignored (|| true): if the PCI device is absent (a non-GPU
# boot) modprobe still succeeds; a genuine build failure surfaces via dkms status.
ExecStart=/sbin/modprobe infinigpu

[Install]
WantedBy=multi-user.target
UNIT
  systemctl enable infinigpu-load.service 2>/dev/null || true
  log "[OK] infinigpu-load.service enabled (modprobe After=dkms.service — first-boot race fix)"

  # Order the display manager AFTER the module is loaded, else gdm/Wayland comes up
  # before /dev/dri/card0 exists and the whole GPU console is BLACK until a manual
  # modprobe + DM restart (there is no `dkms.service` on Ubuntu — DKMS builds via kernel
  # hooks — so infinigpu-load's `After=dkms.service` is a dangling no-op and the module
  # can otherwise land after gdm has already picked its renderer). A *weak* Wants means a
  # non-GPU boot where the module can't bind never blocks the display manager.
  mkdir -p /etc/systemd/system/display-manager.service.d
  cat > /etc/systemd/system/display-manager.service.d/10-infinigpu.conf <<'DROPIN'
[Unit]
After=infinigpu-load.service
Wants=infinigpu-load.service
DROPIN
  log "[OK] display-manager ordered after infinigpu-load (no black GPU console on boot)"
fi

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
  # NOTE: deliberately NOT installing mesa-vulkan-drivers — its lavapipe/llvmpipe ICD is a
  # competing software fallback that an app would silently pick when the infinigpu ICD isn't
  # enumerating, masking a real failure as a (CPU-rendered) pass. The infinigpu ICD is the
  # only Vulkan driver a GPU VM should have.
  if [ "$PM" = apt ]; then
    apt-get install -y libvulkan1 libdrm2 vulkan-tools libvulkan-dev 2>&1 | tee -a "$LOG" || true
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
