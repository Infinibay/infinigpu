#!/usr/bin/env bash
# 3D DE-RISK SPIKE (docs/adr/3D-ACCEL-IMPLEMENTATION.md, Phase 0 / PR0.2).
#
# Isolates the ONE load-bearing question before any host-decoder budget is spent:
#   can a Venus command stream drive NVIDIA's closed Vulkan userspace on THIS A5000?
#
# It uses the STOCK virtio-gpu + virglrenderer-venus stack as a MEASURING INSTRUMENT — deliberately
# NOT our vfio-user device — so the answer is about NVIDIA-as-a-Venus-host, uncoupled from any of
# our own code. The crux is forcing VK_DRIVER_FILES=nvidia_icd on the QEMU process, which makes
# virglrenderer's venus backend bind the NVIDIA proprietary driver on the host.
#
# This script only sets up + launches the guest. The 4-rung workload ladder runs INSIDE the guest;
# record each result in docs/spikes/venus-nvidia-a5000.md and write the GO/NO-GO decision there.
#
#   usage: scripts/spike-venus-nvidia.sh /path/to/guest.qcow2
#   needs: a distro qemu-system-x86_64 built with virtio-gpu-gl (venus=), virglrenderer -Dvenus=true,
#          the NVIDIA proprietary driver >= 570.86 (the 570.86 Venus-host floor), KVM.
set -euo pipefail

GUEST_IMG="${1:-}"
MEM_GB="${SPIKE_MEM_GB:-8}"
HOSTMEM_GB="${SPIKE_HOSTMEM_GB:-4}"
NVIDIA_ICD="${VK_ICD:-/usr/share/vulkan/icd.d/nvidia_icd.json}"
QEMU="${QEMU_BIN:-qemu-system-x86_64}"   # the DISTRO qemu, NOT /opt/qemu-vfio-user
# virtio-gpu-gl needs a GL-capable display backend. On NVIDIA hosts the default `egl-headless` FAILS
# ("egl: no drm render node available") because the proprietary driver exposes no GBM render node —
# override, e.g. SPIKE_DISPLAY='egl-headless,rendernode=/dev/dri/renderD128' once NVIDIA GBM works,
# or run virgl_render_server via RENDER_SERVER_EXEC_PATH (set it to an extracted virgl_render_server
# if `/usr/libexec/virgl_render_server` is not installed). `-display none` is rejected by -gl devices.
SPIKE_DISPLAY="${SPIKE_DISPLAY:-egl-headless}"

die() { echo "!! $*" >&2; exit 1; }

[ -n "$GUEST_IMG" ] || die "usage: $0 /path/to/guest.qcow2  (an Ubuntu 25.04+ guest with Mesa venus)"
[ -f "$GUEST_IMG" ] || die "guest image not found: $GUEST_IMG"
command -v "$QEMU" >/dev/null || die "$QEMU missing (need a distro qemu-system-x86_64 with virtio-gpu-gl)"
[ -e /dev/kvm ] || die "/dev/kvm missing — the spike needs KVM"
[ -f "$NVIDIA_ICD" ] || die "NVIDIA Vulkan ICD not found at $NVIDIA_ICD (set VK_ICD=...)"

# --- Rung 0 (host prep, PR0.1): the driver pin IS the spike. 550.x predates NVIDIA Venus-host
#     support, so a spike on it is a GUARANTEED false NO-GO. Warn loudly if below the floor. ---
if command -v nvidia-smi >/dev/null; then
  DRV="$(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -1 || true)"
  echo ">> NVIDIA driver: ${DRV:-unknown}"
  # Compare major.minor against the 570.86 floor.
  maj="${DRV%%.*}"
  if [ -n "$maj" ] && [ "$maj" -lt 570 ] 2>/dev/null; then
    echo "!! WARNING: driver $DRV is BELOW the 570.86 Venus-host floor — this spike will FALSELY"
    echo "!! NO-GO. Pin/upgrade to >= 570.86 (fleet baseline 570.153.02 or 575.x), reboot, retry."
    echo "!! Set SPIKE_IGNORE_DRIVER=1 to run anyway (only to confirm the false-negative)."
    [ "${SPIKE_IGNORE_DRIVER:-0}" = "1" ] || exit 2
  fi
else
  echo "!! nvidia-smi not found — cannot confirm the driver version (need >= 570.86)."
fi

echo ">> launching the venus measurement guest (host GPU = NVIDIA via $NVIDIA_ICD)"
echo ">> once booted, run the 4-rung ladder in the guest (see docs/spikes/venus-nvidia-a5000.md):"
cat <<'RUNGS'
     Rung 1  vulkaninfo | grep -E 'driverID|deviceName|apiVersion'
             EXPECT driverID=VK_DRIVER_ID_MESA_VENUS, deviceName='NVIDIA RTX A5000', apiVersion>=1.3
             (a failure = NVIDIA missing a required host extension; read VN_DEBUG=init host log)
     Rung 2  vkcube   + on the HOST: nvidia-smi dmon   (the qemu PID must show non-zero GPU-Util)
     Rung 3  (THE CRUX) a HOST_VISIBLE|HOST_COHERENT compute round-trip, byte-correct readback
             (NVIDIA-Venus's historical weak point: host-visible dma-buf export)
     Rung 4  wine + DXVK d3d11-triangle, DXVK_HUD=devinfo shows the Venus device
             (de-risks the whole Windows/D3D path on Linux with zero WDK work)
     GO iff all four pass. Rung 1 or 3 fail on the pinned driver => NO-GO for Path A (Venus-on-NVIDIA)
        => fall back to the own-decoder path (3D ADR "Fallback") or pivot host silicon.
RUNGS

# In-guest, force Mesa's venus ICD:
#   VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json VN_DEBUG=init vulkaninfo
exec env VK_DRIVER_FILES="$NVIDIA_ICD" "$QEMU" \
  -enable-kvm -cpu host -smp 4 \
  -object "memory-backend-memfd,id=mem1,size=${MEM_GB}G,share=on" \
  -machine "q35,memory-backend=mem1" \
  -device "virtio-gpu-gl,blob=true,venus=true,hostmem=${HOSTMEM_GB}G,max_hostmem=${HOSTMEM_GB}G" \
  -display "$SPIKE_DISPLAY" \
  -drive "file=${GUEST_IMG},if=virtio,format=qcow2" \
  -device virtio-net-pci,netdev=n0 -netdev user,id=n0 \
  "${@:2}"
