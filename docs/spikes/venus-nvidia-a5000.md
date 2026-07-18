# Spike: does Venus drive NVIDIA's proprietary Vulkan on the A5000?

**Status:** NOT YET RUN. This is the Phase-0 go/no-go gate for
[`docs/adr/3D-ACCEL-IMPLEMENTATION.md`](../adr/3D-ACCEL-IMPLEMENTATION.md). **No host-decoder work
(`crates/infinigpu-replay/src/venus/`, the BAR2 aperture, the guest render node's Venus binding)
starts until this records a GO.** Run with `scripts/spike-venus-nvidia.sh /path/to/guest.qcow2`.

## Why this exists

The entire 3D plan reuses **Mesa Venus** (guest) + **virglrenderer-venus** (host decoder) to run
guest Vulkan on the A5000. Venus is CI-validated mostly on Intel/AMD Mesa hosts; NVIDIA-proprietary
as a Venus *host* is the unproven load-bearing assumption. This spike answers it with the **stock**
stack (no infinigpu code), so a NO-GO kills the reuse premise cheaply — before weeks of decoder work —
and forces the ADR's fallback (own `vn_protocol_renderer` / own thin guest ICD) or an AMD/Intel-first
pivot.

## Preconditions

- [ ] **Host NVIDIA driver ≥ 570.86** (the Mesa-documented Venus-host floor). The installed baseline
  was **550.163.01**, which is BELOW the floor — a spike on it is a **guaranteed false NO-GO**. Pin
  ≥ 570.86 (fleet baseline 570.153.02 or 575.x), reboot, confirm with `nvidia-smi`.
- [ ] Distro `qemu-system-x86_64` with `virtio-gpu-gl` (venus=), virglrenderer built `-Dvenus=true`,
  `/usr/share/vulkan/icd.d/{nvidia_icd,virtio_icd.x86_64}.json` present, `/dev/kvm`.
- [ ] Guest = Ubuntu 25.04+ (virtio-gpu already `DRIVER_RENDER` → `/dev/dri/renderD128` exists) with
  Mesa 25.x venus ICD; in-guest force `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json`.

## The four-rung ladder

| Rung | Workload (in guest) | Pass criterion | Result | Notes |
|------|---------------------|----------------|--------|-------|
| 1 | `VN_DEBUG=init vulkaninfo` | `driverID=VK_DRIVER_ID_MESA_VENUS`, `deviceName='NVIDIA RTX A5000'`, `apiVersion≥1.3` (NOT llvmpipe/lavapipe) | ☐ | on fail, read the missing host extension from the `VN_DEBUG=init` host log |
| 2 | `vkcube` + host `nvidia-smi dmon` | the qemu PID shows non-zero GPU-Util/VRAM (silicon, not llvmpipe) | ☐ | |
| 3 **(crux)** | `HOST_VISIBLE\|HOST_COHERENT` compute round-trip | GPU writes guest-mappable memory; `memcpy` readback byte-correct | ☐ | NVIDIA-Venus's historical weak point (host-visible dma-buf export) — this is what DXVK/vkd3d staging buffers need |
| 4 | `wine` + DXVK `d3d11-triangle`, `DXVK_HUD=devinfo` | renders; HUD shows the Venus device | ☐ | de-risks the whole Windows/D3D path on Linux with zero WDK work |

## Decision

> **GO** iff all four rungs pass on a host pinned ≥ 570.86.
> **NO-GO for Path A** if Rung 1 or Rung 3 fails → take the 3D-ADR **Fallback** (own
> `vn_protocol_renderer` keeping stock guest Mesa, or own thin guest ICD) **or** pivot host silicon
> to AMD/Intel-first (Path A works there unchanged).

- **Driver version tested:** _(fill in)_
- **Decision:** ☐ GO ☐ NO-GO
- **Negotiated NVIDIA host-extension set** (feeds the driver-skew compat matrix): _(fill in — the
  set of extensions Venus required and NVIDIA provided; from the `VN_DEBUG=init` host log)_
- **Date / operator:** _(fill in)_

On GO: authorize `crates/infinigpu-replay/src/venus/` + the BAR2 `HOST_VISIBLE` aperture (3D-ADR
Phase 2). On NO-GO: record which rung failed and the exact missing extension, then open the fallback.
