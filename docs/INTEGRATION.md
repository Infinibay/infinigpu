# Integrating infinigpu into Infinibay

This is the concrete, ready-to-implement blueprint for wiring the (working) infinigpu stack into
the Infinibay repos (`infinization`, `backend`, `frontend`). It is a **gated, opt-in feature**: a
VM gets a virtual GPU only when its department enables it, so existing VMs are unaffected. Nothing
here is speculative — every host-side piece it depends on already exists and is tested in this repo.

Derived from `PHASE-0-PROTOTYPE.md` (touch-points), ADR-0007 (§Infinibay mapping) and ADR-0009
(§Peripheral channels & integration).

## 0. Deployment prerequisite

Build + place the host artifacts (mirrors how `infiniservice` binaries are served):

- `cargo build --release -p infinigpu-device` → the vfio-user device server binary.
- Build QEMU ≥ 10.1.1 with the upstream `vfio-user-pci` client (`scripts/build-qemu-vfio-user.sh`),
  or use a distro QEMU that ships it. `infinization` must invoke **that** QEMU for GPU VMs.
- The guest `infinigpu.ko` (+ `drm_dma_helper.ko` on modular kernels) is delivered to the guest the
  same way `infiniservice` is (served over REST, installed on first boot). Windows is Phase 2+.

## 1. `infinization` — attach the device (QemuCommandBuilder)

Add an **opt-in** `addInfinigpuDevice(vmId, opts)` to `infinization/src/core/QemuCommandBuilder.ts`,
called from the VM-create path **only when `opts.gpu` is set**. It must add, in order:

```
# 1. a memfd-backed RAM so the device can mmap guest RAM zero-copy (share=on is mandatory):
-object memory-backend-memfd,id=mem0,share=on,size=<guestRamBytes>
-machine q35,accel=kvm,memory-backend=mem0

# 2. no default VGA, so our device is the guest's only display (fbcon binds fb0 to us):
-vga none

# 3. the vfio-user device, JSON form (socket is a SocketAddress union → flat form is rejected):
-device '{"driver":"vfio-user-pci",
          "socket":{"path":"${INFINIZATION_SOCKET_DIR}/<vmId>.gpu.sock","type":"unix"},
          "x-pci-class-code":229376,
          "x-no-posted-writes":true}'
```

**`x-no-posted-writes:true` is mandatory** — the `vfio_user` v0.1.3 server always replies to
`REGION_WRITE`, but QEMU posts MMIO writes by default (expects no reply) → protocol desync the
moment a guest driver writes a BAR. `x-pci-class-code:229376` = `0x038000` (Display-Other).
`-vga none` is required or the guest's fbcon binds to QEMU's default VGA instead of us.

## 2. `infinization` — per-VM device-server lifecycle

A new host-side hook (mirrors the SPICE-proxy lifecycle) spawns/reaps the device server alongside
VM start/stop:

- **On VM start (GPU VM):** `spawn infinigpu-device --socket ${INFINIZATION_SOCKET_DIR}/<vmId>.gpu.sock --vm-id <vmId>`
  **before** launching QEMU (QEMU connects to the socket at boot). Pass policy + streaming via env:
  - `INFINIGPU_PIXEL_PORT=<port>` to enable the infiniPixel stream for this VM (allocate from a
    pool like the SPICE ports; omit to disable streaming).
  - (future) broker policy env / a shared broker socket — see §3.
- **On VM stop/crash:** SIGTERM the device server; it drops its admission ticket (reaps VRAM +
  concurrency slot) and exits. Wire into the backend's VM crash-reconciliation like other services.
- The server must run with GPU access (the same group/permissions the render path needs).

> Today each device server is its own process with its own `GpuBroker`. For real cross-VM
> scheduling, run **one** broker per host and have the per-VM servers share it — either a shared
> broker process the servers RPC into, or (the ADR-0003 north star) one jailed **replay process per
> VM** reporting to a host `GpuBrokerService`. Both are post-blueprint; the `serve_with_broker()`
> entry point and the GPU-agnostic `infinigpu-sched` crate already anticipate this.

## 3. `backend` — policy, RBAC, and the broker (ADR-0007)

- **7 new `Department` Prisma fields** (canonical set): `gpuEnabled` (default false),
  `vramReserveMB`, `vramCapMB`, `priorityTier`, `maxConcurrentGpuVMs`, `gpuTimeWeight`,
  `submissionRateTokens`. `Machine` already has `gpuPciAddress`/`departmentId`/`nodeId`.
- **RBAC-gated `attachGpu` mutation** → a **`GpuBrokerService`** singleton (mirrors
  `InfinizationService`). It owns the host `GpuBroker` policy and, at GPU-attach, maps a
  department's fields to `infinigpu-sched`'s `VmConfig` (`weight = gpuTimeWeight`, `vram_cap_mb =
  vramCapMB`, `priority = priorityTier`) and `BrokerConfig` (`total_vram_mb`, `vram_reserve_mb =
  vramReserveMB`, `max_concurrent_gpu_vms = maxConcurrentGpuVMs`). The broker's `admit()` result
  gates the VM start (fail-closed).
- **Telemetry** (FleetView per-VM GPU-time / VRAM / throttle) rides the existing Socket.IO
  health-slice bridge, like other real-time metrics.

## 4. `backend` + `frontend` — the remote display (ADR-0009)

- A new **`encoded-console-stream`** service beside `SpiceProxyService.ts`, reusing its port/auth/
  session scaffolding. It proxies the device server's infiniPixel WebSocket (`INFINIGPU_PIXEL_PORT`)
  to the browser (auth + session binding), exactly as the SPICE proxy does for `.vv`.
- **`frontend`**: ship `client/infinipixel.html`'s logic as a console component — a WebCodecs
  `VideoDecoder` (H.264) rendering the stream to a canvas. It becomes a new rung in the console
  fallback ladder: **infiniPixel (HW NVENC) → software-x264 → SPICE** (legacy/thin clients).

## 5. Rollout ladder

1. Land the 7 Prisma fields + `attachGpu` RBAC (low-risk, gates everything). No behavior change yet.
2. `QemuCommandBuilder.addInfinigpuDevice()` + the lifecycle hook, behind `department.gpuEnabled`.
   Validate a single Linux GPU VM boots with `/dev/dri/card0` (the `guest-kms-test.sh` path, live).
3. Turn on `INFINIGPU_PIXEL_PORT` + the `encoded-console-stream` proxy + the frontend viewer.
4. One shared broker per host + admission enforcement across VMs (the density thesis).
5. Per-VM jailed replay process + NVML attribution (ADR-0003) — the isolation upgrade.

Each rung is independently shippable and reversible (flip `gpuEnabled` off).
