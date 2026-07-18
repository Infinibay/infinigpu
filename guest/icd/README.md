# infinigpu guest Vulkan ICD

Our own thin Vulkan **ICD** (Installable Client Driver) so guest apps drive the A5000 through
the infinigpu device instead of falling back to lavapipe (CPU). Full plan + rationale:
[`../../docs/adr/GUEST-ICD-IMPLEMENTATION.md`](../../docs/adr/GUEST-ICD-IMPLEMENTATION.md).

Mesa's Vulkan runtime is static-only / never installed, so there is **no out-of-tree build**.
The driver source lives **here** and is injected into a pinned Mesa checkout as
`src/infinigpu/vulkan/` by `build.sh` — we don't vendor the Mesa tree.

## Status — Phase 0 DONE (loads + enumerates + create-device)

`vulkaninfo` binds the ICD and reports `deviceName = infinigpu (A5000 remote)` (not lavapipe).
The ~10 bring-up entrypoints are implemented; everything else resolves to weak-NULL + `vk_common_*`.
Phase 1 (first triangle, no WSI) is next — see the ADR.

## Build

Prereqs (no sudo needed — extract from `.deb` into a prefix if not installed): a Mesa **tag
`mesa-25.0.7`** checkout at `~/.cache/infinigpu/mesa`, `meson`/`ninja`/`gcc`, python `mako`, and
`libdrm` on `PKG_CONFIG_PATH`.

```bash
# one-time: clone the pinned Mesa (blobless sparse is enough)
git clone --filter=blob:none --sparse --branch mesa-25.0.7 \
  https://gitlab.freedesktop.org/mesa/mesa.git ~/.cache/infinigpu/mesa
git -C ~/.cache/infinigpu/mesa sparse-checkout set src include bin

# build the driver (injects into Mesa, registers -Dvulkan-drivers=infinigpu, builds)
PKG_CONFIG_PATH=/path/to/libdrm/pkgconfig ./build.sh
```

Output: `~/.cache/infinigpu/mesa/build-infinigpu/src/infinigpu/vulkan/libvulkan_infinigpu.so`
+ `infinigpu_devenv_icd.<arch>.json`.

## Smoke-test on the host

The real infinigpu DRM node only exists inside a guest VM; a bare host has none. Set
`INFINIGPU_SMOKE_ANY_NODE=1` to fabricate a device (no backing fd — Phase 0 renders nothing) so the
whole load → enumerate → create-device → property-query path can be exercised here:

```bash
BUILD=~/.cache/infinigpu/mesa/build-infinigpu
VK_DRIVER_FILES=$BUILD/src/infinigpu/vulkan/infinigpu_devenv_icd.$(uname -m).json \
INFINIGPU_SMOKE_ANY_NODE=1 vulkaninfo --summary   # → deviceName = infinigpu (A5000 remote)
```

Inside the guest (real infinigpu renderD128) drop `INFINIGPU_SMOKE_ANY_NODE`; the name check binds
the actual device. `INFINIGPU_DEBUG=1` traces bring-up.
