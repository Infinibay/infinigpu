# Deploying infinigpu into the Infinibay dev stack

The infinigpu ↔ Infinibay wiring (`docs/INTEGRATION.md`) is implemented across the
app repos. This is the operator recipe to turn it on in the **iby dev stack** and
render a real GPU VM on the host's NVIDIA GPU, viewed with the native
`infinigpu-viewer`.

Everything is **opt-in and gated on `department.gpuEnabled`** — until you enable a
department and create a VM under it, none of this changes existing behavior.

Branch note: the app-repo changes live on `feat/infinigpu-integration` (backend,
infinization). infinigpu itself is on `main`.

---

## Stage A — activate the control plane (no GPU needed)

The Prisma migration (7 `Department` GPU-policy columns) and the regenerated
client are applied by the backend entrypoint on restart:

```bash
iby restart backend      # runs prisma migrate + generate, rebuilds infinization
```

Then the GPU GraphQL surface is live and usable (still no real GPU yet):

- `updateDepartmentGpuPolicy(input:{departmentId, gpuEnabled:true, vramCapMB, …})`
- `departmentGpuPolicy(departmentId)`, `gpuFleetView`, `attachGpu`, `gpuConsoleStream`

This is enough to verify policy + admission plumbing before touching hardware.

## Stage B — the render path (root, one-time host prep)

1. **NVIDIA in the container (CDI).**
   ```bash
   sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml
   nvidia-ctk cdi list            # expect nvidia.com/gpu=all
   ```
2. **QEMU with vfio-user-pci** (QEMU ≥ 10.1.1):
   ```bash
   ( cd repos/infinigpu && ./scripts/build-qemu-vfio-user.sh )   # → /opt/qemu-vfio-user
   ```
3. **The device-server binary** (no root):
   ```bash
   ( cd repos/infinigpu && cargo build --release -p infinigpu-device )
   ```
4. **Bring the stack up with the GPU override** (adds the GPU device, the
   vfio-user QEMU, the binary, and publishes the infiniPixel relay ports):
   ```bash
   docker compose --env-file .env.docker \
     -f docker-compose.yml -f docker-compose.kvm.yml \
     -f repos/infinigpu/deploy/docker-compose.gpu.yml up -d
   ```
   (See `deploy/docker-compose.gpu.yml` for the env/volumes and the non-CDI
   fallback. `iby` does not yet wire this override — drive compose directly for now.)

## Stage C — enable a department + create a GPU VM

1. Enable GPU for a department (GraphQL): `updateDepartmentGpuPolicy(input:{ departmentId:"…", gpuEnabled:true, vramCapMB:4096, gpuTimeWeight:1 })`.
2. Create a **Linux** VM in that department the normal way. On create, infinization:
   starts a per-VM `infinigpu-device` server, attaches the vfio-user GPU (`-vga
   none`, so **no SPICE** — the display is infiniPixel), and the backend broker
   admits it (fail-closed) + allocates a pixel port.
3. In the guest, load the display driver (first-boot delivery like infiniservice
   is a follow-up — for now build + insmod it manually):
   `make -C guest/linux && sudo insmod guest/linux/infinigpu.ko` → `/dev/dri/card0`.

## Stage D — connect with the native viewer

Get the stream URL, then point the (already-built) native client at it:

```bash
# GraphQL: gpuConsoleStream(machineId:"…") → { url: "ws://<host>:612x", pixelPort }
repos/infinigpu/target/release/infinigpu-viewer --url ws://<host>:<port>
# headless decode-only smoke test (no window): --headless --frames 60
```

The viewer is winit + Vulkan (Wayland on Linux, Win32 on Windows) + openh264 — no
GTK, no Qt. Build it with `cargo build --release -p infinigpu-viewer`.

---

## Known gaps / follow-ups

- **Guest driver delivery**: `infinigpu.ko` is loaded manually above; serving it to
  the guest on first boot (like the infiniservice binary) is not wired yet.
- **Host QEMU portability**: `deploy/docker-compose.gpu.yml` bind-mounts the host
  `/opt/qemu-vfio-user` build into the container; if its libc/glib differs, build
  QEMU against the container base instead.
- **Broker is in-memory**: a backend restart forgets admission tickets of already-
  running GPU VMs (the ledger under-counts until they stop). Startup
  reconciliation is a follow-up.
- **Multi-node**: admission runs on the master; GPU VMs on remote compute nodes
  are not wired (the device server + broker would run on the node).
- **Frontend**: the browser WebCodecs console (a rung in the console ladder) is not
  wired — the native viewer is the client. `gpuConsoleStream` already returns the URL.
