# infinigpu — Windows guest driver (skeleton)

> **Status: unbuilt skeleton.** This code has **not** been compiled or run — there is no
> Windows/WDK environment in the dev host it was written on. It is a structurally-correct
> starting point (IddCx callback flow + the infinigpu host-handoff seam), not a finished
> driver. Everything below marked **⚠ validate on Windows** needs a Windows 11 + WDK build
> and a real guest VM to confirm.

## What the Windows guest needs to do

Same job as the Linux `guest/linux/infinigpu.c` DRM/KMS driver, but on Windows: present the
guest desktop through our virtual GPU so the host can scan it out and stream it. On Linux
that's one DRM driver on our PCI device. On Windows it is **two** cooperating pieces, because
the framework that captures the desktop (IddCx, user-mode) is not the one that may touch our
PCI device (kernel-mode):

```
 ┌─ Windows guest ─────────────────────────────────────────────┐
 │  Desktop / DWM                                               │
 │     │ composits                                              │
 │     ▼                                                        │
 │  infinigpu-idd  (IddCx indirect display driver, UMDF, C++)  │  ← this skeleton
 │     │ acquires each frame as a D3D11 texture                 │
 │     │ HostLink::SubmitFrame(bgra, w, h)                      │
 │     ▼                                                        │
 │  infinigpu-kmdf (KMDF companion that owns the PCI device)   │  ← documented, not yet written
 │     • maps BAR0, allocates the scanout framebuffer           │
 │     • writes the framebuffer's guest-physical addr to the    │
 │       device via a DISPLAY_SCANOUT command (infinigpu-abi)   │
 └─────────────────────────────────────────────────────────────┘
                     │ vfio-user (host) reads the framebuffer, encodes → infiniPixel
                     ▼
                   host infinigpu-device
```

Why two pieces:
- **IddCx** (Indirect Display Driver, `IddCx*` API) is the modern, supported way to add a
  virtual monitor. It is **user-mode** (UMDF) and the OS hands it composited desktop frames as
  Direct3D textures — exactly what we want to capture, with no WDDM miniport to write. But a
  UMDF driver **cannot** map a PCI BAR or issue DMA.
- So the frames must cross to a **KMDF companion** that binds our PCI device, maps BAR0, owns
  the scanout framebuffer, and pokes the device registers — the direct analogue of the Linux
  driver's `DISPLAY_SCANOUT` page-flip. The two are linked by a private IOCTL + shared memory
  (`HostLink` in this skeleton).

The roadmap (README of the repo) is **IddCx display-only first** (get a desktop on screen),
**then** a full WDDM render miniport for 3D (DXVK/vkd3d in-guest map D3D→Vulkan onto our
device). This directory is the IddCx display-only rung.

## The shared contract (must match the host + Linux guest)

The Windows side speaks the **same** BAR0 register map and wire commands as
`crates/infinigpu-abi` and `guest/linux/infinigpu.c`:
- BAR0 control registers (`DEV_MAGIC`, `GLOBAL_CTRL`, per-context ring config, doorbell).
- The `DISPLAY_SCANOUT` command carrying a `ScanoutPresent { width, height, pitch, format,
  scanout_addr }` (24 bytes, LE) — one per page-flip.
`abi/infinigpu_abi.h` here is the **cbindgen-generated** C header from `infinigpu-abi`
(`scripts/gen-abi-header.sh` in the repo root) — regenerate it, don't hand-edit, so the three
languages never drift. ⚠ the KMDF companion includes it; the IddCx driver only needs the pixel
format constants.

## Files

| File | Role |
|---|---|
| `infinigpu-idd/Driver.cpp` | The IddCx driver: WDF entry, adapter init, monitor arrival, and the swap-chain processing thread that acquires frames and calls `HostLink::SubmitFrame`. |
| `infinigpu-idd/HostLink.h` | The seam to the KMDF companion (submit a captured frame). Two impls: a real IOCTL one (⚠ needs the companion) and a no-op bring-up stub. |
| `infinigpu-idd/infinigpu-idd.inf` | Install information (indirect display class). |
| `infinigpu-idd/infinigpu-idd.vcxproj` | MSBuild/WDK project (key settings; ⚠ verify against your WDK version). |

## Build (⚠ validate on Windows)

Needs Windows 11, Visual Studio 2022 + the **Windows Driver Kit (WDK)** matching your SDK.

```bat
:: from a "x64 Native Tools" prompt with the WDK installed
msbuild guest\windows\infinigpu-idd\infinigpu-idd.vcxproj /p:Configuration=Release /p:Platform=x64
:: test-signing (dev only): enable test signing + install
bcdedit /set testsigning on   :: then reboot
pnputil /add-driver infinigpu-idd.inf /install
```

Delivery mirrors `infiniservice`: the backend serves the signed driver package to the guest,
which installs it on first boot. Production needs an **attestation-signed** (or WHQL) package —
test-signing is dev-only.

## What is NOT done (needs a Windows env)

1. **Compile it.** No WDK here — expect API/type fixes against your exact WDK headers.
2. **The KMDF PCI companion** (`infinigpu-kmdf`) — owns the device, maps BAR0, writes
   `DISPLAY_SCANOUT`. Without it, `HostLink` is a stub and no pixels reach the host.
3. **Signing + INF class** verification, and the backend serve/install path for Windows.
4. **WDDM render miniport** (the 3D rung) — DXVK/vkd3d → our device. Phase 2–3.
