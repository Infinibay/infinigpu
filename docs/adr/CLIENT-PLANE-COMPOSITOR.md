# Client-Side Plane Compositor — Implementation Plan

**Status:** Proposed (2026-07-18). Design synthesized from an adversarial multi-agent
review of the real tree (ABI, device, pixel Hub, relay, viewer, guest driver) and reconciled
against [`2D-ACCEL-IMPLEMENTATION.md`](2D-ACCEL-IMPLEMENTATION.md). **This ADR rewrites that
plan's PR6** — see [Reconciliation](#reconciliation-with-the-2d-accel-adr).

## Context

The infinigpu remote-display pipeline is 100% our own code, guest driver to native viewer:

```
guest DRM/KMS (software cursor rasterized INTO the framebuffer)
  → vfio-user DISPLAY_SCANOUT (whole-FB present)
  → infinigpu-device (DMA-read FB, CPU repack BGRA, feed PixelStreamer)
  → infinigpu-pixel (ffmpeg h264_nvenc 30 fps, Hub fan-out over WS pixelPort 7000)
  → backend GpuConsoleRelay (WS relay 6120: frames down, input JSON up → QMP input-send-event)
  → native viewer (winit + ash/Vulkan + openh264: decode + blit)
```

Confirmed against the tree: the guest driver uses `drm_simple_display_pipe`
(`guest/linux/infinigpu.c:96,355`), which by construction exposes **exactly one implicit
PRIMARY plane** — no cursor plane. So weston / mutter / Xorg-modesetting have nothing to
offload to, and they **paint the cursor sprite into the primary shadow/scanout buffer** on
every pointer move. That dirties the framebuffer → a full `igpu_submit_scanout` → the device
DMA-reads the whole FB (`present_scanout`, `crates/infinigpu-device/src/lib.rs`) → scalar
BGRA repack → nvenc → stream → client decode.

### The recurring-cursor-lag problem

**Because the cursor is a guest-software-rendered pixel baked into the streamed
framebuffer, every cursor motion carries the FULL glass-to-glass pipeline latency:** input
round-trip to the guest + host encode + network stream + client decode. The observed result
is a recurring **~0.5–1 s cursor lag** that dominates the interactive feel of the console,
independent of codec, 3D, or bandwidth. This is the same problem SPICE client-mouse-mode and
RDP AVC solve by drawing the cursor locally at the client.

The fix: **make the viewer a client-side multi-plane compositor.** Draw the cursor as a local
Vulkan overlay at the local pointer position — which the viewer *already knows* from the
`{"t":"m"}` it sends upstream — so cursor motion has zero network latency. Feed the cursor
*shape* to the viewer by a lightweight guest→device→viewer sideband. The same plane framework
later enables **media redirection** (forward a guest video sub-region's original bitstream to
a viewer decode-into-overlay, avoiding a double transcode).

### Two wires — the load-bearing distinction

Do not conflate them:

- **Guest→device ABI** (`infinigpu-abi`, zerocopy `#[repr(C)]` on the vfio-user rings) — where
  `msg_type::CURSOR_UPDATE = 0x0042` lives. **Concrete per plane type.** An ARGB cursor sprite
  by guest-phys addr and a compressed video sub-region share almost nothing on the guest
  production side, so this struct is *not* generalized.
- **Device→viewer sideband** (a new typed family on the existing infiniPixel WS) — where the
  reusable "plane" abstraction is real and cheap to define once. This is the multi-plane
  transport; the cursor is its first `plane_kind`.

The backend relay (`GpuConsoleRelay.ts`) is a **verbatim, opcode-preserving, codec-blind**
forwarder downstream (`upstream.on('message', … client.send(data,{binary:isBinary}))`,
`:204-213`) and a QMP input injector upstream. It needs **zero code changes for the sideband
and for last-writer-wins v1 input**; the aspect-fit coordinate fix (D5/M6) and the deferred
multi-viewer input token (D7) and relative-motion injection (D3) are the *only* rungs that
touch it, and each is called out as an explicit relay change — not smuggled under a "no relay
change" claim (see [D10](#d10--the-zero-relay-change-invariant-is-scoped-to-v1)).

## Decisions

### D1 — Client vs server mouse mode is a per-VM, self-configuring property (not per-viewer)

The single organizing decision that collapses most of the "mouse mode" complexity, grounded in
one hard constraint: **there is exactly one encoded framebuffer stream per VM**
(`serve_with_broker` → one `InfinigpuBackend` → one `PixelStreamer`/`Hub` per QEMU socket). You
cannot simultaneously hand one viewer a cursor-free frame (client composite) and another a
cursor-baked frame (host composite). Therefore:

- **Composite location is a per-VM property**, decided at the guest/device at capability time,
  never a per-viewer runtime toggle.
- **The viewer self-configures from the wire, gated by a capability hello.** If a viewer
  *announced sideband support* (see [D9](#d9--client-capability-hello-mixed-version-safety))
  and then receives cursor sideband messages, it hides the OS cursor and draws the local
  overlay. If it never announces, or never receives sideband, it shows the OS cursor and the
  guest's baked cursor rides the video as today. **Sideband presence is the mode signal *among
  announced-capable clients*** — the one-message hello (D9) is the necessary retreat from a
  pure zero-handshake design that mixed client/device versions in the field force.
- Because we own every client (native viewer + `client/infinipixel.html`), **client mode is
  universal whenever the guest cursor plane is active and clients are capable.** Host-side
  composite (the ADR's original PR6 step) is demoted to an optional, deferred fallback for a
  cursor-plane guest paired with a non-overlay viewer — which does not occur in our fleet.

Three modes exist, in fail-safe order:

| Mode | Guest FB | Viewer | Latency | When |
|---|---|---|---|---|
| **Server** (fallback, today) | cursor baked in | shows OS cursor | full pipeline | `caps::CURSOR_PLANE` clear / kernel < 6.6 / client didn't announce / compositor SW-cursor frame (D4) / relative-mode app before PR-C7 |
| **Client** (target, default when active) | cursor-free | local overlay at local pointer | **zero** | guest cursor plane active + announced-capable viewer |
| **Host-composite** (deferred) | cursor-free | thin client | full pipeline | future non-overlay client (PR-C6) |

In **all** modes the guest *logical* pointer is still driven by absolute QMP `input-send-event`
coordinates — the mode decides only *where the cursor is drawn*, never how absolute input is
injected. (Requires an absolute pointing device — usb/virtio-tablet — in the GPU VM's QEMU
argv; an infinization precondition, not a relay concern.) **Relative-mode apps are the one
exception** and require a separate path frozen now — see [D3](#d3--relative-pointer--pointer-lock-freeze-the-hooks-now).

### D2 — Freeze the `CURSOR_UPDATE` body now (48 bytes) — including the relative/warp flags

`msg_type::CURSOR_UPDATE = 0x0042` is reserved at `crates/infinigpu-abi/src/wire.rs` with an
unfrozen body (the 2D-ADR deliberately deferred it). We freeze it **now** — pulling the freeze
earlier than the ADR's PR6 — so device/viewer work can proceed against a stable contract. Model
it on `ScanoutPresentDamaged`; use a **guest-physical shape address** so it has **no dependency
on PR4's `ResourceTable`** and can ship before PR4.

**The body layout (48 bytes) is frozen exactly as below.** All mode dynamics the critique
surfaced — relative-pointer lock (D3), cursor warp/teleport (M3), and the compositor HW↔SW-toggle
double-cursor state machine (D4) — are expressed **entirely inside the existing `flags: u32`**,
so no field, size, or offset changes. Freezing these flag *bits* now (they are free and additive)
is what forecloses an expensive re-freeze after the 48-byte body ships.

Insert after the `ScanoutPresentDamaged` block in `wire.rs`:

```rust
/// `CURSOR_UPDATE` (msg_type 0x0042) body. The guest reports its cursor plane out-of-band so
/// the cursor leaves the primary framebuffer. The device forwards it to a client-side overlay
/// (this design) OR composites it host-side (deferred fallback). Position/hotspot are carried
/// for the fallback, for view-only viewers, and for WARP correction even though a driving
/// client-composite viewer normally draws at its own local pointer. Additive, ABI 0.3.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct CursorUpdate {
    pub scanout_id: u32,  // 0x00  which head (MAX_SCANOUTS=1 today; cheap future-proof)
    pub flags: u32,       // 0x04  cursor_flags::*
    pub pos_x: i32,       // 0x08  crtc_x — SIGNED (hotspot pushes origin < 0 at screen edge)
    pub pos_y: i32,       // 0x0C  crtc_y
    pub hot_x: u16,       // 0x10  hotspot within the sprite
    pub hot_y: u16,       // 0x12
    pub width: u16,       // 0x14  sprite dims (0 when MOVE_ONLY / hidden)
    pub height: u16,      // 0x16
    pub pitch: u32,       // 0x18  bytes/row of the ARGB sprite (validated ≥ width*4)
    pub format: u32,      // 0x1C  reuse format::B8G8R8A8 (premultiplied — see flag)
    pub shape_ref: u64,   // 0x20  guest-phys addr OR res_id (see SHAPE_BY_RESID)
    pub _reserved: u64,   // 0x28  additive headroom (0 today)
}   // size = 0x30 = 48, align 8, zero internal padding
```

Offsets are padding-free: `pos_x@8`, `hot_x@16`, `pitch@24`, `format@28`, `shape_ref@32`
(u64 8-aligned), `_reserved@40`, **size 48**.

Flags module beside `desc_flags` (`wire.rs:~98`) — **all bits frozen in PR-C1**:

```rust
pub mod cursor_flags {
    pub const VISIBLE: u32        = 1 << 0; // clear = HIDE (text caret, plane HW→SW handoff, relative-lock)
    pub const MOVE_ONLY: u32      = 1 << 1; // only pos_* fresh; retain last shape
    pub const SHAPE_BY_RESID: u32 = 1 << 2; // shape_ref is a PR4 res_id, else a guest-phys addr
    pub const PREMULTIPLIED: u32  = 1 << 3; // sprite alpha is premultiplied (DRM default)
    pub const WARP: u32           = 1 << 4; // pos_* is an authoritative TELEPORT — driving viewer MUST snap (M3)
    pub const RELATIVE: u32       = 1 << 5; // guest wants pointer-lock/relative mode (LOCKED); overlay hides, viewer grabs (D3)
}
```

Rationale for each contested field:

- **Shape by reference, not inline.** `shape_ref` is DMA-read exactly like
  `ScanoutPresent.scanout_addr`. Cursors are tiny and change shape rarely; inline ARGB via
  `desc_flags::INLINE` forces a 4–16 KiB memcpy into the ring data region every DEFINE. Rejected.
- **res_id vs guest-phys addr, resolved additively.** One `shape_ref` + a `SHAPE_BY_RESID` flag:
  clear = guest-phys addr (works **today**, pre-PR4, no `ResourceTable`); set = res_id into PR4's
  table. Closes the 2D-ADR open question **without a struct re-freeze** when PR4/PR6 land.
- **Position stays in the ABI even for client-cursor.** `i32` because DRM `crtc_x/crtc_y` go
  negative at screen edges (`u32` would silently drop edge cursors). It feeds (a) the
  host-composite fallback, (b) **view-only viewers** in the multi-client case (D7), and (c)
  **WARP correction** (M3): the driving viewer normally ignores position, but on a `WARP` MOVE
  it snaps its pointer notion to `pos_x/pos_y` so a guest-initiated teleport (menu recenter,
  "snap to default button", `XWarpPointer`) does not desync clicks. This is the entire
  justification for carrying position end-to-end.
- **`MOVE_ONLY` is coalesced and conditionally forwarded, never blanket-dropped.** The device
  retains a per-VM `CursorState`. It **suppresses** pure MOVE_ONLY *only* in the single-viewer,
  non-`WARP` common case; it **forwards coalesced MOVE** (latest-wins, capped ~30–60/s) whenever
  `clients > 1` (view-only viewers need it, D7) **or** the `WARP` bit is set (M3). This resolves
  the D2↔D7 contradiction the critique found (M1): view-only viewers' cursors never freeze, and
  warps always reach the driving viewer. Coalescing lives in the device, not the guest.
- **`WARP` / `RELATIVE`** are frozen now precisely because re-freezing the body later is
  expensive; their runtime behavior is specified in [D3](#d3--relative-pointer--pointer-lock-freeze-the-hooks-now) / [M3](#m3--cursor-warp).
- **No new pixel format.** DRM standard cursor = premultiplied ARGB8888 = `format::B8G8R8A8`.
  Reuse it; `PREMULTIPLIED` documents the blend the viewer must match. The device **accepts only
  `B8G8R8A8`** and fails closed on any other `format` (D-DoS below).

### D3 — Relative-pointer / pointer-lock: freeze the hooks now, implement later

The input wire is **absolute-only** (`{"t":"m",x,y}` → QEMU `abs` axes, `GpuConsoleRelay.ts:363-364`).
Relative-mode apps (FPS games, `pointer-constraints`/`XGrabPointer`, orbit/pan in Blender/CAD)
need **relative deltas** with the OS pointer warped/locked. A client cursor that only *hides* via
`VISIBLE=0` leaves such apps **unplayable**, not merely cursorless — there is no relative motion
at all. Because the ABI is frozen in PR-C1, the hooks must be reserved *now*; the implementation
is deferred but not foreclosed.

Frozen now (PR-C1), implemented in PR-C7:

- **`cursor_flags::RELATIVE` (LOCKED)** in the 48-byte body (D2).
- **A relative-motion upstream input message shape**: `{"t":"mr","dx":<i32>,"dy":<i32>}`
  alongside the existing `{"t":"m"}`. Reserving the shape now keeps the relay's message parser
  additive.

Specified runtime behavior (deferred implementation):

- Guest enters relative mode (a `pointer-constraints` lock / `XGrabPointer`) → cursor
  `atomic_update` emits `CURSOR_UPDATE` with `RELATIVE` set and `VISIBLE` clear.
- Viewer, on `RELATIVE`: `Window::set_cursor_grab(Locked/Confined)`, hide the overlay, and send
  `{"t":"mr",dx,dy}` from raw device deltas instead of absolute `{"t":"m"}`.
- Relay injects QEMU `rel` events (`type:'rel'`, axes `x`/`y`) for `mr` messages.

Until this lands, **relative-mode apps regress under a cursor-plane guest**, and **server mode
(baked cursor, absolute input) is the only working path for them.** Because mode is per-VM (D1),
a fleet policy can pin known relative-heavy VMs (game/CAD templates) to server mode by clearing
`caps::CURSOR_PLANE` on that VM until PR-C7 ships. Document this loudly.

### D4 — Double-cursor avoidance: the compositor HW↔SW toggle state machine

The naive assumption "cursor plane active ⇒ primary is always cursor-free" is **false**.
Compositors (mutter/weston/Xorg-modesetting) **dynamically fall back to a software cursor**
per-cursor/per-frame when the cursor exceeds the advertised size, uses an unsupported format, or
is animated (Chromium/Firefox custom cursors, large text-select/wait cursors, a11y large
cursors). During those frames the compositor **bakes the cursor into the primary FB** while a
naive viewer would still be in client mode (OS cursor hidden, drawing a stale local overlay) →
**two cursors**. This toggle is routine in practice and is the real production double-cursor
window — not the never-uses-the-plane case D1 already covers.

Make plane-disable an **explicit, specified transition** on both sides (M4):

- **Guest.** When the cursor plane is disabled in an atomic commit (`plane_state->fb == NULL` /
  `!visible` — DRM *does* invoke `atomic_update` on the HW→SW handoff), emit `CURSOR_UPDATE` with
  `VISIBLE` clear (a MOVE_ONLY-class, shapeless message). Re-emit `VISIBLE=1` + a full DEFINE when
  the plane is re-enabled.
- **Viewer.** On `VISIBLE=0` from the guest, **hide the local overlay and do NOT restore the OS
  cursor** — for the duration, rely on the now-baked stream cursor (revert to server mode). On the
  next `VISIBLE=1`+DEFINE, resume client mode. This "guest-VISIBLE drives the overlay, never the
  OS cursor, during an active plane session" rule is the crux that prevents the double cursor. The
  OS cursor is re-shown only when the *session* ends (cap cleared, no plane, disconnect) or as the
  never-black fallback (D5/D8), never mid-session on a HW→SW frame.
- **Reduce fallback frequency.** Commit `mode_config.cursor_width = cursor_height = 256` (a single
  fixed number, not "64 or 256") so common large cursors stay on the plane, and size the device
  ARGB read bound to the *same* 256×256 (ties to the DoS bound below and D6).

### D5 — Predictive local cursor in the viewer, in aspect-fit `video_rect` space

`WindowEvent::CursorMoved` (`window.rs:154-156`) delivers `position` (`PhysicalPosition`) and
`win.inner_size()` — known **the instant winit delivers the event, before any round trip**. This
is the zero-lag source. **But the raw window-normalized value is NOT the guest coordinate unless
the video fills the whole window.**

**Aspect-fit is current reality, not a future risk (M6).** The correct UX letterboxes the video
(stretch visibly distorts aspect), so the mapping window-fraction→guest is **not** identity. Both
the viewer overlay position **and** the relay's absolute-input normalization (`absAxis`,
`GpuConsoleRelay.ts:343`) must route through a single shared **`video_rect`** (origin + size of
the displayed video within the window) or the local cursor and the injected guest pointer drift,
and clicks land wrong in the letterbox case *today, before any overlay ships*.

Single source of truth — one `video_rect`:

```
video_rect = aspect-fit(guest_scanout_w:h) inside window_inner_size   // origin (ox,oy) + size (vw,vh)
guest_norm  = clamp01( (pointer_px - video_origin) / video_size )
```

- **Viewer overlay.** Position the cursor quad in `video_rect` space, not full-window: quad
  top-left NDC derives from `pointer_px` clamped into `video_rect`, minus the *scaled* hotspot
  (S4). If the pointer is in a letterbox bar, hide/park the overlay (guest pointer is clamped to
  the edge).
- **Relay input.** `absAxis` must normalize by `video_rect`: `guest_axis = clamp((win_frac −
  video_origin_frac)/video_size_frac) * 32767`, and input in the bars is clamped or ignored. **This
  is a relay coordinate fix, orthogonal to the "no relay change for the sideband" invariant** — it
  corrects a *pre-existing* letterbox bug and must ship with, or before, the overlay so the two
  stay coupled. Pick one placement of the mapping and document it: either the viewer sends the same
  window-normalized `{"t":"m"}` and the relay owns `video_rect`→guest using the guest scanout dims
  it already knows, **or** the viewer sends already-`video_rect`-normalized coords. The load-bearing
  rule is **one** `video_rect` source shared by overlay draw and input normalization so they can
  never diverge (risk R8).
- **DPI-clean by construction** — `position`, `inner_size()`, and `extent` are all physical pixels,
  so `video_rect` math carries no scale factor.

**Cursor scale + hotspot (S4).** When guest resolution ≠ `video_rect` size (1080p guest in a 4K
window, HiDPI), draw the sprite scaled by `video_size / guest_scanout_size` and **scale the hotspot
by the same factor** — otherwise the click point misaligns even when the sprite looks right. Sprite
scale and hotspot scale are the same factor; do both or neither.

**The overlay pipeline is the central engineering cost, not the cursor logic.** The viewer has
**no graphics pipeline today** — presentation is 100% transfer-queue ops
(`cmd_copy_buffer_to_image` + `cmd_blit_image`, `window.rs:670,720`), deliberately shader-free.
`cmd_blit_image`/`cmd_copy_image` **overwrite** the destination; they do not alpha-blend, so a
transfer paste writes an opaque bounding box over the cursor's transparent border. Proper
compositing needs a graphics draw with `blend_enable:true` (premultiplied `ONE /
ONE_MINUS_SRC_ALPHA` given `PREMULTIPLIED`; plain `SRC_ALPHA / ONE_MINUS_SRC_ALPHA` otherwise).

Build it as a **plane table from day one** so media reuses it verbatim:

```rust
enum PlaneKind { Cursor, Video }
struct Plane { kind: PlaneKind, tex: OverlayTexture, rect_guest: Rect,
               hotspot: (u16,u16), z: u8, visible: bool, src: PlaneSource }
struct Compositor { planes: BTreeMap<u32, Plane> }  // plane 0 = cursor
```

Machinery (created once in `VkViewer::new`; recorded in `record`):

- **Per-swap-image color `VkImageView`** — none today (swap images are transfer targets only); the
  swapchain is *already* `TRANSFER_DST | COLOR_ATTACHMENT` (`window.rs:448`), so no swapchain
  change — create/destroy the views in `recreate_swapchain`.
- **Dynamic rendering** (VK 1.3 is already the requested API version, `window.rs:259`):
  `cmd_begin_rendering` with `LOAD_OP_LOAD` to preserve the blit. **Caveat:** `DeviceCreateInfo`
  enables **no** features today — chain `PhysicalDeviceVulkan13Features{ dynamic_rendering: true }`.
  (Legacy `VkRenderPass` + per-image framebuffer is the no-feature-toggle alternative; dynamic
  rendering is less code.)
- **One pipeline for all overlay planes:** vertex shader emits a quad from `gl_VertexIndex` (no
  vertex buffer), positioned by **push constants** (NDC rect in `video_rect` space); fragment shader
  samples the plane texture, blend enabled. Dynamic viewport/scissor from `self.extent` so resize
  never rebuilds the pipeline. One `VkDescriptorSetLayout` (binding 0 = COMBINED_IMAGE_SAMPLER,
  fragment), small pool, `VkSampler` (LINEAR for scaled cursors, NEAREST acceptable at 1:1).
  **Ship pre-compiled SPIR-V via `include_bytes!`** — no shader compiler added to `Cargo.toml`.

**Insertion point:** in `record`, **between the frame→swapchain blit (`window.rs:720`) and the
`→ PRESENT_SRC_KHR` barrier (`:747`)**. Recorded body: barrier `TRANSFER_DST_OPTIMAL →
COLOR_ATTACHMENT_OPTIMAL` (reuse `image_barrier`), begin rendering (LOAD), bind pipeline+set, then
**for each visible plane in z-order** push its `video_rect`-space NDC rect + `cmd_draw(4,1)`, end
rendering; the existing final barrier becomes `COLOR_ATTACHMENT_OPTIMAL → PRESENT_SRC_KHR`. Same
single `cmd`/`in_flight` fence — no extra sync.

**Shape/hotspot receive.** Route by magic in `stream.rs` **before** `FrameHeader::parse`
(`stream.rs:138`): `XIPI` → existing video fast-path (untouched); `XIPL` → plane handler → publish
to a second latest-wins slot (analogous to `FrameSlot`). Validate `payload_len ==
expected(width,height,pitch)` before upload (DoS section). Factor the existing frame uploader
(`ensure_frame_image` + `cmd_copy_buffer_to_image`) into a reusable helper; instantiate a small
(64×64, grown on demand to ≤256×256) `SAMPLED` `cursor_tex`; upload only on a new shape (mirror
`pending_upload`), barrier to `SHADER_READ_ONLY_OPTIMAL`. **Upload BGRA** to match `FMT =
B8G8R8A8_UNORM` (`window.rs:207`) or colors invert.

**OS-cursor hide + visibility state machine.** `Window::set_cursor_visible(false)` once a
client-mode cursor session is active. Fuse these inputs, in this precedence:

1. **Session end** (cap cleared / disconnect) → show OS cursor.
2. **Guest `VISIBLE=0`** (D4 HW→SW handoff, text caret, relative-lock) → hide overlay, **keep OS
   cursor hidden** (baked stream cursor covers it).
3. winit `CursorLeft` / `Focused(false)` (`window.rs:197` already drains input) → skip overlay
   draw for that frame; **do not seize the pointer on the first `CursorMoved` after refocus** if
   view-only (S7).
4. **No shape ever arrived** (cache empty / first-connect / restart-readmit / pipeline build
   failure) → **re-show the OS cursor or draw a default arrow** — the never-"no cursor at all"
   guarantee (D8, S2, M8).

Skip the overlay draw whenever hidden. This state machine is the difference between "clean single
cursor" and "double or missing cursor," so it is specified, not implied.

### D6 — Guest cursor plane (required; no cheap interim exists)

**There is no cheap interim that yields the latency win** — the cursor plane *is* the mechanism.
No guest-userspace helper (XFixes, a transparent cursor, infiniservice) can simultaneously read the
app's intended shape and stop the compositor baking it; that is a KMS-level decision keyed on
whether a cursor plane exists. This is the guest half of the 2D-ADR PR6, retained.

Replace the `drm_simple_display_pipe` construction (`infinigpu.c:96,355`) with an explicit atomic
pipeline:

1. **Primary plane** — `drm_universal_plane_init(… DRM_PLANE_TYPE_PRIMARY …)`; its `atomic_update`
   keeps today's `igpu_flush`/`igpu_submit_scanout` (later `DISPLAY_SCANOUT_DAMAGE`).
2. **Cursor plane** — a second `drm_universal_plane_init(… DRM_PLANE_TYPE_CURSOR …)`, formats
   `{ DRM_FORMAT_ARGB8888 }` (note: `igpu_formats` today is `XRGB8888` only — a cursor needs the
   alpha format).
3. **CRTC** — `drm_crtc_init_with_planes(&crtc, &primary, &cursor, …)`; move the vblank-event
   completion from `igpu_pipe_update` into the CRTC `atomic_flush`.
4. **Encoder** — `drm_simple_encoder_init`; reuse the existing connector funcs verbatim.
5. `mode_config.cursor_width = cursor_height = 256` (D4) so compositors know a HW cursor exists and
   rarely SW-fall-back.
6. **Cursor `atomic_update` emits `CURSOR_UPDATE`** by cloning `igpu_submit_scanout`: read
   `crtc_x/y` → `pos_x/y`, `hotspot_x/y`, the ARGB GEM's guest-phys via `drm_fb_dma_get_gem_addr` →
   `shape_ref`, `pitch` from the fb, and `fb==NULL || !visible` → `VISIBLE=0` (D4). Distinguish
   **MOVE_ONLY** (position changed, same fb pointer) from a **shape DEFINE** (fb changed), and set
   **`WARP`** when the position change did not originate from a pointer-motion input event (a
   guest-initiated warp — best-effort: flag jumps beyond a small threshold without an intervening
   relative input, M3). **It must NOT flush the primary** — a cursor-only commit touches only the
   cursor plane, so the primary now streams a **cursor-free** frame and pointer motion no longer
   dirties/re-flushes it.
7. **Coalesce MOVE emission to vblank (~60 Hz)** (S1): high-DPI / 1000 Hz mice and X11
   non-vblank cursor moves would otherwise push ~1000 `CURSOR_UPDATE` MOVEs/s onto the ring, each
   costing callback-thread work even when the device suppresses forwarding. Only **shape DEFINE**,
   **`VISIBLE` transitions**, and **`WARP`** are unconditional; pure MOVE is vblank-paced.
8. **X11 legacy path (S5).** Xorg-modesetting drives the cursor via `drmModeSetCursor2` /
   `drmModeMoveCursor` ioctls, and the hotspot arrives via `SetCursor2` — which may not route
   through the plane's `atomic_update` as written. **Validate that the legacy ioctl path emits
   `CURSOR_UPDATE` with a correct hotspot on the target kernel**; if it bypasses `atomic_update`,
   add a legacy cursor-funcs hook. This is part of the go/no-go gate below.

**The concrete export gap** (verified against 6.14 `Module.symvers`):
`drm_plane_create_hotspot_properties` is **NOT exported** to out-of-tree/DKMS modules
(`drm_universal_plane_init`, `drm_crtc_init_with_planes`, `drm_plane_enable_fb_damage_clips` all
*are*). The hotspot *infrastructure* (`drm_plane_state.hotspot_x/y`) is present in the 6.14 header,
but the *creator helper* is unlinkable. Resolve one of:

- **(a)** carry a tiny out-of-tree `EXPORT_SYMBOL_GPL(drm_plane_create_hotspot_properties)` patch
  (fragile across Ubuntu SRU kernels);
- **(b)** hand-roll `HOTSPOT_X/Y` range properties via the exported `drm_property_create_range` and
  read them in your own `atomic_check` — **caveat:** the core's auto-population of
  `plane_state->hotspot_x/y` keys on the *core-created* properties, so a hand-rolled property won't
  auto-plumb;
- **(c)** ship **without** DRM hotspot props and carry the hotspot only in the `CURSOR_UPDATE`
  body — fully sufficient for the client-composite viewer, **but** mutter/weston gate HW-cursor
  offload for virtual drivers on `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` + advertised props, so
  without them a compositor may fall back to SW cursor.

**Hotspot / adoption go/no-go gate (M7).** Option (c) can produce the *worst* outcome — the
compositor refuses the plane (cursor stays SW-baked, laggy) while any transient `CURSOR_UPDATE`
still flips announced-capable viewers to a client overlay → **double cursor with zero latency
benefit.** Therefore, before committing PR-C4/PR-C5, prove on the **target 6.14 kernel with
`modetest` + a real weston session AND a real mutter session AND X11-modesetting** that the
compositor **actually offloads to our cursor plane with a workable hotspot mechanism (option a or
b), on both Wayland (atomic) and X11 (legacy `drmModeSetCursor2`)**. If only option (c) is
achievable for a given compositor, **do not ship client mode for that compositor — stay server mode
(clear `caps::CURSOR_PLANE`)**. Do **not** half-enable.

Guard the explicit pipeline behind `#if LINUX_VERSION_CODE >= KERNEL_VERSION(6,6,0)` with the
`drm_simple_display_pipe` path retained for `#else` (DKMS builds per-kernel, so the guard is real).
Gate cursor-plane *exposure* on `caps::CURSOR_PLANE`; if clear, don't create the plane. Keep
`igpu_kms_selftest` and the `DISPLAY_SCANOUT` primary path green — the cursor plane is strictly
additive. **Ship this last** — largest guest change, easiest way to regress boot/console.

### D7 — Multi-viewer arbitration (input token + server-position MOVE)

All viewers funnel into the **single per-VM `inputQueues`** → one QMP abs pointer
(`GpuConsoleRelay.ts:234,251`). Two viewers moving the mouse fight over one guest pointer. A
client-side cursor *exposes* this (it does not cause it): each viewer's local overlay sits at its
own OS pointer, but the guest's true pointer is wherever whoever-moved-last put it. Resolution — an
**input token**, enabled precisely because position + `WARP` stayed end-to-end (D2):

- One viewer holds the token ("driving"): renders the cursor at its **local** pointer (zero lag),
  **ignores routine MOVE but honors `WARP` MOVE** (M3), and its input reaches `inputQueues`.
- Every other viewer is **view-only**: its input is gated off at `enqueueInput`
  (`GpuConsoleRelay.ts:251`), and it renders the cursor at the **authoritative server position**
  from every `PLANE MOVE` (sourced from `CURSOR_UPDATE.pos_x/pos_y`, forwarded because
  `clients > 1`, D2). So a non-driving viewer's cursor tracks the *actual* guest pointer, and it
  does not seize the pointer on refocus (S7).

Default `maxClientsPerSession = 4` (`GpuConsoleRelay.ts:101`) makes this reachable. The token is
**optional for v1** (last-writer-wins is acceptable for the common single-viewer console); the wire
is designed to support it without change. **The token is an explicit relay change** — see D10.

### D8 — Fallback when caps are absent (never black, never double)

Every layer degrades independently to today's working (laggy) path, gated on `caps::CURSOR_PLANE`
and the D9 hello:

- **Old device / host build without the consumer** → never advertises `CURSOR_PLANE` → guest keeps
  `drm_simple_display_pipe` → compositor SW-composites the cursor into the FB → device never
  receives `CURSOR_UPDATE`, never emits `XIPL` → viewer keeps the OS cursor. **Today's
  laggy-but-working path.**
- **Kernel < 6.6 / hotspot-prop gate failed for the compositor (M7)** → `#else` guard or cap
  cleared → simple-pipe → server mode.
- **Client that did not announce sideband support (D9)** → device does **not** emit `XIPL` to it →
  it stays in server mode with the baked cursor, even on a client-mode VM. This closes the
  stale-cached-browser desync (M9).
- **Compositor HW→SW cursor frame (D4)** → guest `VISIBLE=0` → viewer reverts to the baked stream
  cursor for the duration; no double cursor.
- **Relative-mode app before PR-C7 (D3)** → pin the VM to server mode; baked cursor + absolute input
  remains the working path.
- **Any DMA/geometry/format error in `handle_cursor_update`** → fail-closed (bounded ≤256×256,
  `pitch` validated, `format` must be B8G8R8A8, rate-limited, `ensure_admitted` gate); the sideband
  simply isn't emitted; the video stream is unaffected.
- **Viewer can't build the overlay pipeline** (feature/SPIR-V load failure) → skip the overlay draw,
  re-show the OS cursor; the transfer-only video path is untouched.
- **Cache empty / first-connect / restart-readmit before any DEFINE (S2, M8)** → viewer draws a
  default arrow (never "no cursor at all") until the first DEFINE arrives.

The OS cursor is hidden **only** while the viewer is in an active client-mode session AND the guest
`VISIBLE` bit is set (D5 state machine) — so there is never a "no cursor at all" window and never a
mid-session double.

### D9 — Client capability hello (mixed-version safety)

D1's "sideband presence = mode signal" elegance breaks against a **stale cached browser client**
(`client/infinipixel.html` is cache-served and reads fixed video offsets): if a device already in
client mode streams `XIPL` to a stale build lacking the magic guard, it misparses the sideband as a
`FrameHeader` and feeds WebCodecs → decode desync. The PR order (browser guard in C2 before device
emit in C3) protects *fresh* loads but not *cached* ones. So a small, explicit retreat from
zero-handshake is required (M9):

- **One client→device hello message** on WS connect announcing sideband support (a version /
  capability token — e.g. reuse a WS subprotocol string, or a first control message). The device
  **only emits `XIPL` to clients that announced support**; unannounced clients get the baked-cursor
  server-mode stream (device withholds `XIPL` from that client, which then shows the baked cursor).
- **Defense in depth on the browser**: reject any message whose first 4 bytes are not `XIPI` before
  offset-parsing it as a `FrameHeader`, so even an un-guarded stale build fails safe instead of
  desyncing.

This is the minimum targeted handshake that mixed client/device versions in the field force; it is
scoped to a single announce message, not a negotiation protocol.

### D10 — The "zero relay change" invariant is scoped to v1

State plainly, to kill the internal contradiction the critique found (M10):

- The **"relay needs zero changes"** invariant holds **only** for the sideband transport and
  **last-writer-wins v1 input**. The sideband rides `GpuConsoleRelay` verbatim (`:204-213`); the
  relay must stay a transparent binary pipe for frames and must **not** start parsing `FrameHeader`.
- **Three acknowledged relay changes exist and are NOT under that umbrella:**
  1. **The `video_rect` input-normalization fix (D5/M6)** — corrects a pre-existing letterbox
     coordinate bug in `absAxis` (`:343`); required now, ships with the overlay.
  2. **The multi-viewer input token (D7)** — gates `enqueueInput` (`:251`) by the token holder plus
     a token grant/revoke control message. Deferred.
  3. **The relative-motion `rel` injection (D3)** — parses `{"t":"mr",dx,dy}` → QEMU `rel` events.
     Deferred to PR-C7.

So "Backend/relay code changes" is a non-goal **for the sideband only**; the coordinate fix is
required now and the token/relative paths are acknowledged future relay work.

## Sideband transport: one generalized `XIPL` plane family

The sideband is where "plane" is real. Define it **once**, in `infinigpu-pixel`, as a typed
op-family so the cursor and (future) media planes share one wire, one Hub cache, and one viewer
demux. It multiplexes onto the **existing** infiniPixel WS — no new socket, port, or backend code.

**Multiplex by a distinct magic, not a repurposed `FrameHeader` byte.** A plane message whose first
4 bytes are a *different* magic makes an un-updated native viewer's `FrameHeader::parse` return
`None` (`stream.rs:138`) → logged and `continue`d → **safely dropped, never reaching openh264**.
Repurposing `FrameHeader.kind` is rejected: an old viewer would pass the header and feed sideband
bytes to openh264 → decode desync.

`PlaneHeader` (36 bytes, little-endian, new `proto::plane` module in `infinigpu-pixel`):

```
 off size field
  0   4   magic  "XIPL"  (u32 LE; distinct from video "XIPI" = proto::MAGIC)
  4   1   version (1)
  5   1   op          DEFINE=1 | MOVE=2 | DATA=3 | DESTROY=4
  6   1   plane_kind  CURSOR=1 | VIDEO=2
  7   1   flags       bit0 VISIBLE ; bit1 PREMULTIPLIED ; bit2 WARP ; bit3 RELATIVE
  8   4   plane_id (u32 LE) — 0 reserved = cursor
 12   1   codec/format (cursor: pixel format ; video: codec id ; else 0)
 13   1   z_order
 14   2   (pad)
 16   2   width  (u16) — shape/region dims on DEFINE, else 0
 18   2   height (u16)
 20   2   hot_x  (u16) — cursor hotspot
 22   2   hot_y  (u16)
 24   4   pos_x  (i32) — authoritative server position (guest space)
 28   4   pos_y  (i32)
 32   4   payload_len (u32) — ARGB pixels (cursor DEFINE) / bitstream chunk (video DATA)
       = 36-byte header; body follows
```

- **CURSOR DEFINE** = header (shape dims, hotspot, VISIBLE) + BGRA body. **CURSOR MOVE** = header
  only (server position + `WARP` bit, for view-only viewers and warp correction). **CURSOR MOVE
  with VISIBLE clear** = the D4 HW→SW / hide transition (header only). **DESTROY** = header only.
  (VIDEO ops are the media rung — see [Media Redirection](#media-redirection-future-rung).)
- **`flags` mirrors the ABI `cursor_flags` low bits** so the device forwards them verbatim without
  re-encoding semantics — `VISIBLE`/`PREMULTIPLIED`/`WARP`/`RELATIVE` cross the sideband unchanged.
- **Device forwards without touching the encoder.** New `PixelStreamer::send_plane(hdr, body)` calls
  `self.hub.broadcast_control(msg)` (a control-lane variant) — it **never** calls
  `ensure_encoder`/`take_keyframe_request`/spawns ffmpeg, so it is safe on the single vfio-user
  callback thread (`hub.broadcast → ClientQueue::push` never blocks). This is the design's headline
  safety win: it directly retires the 2D-ADR's stated **biggest risk #1** ("blocking the vfio-user
  callback thread"), which PR6's host-composite re-encode would *aggravate*.
- **Hub joiner priming + shed discipline** (mirrors the keyframe machinery): add to `HubState` a
  `planes: HashMap<u32, PlaneCache>` where `PlaneCache { last_define, last_move }`; prime it into
  each joining client in `Hub::register` exactly where `last_keyframe` is primed — so a
  reconnecting/late viewer gets the current cursor shape+position immediately (video self-heals via
  `last_keyframe`+IDR; a static cursor otherwise shows **no bitmap until the next shape change,
  which may never come**). Add a `control: bool` param to `ClientQueue::push`:
  `DEFINE`/`MOVE`/`DESTROY` are **control** — they bypass the `dropping` shed gate and are re-primed
  from the cache when backpressure clears. `VIDEO DATA` is video — bounded/drop-to-keyframe.
- **Relay: transparent for the sideband.** The `XIPL` stream rides through `GpuConsoleRelay`
  verbatim; the relay must not parse `FrameHeader`. (Its input-coordinate fix D5/M6 and the deferred
  token D7 / relative D3 are separate, acknowledged changes — D10.)

## Device-side shape ingestion is a hardened, rate-limited path (M5)

The device DMA-reads a guest-phys ARGB sprite and ARGB→BGRA repacks **on the single vfio-user
callback thread** — the exact thread whose stalls cause the documented guest-scanout/BQL freeze. A
size cap alone is insufficient; `handle_cursor_update` must:

- **Rate-limit shape DMA/repack** — ≤ N DEFINE reads/sec per VM (coalesce excess, keep only the
  latest). Defeats the **de-dup-alternation** attack (a guest alternating two shapes every DEFINE so
  de-dup never fires → 256 KiB DMA + repack at doorbell rate).
- **Validate `pitch`** — require `pitch ≥ width*4` and use `pitch*height` (not `width*height`) as
  the DMA read length, with `pitch*height ≤ cap` (cap = 256×256×4, matching D4/D6). A large pitch
  with small width must not over-read or blow the bound. Fail-closed on violation, in the style of
  `present_scanout`.
- **Accept only `format::B8G8R8A8`** — reject any other format.
- **Assert `payload_len == expected(width,height,pitch)`** at device emit **and** re-validate in the
  viewer `stream.rs` / browser before texture upload — or the viewer over-reads the upload.
- **Suppress pure MOVE_ONLY** in the single-viewer non-`WARP` case, **coalesce** and forward MOVE
  otherwise (D2/M1); de-dup unchanged shapes vs `CursorState`.
- **Fail-closed everywhere:** any DMA/geometry/format/rate violation → the sideband simply isn't
  emitted; seqno still retires; video stream unaffected; no OOB.

**Restart survival (M8).** The GPU VM survives a backend/device restart (device re-adopts the VM),
but the per-VM `CursorState` and the Hub `last_cursor` cache are lost, and the guest only emits
`CURSOR_UPDATE` on a compositor cursor *change* — a static screen yields no re-DEFINE, so a
re-adopted device would have no shape and an announced-capable viewer would show an empty overlay
(violating D8's "never no cursor"). On (re)admission the device must **solicit a fresh cursor**: set
a "need cursor" state and either (a) a device→guest doorbell/cap-event that makes the guest re-emit
the current cursor on the next commit, or (b) have the guest re-DEFINE the current cursor whenever
it observes the device (re)initialize. Until a DEFINE arrives, the viewer falls back to the OS
cursor / default arrow (S2).

## Caps: exactly one new bit, composite-neutral

The guest's behavior change (build a DRM cursor plane; stop baking the cursor; emit `CURSOR_UPDATE`)
is **byte-identical** whether the host composites or the client does. So the ABI needs exactly
**one** `DEV_CAPS` bit meaning *"cursor is off the primary plane"* — **not** both `HW_CURSOR` and
`CLIENT_CURSOR`. Insert after `DISPLAY_ACCEL` (`regs.rs:111`):

```rust
/// Cursor is off the primary plane: the guest builds a DRM cursor plane and emits
/// CURSOR_UPDATE instead of blitting the cursor into the framebuffer. Whether the device
/// composites host-side or forwards to a client overlay is chosen on the device build + the
/// device↔viewer sideband, NOT here. Clear = today's software-cursor-in-framebuffer path.
pub const CURSOR_PLANE: u32 = 1 << 6;   // regs::caps

pub const PHASE2_DEV_CAPS: u32 = PHASE1_DEV_CAPS | caps::CURSOR_PLANE;
```

This **renames the 2D-ADR PR6 `caps::HW_CURSOR` bit** to the composite-neutral `CURSOR_PLANE`.
`CURSOR_UPDATE` rides under the existing `capset::CAP_DISPLAY_2D` umbrella — **no new capset**.
`caps::DISPLAY_ACCEL` (damage) and `caps::CURSOR_PLANE` are orthogonal `DEV_CAPS` bits, gated
independently, each fail-safe to today's path when clear.

**ABI-version impact (additive, ADR-0004 compliant):**

- `ids.rs` — bump `ABI_MINOR` `2 → 3`, update the doc comment (keep `ABI_MAJOR = 0`).
- `lib.rs` — `abi_version_packs_major_minor` test `0x0000_0002 → 0x0000_0003`.
- `lib.rs` (after the `ScanoutPresentDamaged` asserts) — layout asserts:
  `size_of::<CursorUpdate>()==48`, `offset_of!(_, pos_x)==8`, `offset_of!(_, hot_x)==16`,
  `offset_of!(_, pitch)==24`, `offset_of!(_, format)==28`, `offset_of!(_, shape_ref)==32`,
  `offset_of!(_, _reserved)==40`.
- `cbindgen.toml` include — add `"CursorUpdate"`. **Pre-existing footgun:** cbindgen emits *structs
  only*, so `msg_type::CURSOR_UPDATE`, `cursor_flags::*`, and `caps::CURSOR_PLANE` are **not** in the
  generated C header; the guest C driver hardcodes those integers exactly as it already does for
  `DISPLAY_SCANOUT`/`DISPLAY_ACCEL`.
- `guest/include/abi_conformance.c` — add `_Static_assert(sizeof(struct CursorUpdate)==48,…)` +
  offset asserts; regenerate `guest/include/infinigpu_abi.h` via `scripts/gen-abi-header.sh`
  (compiled `-Werror` = the cross-language gate).
- **`msg_type::MEDIA_REGION = 0x0043` reserved now (S8), body unfrozen** — reserving an opcode is
  additive and costless and prevents a future ABI-minor collision, consistent with the ADR's
  "reserve early, freeze late" discipline.
- **Device decode prerequisite (already ADR-mandated):** the `0x0042` arm **must** use the
  `min(payload_len, size_of)` zero-filled read (2D-ADR decision #1) so a future growth into
  `_reserved` never breaks an older decoder.

## PR sequence

Everything **C1–C4 ships dark** (cap gated off) with zero user-visible change and zero regression at
every step; each is independently reviewable, mergeable, and testable against a synthetic emitter.
**C5 is the single switch that lights the feature up** end-to-end. This concentrates all risk in one
final, cap-gated, fail-safe rung.

Dependency shape: C1 is foundational; C2 and C3 depend on C1; C4 depends on C1+C2 and is fully
validatable with a synthetic cursor (no C3/C5); C5 depends on C1+C3+C4 and is gated on the D6/M7
hotspot go/no-go. C6 and C7 are deferred and independent.

### PR-C1 — ABI freeze + sideband constants *(S, ~0.5 wk; no behavior)*

- `wire.rs`: `struct CursorUpdate` (48 B) + `mod cursor_flags` (**all bits incl. `WARP`,
  `RELATIVE`**). `regs.rs`: `caps::CURSOR_PLANE = 1<<6` + `PHASE2_DEV_CAPS`. `ids.rs`:
  `ABI_MINOR 2→3`. Reserve `msg_type::MEDIA_REGION = 0x0043` (body unfrozen). `lib.rs`: layout
  asserts + `abi_version()==0x0000_0003`. `cbindgen.toml` + `abi_conformance.c` + `gen-abi-header.sh`.
- `infinigpu-pixel`: `proto::plane` module (magic `XIPL`, `PlaneHeader`, `op`/`plane_kind` consts,
  `flags` mirroring `cursor_flags` low bits) + a round-trip test mirroring
  `header_round_trips_little_endian`.
- Reserve the relative-motion upstream input shape `{"t":"mr",dx,dy}` in the relay message-type doc
  (parse stub, not yet injected).
- **Accept:** `cargo build/test -p infinigpu-abi -p infinigpu-pixel` (layout asserts compile only if
  exact; `abi_version()==0x0000_0003`); `gen-abi-header.sh` regenerates the header and
  `abi_conformance.c` compiles `-Werror` clean — the C view byte-matches Rust; `PlaneHeader`
  round-trips; `WARP`/`RELATIVE`/`MEDIA_REGION` constants present.

### PR-C2 — Pixel sideband lane + Hub priming + client demux guards + capability hello *(M)*

- `infinigpu-pixel`: `PixelStreamer::send_plane` (never touches the encoder), `Hub` control-lane
  (`broadcast_control`, `planes` cache, `push(control:true)`, prime in `register`).
- Native viewer `stream.rs` + browser `client/infinipixel.html`: peek the first 4 bytes **before**
  `FrameHeader::parse`; `XIPL` → route to a plane handler (v1: safe-drop is acceptable, real overlay
  lands in C4); `XIPI` → existing path; **browser additionally rejects any non-`XIPI` before
  offset-parsing** (D9 defense in depth).
- **Client capability hello (D9):** viewer/browser announce sideband support on connect; device
  records it per client and will only emit `XIPL` to announced clients (consumed in C3).
- **Accept:** wire round-trip unit test; a synthetic `XIPL` primed on a mock WS client reaches a late
  joiner; an un-updated viewer drops `XIPL` safely (`FrameHeader::parse`→`None`, no openh264 desync);
  a stale browser without the guard is rejected by the non-`XIPI` check; every existing video frame
  is byte-identical; a client that did **not** announce receives no `XIPL`.

### PR-C3 — Device decode `CURSOR_UPDATE` + forward (hardened) *(M)*

- `process_ring`: a `CURSOR_UPDATE` branch **before** the SUBMIT_CMD guard → `handle_cursor_update`
  (a helper so PR4's two-phase drainer can reuse it), then retire the seqno like `present_scanout`.
- `handle_cursor_update` (M5): `ensure_admitted` gate; `min(len,size_of)` zero-filled read;
  **rate-limit** shape DMA/repack per VM; **validate `pitch ≥ width*4`, bound `pitch*height ≤
  256×256×4`, accept only `B8G8R8A8`**, fail-closed; DMA-read the ARGB sprite from `shape_ref`;
  de-dup vs `CursorState`; **suppress pure MOVE_ONLY only single-viewer/non-WARP, else coalesce +
  forward** (M1); always forward `WARP` and `VISIBLE`-transition (D4); ARGB→BGRA repack; assert
  `payload_len` on emit; `pixel.send_plane(...)` **only to announced-capable clients** (D9).
- Per-VM `CursorState` field next to `pixel`; clear in `reset_state`; **solicit-on-readmission**
  need-cursor state (M8). Advertise `PHASE2_DEV_CAPS` behind an `INFINIGPU_CURSOR_PLANE` build/env
  gate during rollout.
- **Accept:** loopback — push a synthetic `CURSOR_UPDATE` descriptor → assert exactly one `XIPL
  DEFINE` with correct BGRA/hotspot/pos; MOVE_ONLY single-viewer → suppressed; MOVE with `clients>1`
  → forwarded (coalesced); `WARP` → always forwarded; unchanged shape → de-duped; alternating-shape
  flood → rate-limited; hostile `pitch`/oversized/wrong-format → rejected, seqno still retires, no
  OOB; a `PHASE1_DEV_CAPS` device emits nothing (fallback proof); an unannounced client gets no
  `XIPL`; `send_plane` never calls `ensure_encoder` (callback-thread safety); restart → need-cursor
  solicits a fresh DEFINE.

### PR-C4 — Viewer overlay compositor + predictive local cursor *(L; the biggest single lift)*

- First graphics pipeline: VK 1.3 `dynamic_rendering` feature, pre-compiled SPIR-V,
  descriptor/sampler, per-swap-image color views, the generic `Compositor`/`Plane` table.
- **`video_rect` single source of truth (D5/M6):** compute aspect-fit video rect; route **both**
  overlay positioning and the relay input normalization through it (the relay `absAxis` coordinate
  fix ships here, D10); predictive local pointer from `CursorMoved` mapped via `video_rect`; scale
  sprite **and hotspot** by `video_size/guest_size` (S4).
- `XIPL` demux → cursor slot; BGRA shape upload + hotspot; `payload_len` re-validation before upload
  (M5); overlay draw at the insertion point (D5); **OS-cursor hide + `VISIBLE` state machine incl.
  the D4 HW→SW-handoff rule** (`CursorEntered`/`Left` + `Focused` + guest `VISIBLE`); WARP snap (M3);
  dynamic viewport/scissor on resize; default-arrow fallback (S2/M8). Driving-vs-view-only MOVE
  handling + no-seize-on-refocus (D7/S7). Browser client 2D-canvas cursor overlay for parity.
- **Validate against a device-synthesized `CURSOR_UPDATE` (de-risks the whole client path before the
  guest plane exists).**
- **Accept:** cursor overlay tracks the local pointer with **zero added latency** (visually decoupled
  from stream latency); correct in the **letterboxed/aspect-fit** case (click point aligns via
  `video_rect`, no drift vs injected pointer); shape/hotspot correct and correctly scaled at
  guest≠window resolution; OS cursor hidden in client mode, **no double cursor when the guest sends
  `VISIBLE=0`** (baked cursor shows instead); WARP snaps the local pointer; resize/DPI clean with no
  pipeline rebuild; a viewer built without overlay still decodes video; `XIPL` never reaches
  openh264/WebCodecs.

### PR-C5 — Guest cursor plane *(L; highest regression risk — SHIP LAST; = the switch)*

- **Prerequisite gate (M7):** pass the D6 hotspot / compositor-adoption go/no-go on the target 6.14
  kernel with `modetest` + weston + mutter + X11-modesetting (both atomic and legacy
  `drmModeSetCursor2` paths, S5). If a compositor only reaches option (c), keep it in server mode.
- Replace `drm_simple_display_pipe` with explicit primary + `DRM_PLANE_TYPE_CURSOR` + CRTC +
  encoder (D6); `mode_config.cursor_width = cursor_height = 256`; cursor `atomic_update` emits
  `CURSOR_UPDATE` (MOVE_ONLY vblank-coalesced S1; `WARP` on guest-initiated jumps; `VISIBLE=0` on
  plane-disable/HW→SW handoff D4; guest-phys `shape_ref`, no PR4 dependency); primary becomes
  cursor-free; `#if LINUX_VERSION_CODE` guard + simple-pipe `#else`; gate exposure on
  `caps::CURSOR_PLANE`; keep KMS selftest + `DISPLAY_SCANOUT` green.
- **Accept:** `modetest` shows a CURSOR plane (with hotspot props under the chosen a/b workaround);
  moving the mouse in weston/mutter/Xorg emits `CURSOR_UPDATE` (MOVE_ONLY, vblank-paced) and does
  **not** flush the primary; a compositor HW→SW cursor fallback emits `VISIBLE=0` and the viewer
  shows exactly one (baked) cursor — **no double**; a guest warp sets `WARP` and the driving viewer
  snaps; **end-to-end cursor lag drops from ~0.5–1 s to local-frame latency**; a no-`CURSOR_PLANE`
  host → SW cursor, still renders; boot/console/selftest unregressed.

### PR-C6 — Host-composite fallback *(deferred, optional; = the ADR PR6 device half)*

- Device blends the retained `CursorState` into the frame + re-encodes for a future non-overlay /
  thin viewer (reuse PR5's per-VM worker + build; per-VM mode). Out of the minimal critical path;
  only for clients that cannot client-composite — which our fleet has none of.
- **Accept:** a synthetic non-overlay client sees a correctly composited cursor with no trail; the
  composite runs on the per-VM worker, never the callback thread.

### PR-C7 — Relative-pointer / pointer-lock path *(deferred; unblocks games/CAD)*

- Implement the D3 runtime: viewer honors `RELATIVE`, `set_cursor_grab(Locked/Confined)`, sends
  `{"t":"mr",dx,dy}`; relay injects QEMU `rel` events (an explicit relay change, D10). Until then,
  relative-heavy VMs stay in server mode (cap cleared).
- **Accept:** an FPS/orbit-camera app is playable under a cursor-plane guest; the overlay is hidden
  while locked; leaving the lock (`RELATIVE` clear) restores absolute mode and the overlay.

## Reconciliation with the 2D-ACCEL ADR

`docs/adr/2D-ACCEL-IMPLEMENTATION.md` **PR1–PR5 and PR7–PR8 are untouched and orthogonal** — the
cursor sideband never reads or writes the framebuffer/scanout surface; it is a separate ring message
and a separate Hub lane. Two edits to that ADR:

1. **PR6 is rewritten and split.** Its **guest half** (abandon simple-pipe; add cursor plane +
   hotspot + `CURSOR_UPDATE` emission; cursor-free primary) is **kept** as **PR-C5** (extended with
   the D4 `VISIBLE=0` handoff, S1 vblank coalescing, and the M7 adoption gate). Its **device half**
   (host-GPU composite + re-encode) is **removed from the critical path** and demoted to the
   deferred **PR-C6**. Its cap bit `caps::HW_CURSOR` is **renamed `caps::CURSOR_PLANE`**
   (composite-neutral, one bit). PR7's re-encode becomes *slightly simpler* — it no longer composites
   a cursor.
2. **The `CursorUpdate` body freeze moves earlier.** The ADR's decision #4 ("defer the body to PR6")
   and its open question ("guest-phys addr vs res_id") are **resolved now** in **PR-C1**: freeze the
   48-byte body with a **guest-phys `shape_ref` + `SHAPE_BY_RESID` flag** (works today, no PR4
   `ResourceTable` dependency; res_id becomes an additive option once PR4 lands), **plus the `WARP`
   and `RELATIVE` flag bits** (free now, expensive to add after the body ships). The freeze is
   additive and independent of the damage path, so it need not wait.

Rationale for superseding host-composite: PR6's host composite fixes **bandwidth but not latency**
and bills a blend + re-encode on the present path — aggravating the 2D-ADR's own biggest risk #1
(blocking the vfio-user callback thread). Client composite fixes **latency and bandwidth at ~0
device cost** and adds **zero** encode work to the callback thread.

## Media Redirection (future rung)

The overlay pipeline (D5) and the `XIPL` sideband are deliberately the general "buffer sector" plane
framework media reuses. The goal: forward a guest video sub-region's **original compressed
bitstream** to a viewer **second decoder** rendering into an overlay plane positioned at the guest
rect — avoiding the double transcode (guest-decode → host-nvenc-reencode → client-decode).

**What reuses cleanly (cheap):**

- **Sideband:** new `plane_kind = VIDEO`. `DEFINE` carries the guest rect (`pos_*`/`width`/`height`)
  + `codec`; `DATA` carries bitstream chunks (bounded/shed queue discipline like video AUs);
  `DESTROY` tears it down. No new wire family — the same `PlaneHeader`. `msg_type::MEDIA_REGION =
  0x0043` is **reserved** in PR-C1 (body unfrozen), same "reserve early, freeze late" discipline as
  `CURSOR_UPDATE`.
- **Viewer:** the same textured-quad + blend pipeline (in `video_rect` space); swap the plane's
  `PlaneSource` from an ARGB upload to a `VideoDecoder` (second openh264/other) into a bigger
  texture, positioned by the guest rect, z-ordered over the primary.
- **Device:** forward the guest bitstream without re-encoding (a genuine bandwidth win); the guest
  punches a color-keyed/transparent hole in the primary so the primary encode skips those pixels.

**What does NOT reuse — honest: this is research-grade (months, partial coverage):**

- **There is no universal pre-decode hook on Linux.** The guest driver is display-only
  (`DRIVER_MODESET|ATOMIC|GEM`, **no `DRIVER_RENDER`**, no VA-API/VDPAU/V4L2 backend), so capture is
  net-new. The least-bad point is a **VA-API backend shim** (`LIBVA_DRIVER_NAME=infinigpu`) that
  intercepts `vaRenderPicture` slice/param buffers and remuxes to an elementary stream — HIGH effort
  (a libva backend + a slice→Annex-B remuxer synthesizing SPS/PPS) and it only catches apps that use
  VA-API. **Chrome and Firefox each own their decode selection and frequently software-decode,
  bypassing any GPU-side interception entirely.**
- **The real killer is the "hole" + occlusion tracking, not the capture.** You must leave a
  transparent rect in the primary where the video is, send the viewer the *screen-space* rect (routed
  through the same `video_rect`/aspect-fit mapping the cursor now uses), and keep it correct as an
  arbitrary compositor moves/scales/clips/occludes/scrolls the window **with no compositor
  cooperation.** There is no clean guest-side hook; the client-side plane framework gives you the
  viewer-side overlay for free, but guest-side rect-tracking + keep-transparent + resync-on-scroll is
  the expensive, unsolved part.
- **Audio + A/V sync are additional net-new subsystems (S3).** Redirected video usually carries
  **audio**, and no audio path exists anywhere in the stack. The overlay decoder's output must also
  be **A/V-synced and time-aligned** with the primary desktop stream, or the video tears against
  window chrome/scroll. Both are unsolved and out of scope for the plane framework.

**Recommendation:** build the cursor plane now (standardized DRM, ~weeks). **Defer media redirection
to a later opt-in experiment scoped to a bundled player** (e.g. a bundled mpv/ffmpeg on the VA-API
shim), **explicitly not a general solution.** The sideband, the reserved ABI opcode (`0x0043`), and
the viewer plane compositor are kept generic so a future `MEDIA_REGION` drops into the existing
`plane_kind = VIDEO` path without re-architecting.

## Risks

1. **Lockstep client demux (highest).** `XIPL` must be routed **before** `FrameHeader::parse` in
   *both* `stream.rs:138` and `client/infinipixel.html`, or sideband bytes desync openh264 /
   WebCodecs. Pin with the PR-C1 round-trip wire test; the browser guard + non-`XIPI` reject + the D9
   capability hello land in PR-C2 (protects fresh *and* stale cached browsers).
2. **`drm_plane_create_hotspot_properties` is NOT DKMS-exported on Ubuntu 6.14** (verified) and can
   yield the *worst* case (compositor refuses the plane → SW cursor → still-laggy → but any transient
   `CURSOR_UPDATE` flips capable viewers to a client overlay → double cursor, zero benefit). Resolve
   (a/b) and make compositor adoption a **hard go/no-go gate (M7)** on Wayland (atomic) **and** X11
   (legacy `drmModeSetCursor2`, S5) before PR-C4/C5. Option (c) alone ⇒ stay server mode.
3. **Compositor HW↔SW cursor toggle (in-production double cursor, M4).** Handled by the explicit
   `VISIBLE=0`→server-mode state machine (D4): guest emits `VISIBLE=0` on plane-disable; viewer hides
   the overlay but keeps the OS cursor hidden and rides the baked stream cursor. Reduce frequency with
   `cursor_width/height = 256`.
4. **Guest cursor-plane rework (PR-C5) is the largest change and the easiest boot/console
   regression.** Ship last; mirror `virtgpu_plane.c`/vkms; keep `igpu_kms_selftest` +
   `DISPLAY_SCANOUT` green; fail-safe to simple-pipe.
5. **The viewer's first graphics pipeline (PR-C4)** — shaders (pre-compiled SPIR-V), descriptors,
   per-image views, the VK1.3 `dynamicRendering` feature toggle. Self-contained but the biggest single
   engineering lift. Resist "just blit the cursor" — `cmd_blit_image` writes an opaque bounding box.
6. **Shared single guest pointer across viewers (M1/M3)** — exposed (not caused) by the client
   cursor; resolved by the input token + conditional/coalesced server-position MOVE for view-only
   viewers and always-forwarded `WARP` (D2/D7), which is why position + `WARP` stay end-to-end. The
   token is an **acknowledged relay change** (D10).
7. **Cursor warp / relative-mode apps (M2/M3).** WARP MOVE forwarding + driving-viewer snap keeps
   clicks aligned; relative-mode apps regress under a cursor-plane guest until PR-C7 — mitigate by
   pinning relative-heavy VMs to server mode (cap cleared). Hooks (`WARP`, `RELATIVE`, `{"t":"mr"}`)
   are frozen in PR-C1 so no re-freeze is needed.
8. **Aspect-fit coordinate drift (M6).** The local overlay and the injected guest pointer share
   **one** `video_rect` (D5); the relay `absAxis` normalization is corrected to the same rect (a
   pre-existing letterbox bug fixed now, not deferred). Any future change to letterboxing must update
   both through that single source or they drift.
9. **Callback-thread safety / shape DoS (M5).** `send_plane`/`broadcast_control` never block and
   never call `ensure_encoder`; the shape DMA is rate-limited, `pitch`-validated, format-restricted,
   and `payload_len`-checked, bounded ≤256×256 fail-closed.
10. **Restart loses cursor state (M8)** vs the survives-restart feature — device solicits a fresh
    DEFINE on readmission; viewer falls back to a default arrow until it arrives (S2).
11. **Stale cached browser (M9)** — the D9 capability hello + non-`XIPI` reject guard prevent a stale
    build from misparsing `XIPL` as a `FrameHeader`.
12. **Premultiplied-alpha / BGRA order** — the viewer blend must assume premultiplied ARGB and upload
    BGRA, or the cursor inverts / halos. Pinned by the `PREMULTIPLIED` flag.
13. **`set_cursor_visible(false)` is client-area-only (S6)** — OS cursor can reappear at
    edges/decorations/some X11 setups; fuse with `CursorLeft`/`Focused(false)` and accept edge
    reappearance as a minor known issue, or grab-confine when driving.

## Non-goals

- **General media redirection** (see the section above) — deferred to a scoped, opt-in experiment;
  audio + A/V sync explicitly out of scope; not part of the cursor deliverable.
- **A per-viewer runtime mouse-mode toggle** — mode is a per-VM, cap-gated, self-configuring property
  (D1) modulated only by the guest `VISIBLE` bit and the D9 capability hello; there is no
  `{"t":"cur","mode":…}` control wire.
- **Relative-mouse / pointer-lock at v1** — hooks frozen in PR-C1 (D3), runtime deferred to PR-C7;
  until then relative-mode apps use server mode.
- **Backend/relay code changes for the sideband** — the relay stays a transparent binary pipe for
  frames. The `video_rect` input-coordinate fix (required now), the deferred input token, and the
  relative-`rel` injection are **acknowledged** relay changes, not smuggled under this non-goal (D10).
- **Multi-head** — single CRTC today (`MAX_SCANOUTS=1`); `scanout_id` is carried for future headroom
  only.
- **Faster guest 2D/3D drawing** — orthogonal; owned by the 2D/3D accel roadmap. This ADR changes
  *where the cursor is composited*, not how the guest draws.

## Open questions

- **Input token vs last-writer-wins** for multi-viewer (D7) — ship last-writer-wins in v1, land the
  token in a later rung as an explicit relay change (D10). The wire supports both.
- **WARP heuristic in the guest (M3)** — cleanly distinguishing an app-initiated
  `XWarpPointer`/recenter from ordinary motion at the DRM `atomic_update` layer is best-effort (no
  input provenance at KMS level); validate the jump-threshold heuristic against real games/menus, and
  consider a small infiniservice-side hint if the KMS heuristic proves unreliable.
- **`video_rect` mapping placement** — viewer-side normalization vs relay-side `video_rect` math
  (D5); pick one and make it the single source shared by overlay draw and input.
- **VA-API media-capture shim scope** — bundled-player-only vs general; deferred with the media
  experiment.
