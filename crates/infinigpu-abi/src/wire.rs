//! Fixed-layout wire framing shared across the VM boundary.
//!
//! Every struct is `#[repr(C)]` with **no internal padding** (fields ordered so
//! `u64`s land on 8-byte offsets) and derives zerocopy's `FromBytes`/`IntoBytes`/
//! `Immutable`/`KnownLayout`, so a hostile guest's bytes can be reinterpreted
//! without UB and our writes never leak padding. Layout is asserted at compile
//! time in [`crate::layout_asserts`].

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Ring descriptor size in bytes (power of two for cheap index math).
pub const DESCRIPTOR_SIZE: usize = 32;

/// Message-class tags carried in [`MsgHeader::msg_type`] and [`Descriptor::msg_type`].
/// Our own enum values (cribbed from virtio-gpu's command taxonomy, ADR-0004).
pub mod msg_type {
    // Negotiation / capability (control ring)
    pub const NEGOTIATE: u32 = 0x0001;
    pub const GET_CAPSETS: u32 = 0x0002;
    pub const GET_DEVICE_INFO: u32 = 0x0003;
    // Context lifecycle (control ring)
    pub const CTX_CREATE: u32 = 0x0010;
    pub const CTX_ATTACH_RING: u32 = 0x0011;
    pub const CTX_DESTROY: u32 = 0x0012;
    // Resource lifecycle (control ring)
    pub const RESOURCE_CREATE_BLOB: u32 = 0x0020;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0021;
    pub const RESOURCE_MAP_BLOB: u32 = 0x0022;
    pub const RESOURCE_UNMAP_BLOB: u32 = 0x0023;
    pub const RESOURCE_DESTROY: u32 = 0x0024;
    // Command submission (command ring)
    pub const SUBMIT_CMD: u32 = 0x0030;
    pub const FENCE_WAIT: u32 = 0x0031;
    // Presentation (control ring)
    pub const SET_SCANOUT_BLOB: u32 = 0x0040;
    pub const RESOURCE_FLUSH: u32 = 0x0041;
    pub const CURSOR_UPDATE: u32 = 0x0042;
    /// Reserved (body unfrozen) for the future media-redirection plane
    /// (`docs/adr/CLIENT-PLANE-COMPOSITOR.md` — forward a guest video sub-region's original
    /// bitstream to a client decode-into-overlay). Reserving the opcode now is additive and
    /// costless; it prevents a later ABI-minor collision. "Reserve early, freeze late."
    pub const MEDIA_REGION: u32 = 0x0043;
    // Control / async (host -> guest)
    pub const RESET: u32 = 0x0050;
    pub const EVENT: u32 = 0x0051;
}

/// Capset bitmap (which command sub-protocol a context speaks).
pub mod capset {
    pub const CAP_VULKAN: u32 = 1 << 0;
    pub const CAP_D3D12: u32 = 1 << 1;
    pub const CAP_DISPLAY_ONLY: u32 = 1 << 2;
    /// Accelerated 2D present: damage-rect scan-out + (later rungs) host-GPU
    /// convert/composite and a hardware cursor. Paired with the `DISPLAY_ACCEL` DEV_CAPS bit.
    pub const CAP_DISPLAY_2D: u32 = 1 << 3;
}

/// `CtxCreate::api_type`.
pub mod api_type {
    pub const DISPLAY: u32 = 0;
    pub const VULKAN: u32 = 1;
    pub const D3D12: u32 = 2;
}

/// `SubmitCmd::encoding` — tags the opaque payload's sub-protocol. Framing never
/// parses the payload; only the matching host decoder does.
pub mod encoding {
    pub const VULKAN_VENUSLIKE: u32 = 1;
    pub const D3D12_DDI: u32 = 2;
    pub const DXGI_PRESENT: u32 = 3;
    /// Phase-0 bring-up payload: a [`super::ClearPresent`] — clear an image to a
    /// colour and write it to a guest scanout address. Proves the whole pipeline
    /// end-to-end before the real Vulkan encoder exists.
    pub const DISPLAY_CLEAR: u32 = 0x0100;
    /// DRM/KMS present payload: a [`super::ScanoutPresent`] — the guest already
    /// holds pixels in a contiguous framebuffer (drawn by fbcon / a compositor) and
    /// asks the host to scan it out. The host *reads* the framebuffer from guest RAM
    /// (opposite direction to [`DISPLAY_CLEAR`], which writes) and presents it. This
    /// is what the real Linux DRM/KMS guest driver submits on every page-flip.
    pub const DISPLAY_SCANOUT: u32 = 0x0101;
    /// Like [`DISPLAY_SCANOUT`] but the payload is a [`super::ScanoutPresentDamaged`],
    /// which appends a damage rect (`dx,dy,dw,dh`): only that sub-region changed since the
    /// last present, so the host reads/converts/encodes just those rows into a persistent
    /// per-VM scanout surface. Additive — a device advertising `DISPLAY_ACCEL`
    /// (`regs::caps`) accepts it; a legacy guest keeps sending full-frame `DISPLAY_SCANOUT`.
    pub const DISPLAY_SCANOUT_DAMAGE: u32 = 0x0102;
}

/// `ResourceCreateBlob::blob_mem` (virtio-gpu blob semantics).
pub mod blob_mem {
    pub const GUEST: u32 = 1;
    pub const HOST3D: u32 = 2;
    pub const HOST3D_GUEST: u32 = 3;
}

/// Scanout pixel formats (subset; extend as needed). Values are our own, not fourcc.
pub mod format {
    pub const B8G8R8A8: u32 = 1;
    pub const R8G8B8A8: u32 = 2;
    pub const B8G8R8X8: u32 = 3;
}

/// [`CursorUpdate::flags`] bits (see `docs/adr/CLIENT-PLANE-COMPOSITOR.md` D2). All bits are
/// frozen now — the runtime for `WARP`/`RELATIVE` is deferred, but reserving the bits is free and
/// forecloses an expensive body re-freeze later.
pub mod cursor_flags {
    /// Clear = **hide** the cursor (text caret, plane HW→SW handoff, relative-lock). Set = visible.
    pub const VISIBLE: u32 = 1 << 0;
    /// Only `pos_*` is fresh; retain the last-defined shape (a pure move).
    pub const MOVE_ONLY: u32 = 1 << 1;
    /// `shape_ref` is a `ResourceTable` res_id (post-PR4); clear = a guest-physical address.
    pub const SHAPE_BY_RESID: u32 = 1 << 2;
    /// The sprite's alpha is premultiplied (the DRM cursor default) — the viewer blends
    /// `ONE / ONE_MINUS_SRC_ALPHA`; clear = straight alpha.
    pub const PREMULTIPLIED: u32 = 1 << 3;
    /// `pos_*` is an authoritative **teleport**: a driving client-composite viewer must snap its
    /// local pointer notion to it (guest-initiated warp / recenter), not ignore it as routine move.
    pub const WARP: u32 = 1 << 4;
    /// The guest entered pointer-lock / relative mode: the overlay hides and the viewer grabs the
    /// pointer, sending relative deltas. Runtime deferred (PR-C7); the bit is frozen now.
    pub const RELATIVE: u32 = 1 << 5;
}

/// [`Descriptor::flags`] bits.
pub mod desc_flags {
    /// This descriptor carries an inline fence (`SubmitCmd::out_fence` valid).
    pub const FENCED: u32 = 1 << 0;
    /// Payload bytes live inline right after the descriptor (small messages),
    /// rather than in the ring's data region at `data_offset`.
    pub const INLINE: u32 = 1 << 1;
}

/// Per-context index words, shared directly between guest and host via the
/// sparse-mmap index page (`regs::INDEX_PAGE + i*CMD_RING_STRIDE`). One cacheline,
/// 64-byte aligned to avoid false sharing between adjacent rings.
///
/// Ownership: guest writes `tail`/`seqno_submit`; host writes `head`/`seqno_retired`
/// /`status`. Access is plain memory with explicit `release`/`acquire` fences at the
/// producer/consumer boundaries (see `infinigpu-ring`).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, align(64))]
pub struct RingIndices {
    /// Producer index (guest bumps after publishing descriptors).
    pub tail: u32,
    /// Consumer index (host advances after consuming).
    pub head: u32,
    /// Last submitted seqno (guest).
    pub seqno_submit: u64,
    /// Highest retired seqno (host); guest reads to resolve fences.
    pub seqno_retired: u64,
    /// Per-ring error / back-pressure bits (host).
    pub status: u32,
    /// Pad to a full 64-byte cacheline.
    pub _reserved: [u32; 9],
}

/// A ring descriptor: a fixed 32-byte record referencing one message's payload.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Descriptor {
    /// [`msg_type`] tag.
    pub msg_type: u32,
    /// [`desc_flags`] bits.
    pub flags: u32,
    /// Payload length in bytes.
    pub len: u32,
    /// Byte offset of the payload within the ring's data region (ignored when
    /// [`desc_flags::INLINE`] is set).
    pub data_offset: u32,
    /// Submission seqno for this descriptor.
    pub seqno: u64,
    pub _reserved: u64,
}

/// TLV message header (`{type, length}`) for skip-unknown forward-compat. Precedes
/// a fixed or postcard-encoded message body.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct MsgHeader {
    pub msg_type: u32,
    /// Body length in bytes, not including this header.
    pub length: u32,
}

/// Geometry header describing a command/control ring (magic + ABI + capacity).
/// For the inline-header transport it sits at the ring base; for vfio-user the
/// same values are mirrored in device registers.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct RingGeometry {
    /// [`crate::ids::DEV_MAGIC`].
    pub magic: u32,
    pub abi_major: u16,
    pub abi_minor: u16,
    /// Number of descriptor slots (power of two).
    pub capacity: u32,
    /// Descriptor stride in bytes ([`DESCRIPTOR_SIZE`]).
    pub desc_stride: u32,
    /// Byte offset of the data region from the ring base.
    pub data_offset: u32,
    /// Length of the data region in bytes.
    pub data_len: u32,
    pub flags: u32,
    pub _reserved: u32,
}

// ---- Control-ring messages (fixed bodies; variable-shape ones use postcard) ----

/// `NEGOTIATE` request/response body.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Negotiate {
    pub abi_major: u16,
    pub abi_minor: u16,
    /// Requested (or, in the response, granted) capset bitmap ([`capset`]).
    pub capsets: u32,
    pub flags: u32,
    pub _reserved: u32,
}

/// `CTX_CREATE` body.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct CtxCreate {
    pub ctx_id: u32,
    pub capset_id: u32,
    /// [`api_type`].
    pub api_type: u32,
    pub flags: u32,
}

/// `RESOURCE_CREATE_BLOB` body.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct ResourceCreateBlob {
    pub res_id: u32,
    pub ctx_id: u32,
    /// [`blob_mem`].
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub size: u64,
}

/// `RESOURCE_ATTACH_BACKING` body — a fixed header followed by `num_entries` [`MemEntry`]s (the
/// guest-physical segments that back a blob). The host records them in its per-VM `ResourceTable`
/// and later resolves each through the IOVA table before any dereference. `num_entries` is bounded
/// by the host (fail-closed) so a hostile guest can't force an unbounded read.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct AttachBacking {
    pub res_id: u32,
    /// Number of [`MemEntry`]s that follow this header in the payload.
    pub num_entries: u32,
}

/// One guest-physical backing segment (follows an [`AttachBacking`] header). Mirrors virtio-gpu's
/// `virtio_gpu_mem_entry`: an address + length pair. `length` sums (overflow-checked host-side)
/// must cover the blob's declared size.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct MemEntry {
    pub addr: u64,
    pub length: u64,
}

/// `RESOURCE_MAP_BLOB` body.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct MapBlob {
    pub res_id: u32,
    pub flags: u32,
    /// Offset into the BAR2 aperture at which to window this blob.
    pub offset: u64,
}

/// `SUBMIT_CMD` body (followed by `payload_len` opaque bytes).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct SubmitCmd {
    pub ctx_id: u32,
    /// [`encoding`].
    pub encoding: u32,
    pub payload_len: u32,
    pub flags: u32,
    pub seqno: u64,
    /// Wait for this seqno before executing (0 = none).
    pub in_fence: u64,
    /// Signal this seqno on completion (0 = none).
    pub out_fence: u64,
}

/// Inline fence record / `FENCE_WAIT` body.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Fence {
    pub ctx_id: u32,
    pub flags: u32,
    pub seqno: u64,
}

/// `SET_SCANOUT_BLOB` body.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct SetScanoutBlob {
    pub scanout_id: u32,
    pub res_id: u32,
    pub width: u32,
    pub height: u32,
    /// [`format`].
    pub format: u32,
    pub stride: u32,
}

/// `RESOURCE_FLUSH` body — "present"; host imports the blob dma-buf, encodes, and
/// feeds the console relay. Rect is the damaged region.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct ResourceFlush {
    pub res_id: u32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub _reserved: u32,
}

/// Phase-0 `DISPLAY_CLEAR` payload (see [`encoding::DISPLAY_CLEAR`]): render an
/// `width`×`height` image cleared to `rgba` (linear 0.0–1.0) and DMA-write the
/// resulting `R8G8B8A8` pixels to guest address `scanout_addr`.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct ClearPresent {
    pub width: u32,
    pub height: u32,
    pub rgba: [f32; 4],
    pub scanout_addr: u64,
}

/// [`VulkanWorkload::op`] — the hand-rolled Vulkan workload the guest's thin ICD names for
/// the host to replay (Phase-0 own-remoting subset; a fuller ICD serializes a vkCmd* stream
/// into a trailing opaque region — `SubmitCmd::payload_len` past this fixed header).
pub mod vk_op {
    /// Clear an image to `bg` on the host GPU (a real `vkCmdClear`/render-pass LOAD_CLEAR).
    pub const CLEAR: u32 = 0;
    /// Draw a shader-executed triangle over `bg` — real SM/pipeline execution on the host GPU.
    pub const TRIANGLE: u32 = 1;
}

/// `SUBMIT_CMD` payload for [`encoding::VULKAN_VENUSLIKE`] — the Phase-0 own-remoting 3D
/// subset (`docs/adr/3D-ACCEL-IMPLEMENTATION.md`, Step 4/5). A guest names one hand-rolled
/// Vulkan workload ([`vk_op`]); the host **replays it against real Vulkan (ash) on the physical
/// GPU** and DMA-writes the `R8G8B8A8` result to `scanout_addr` for the guest's page-flip —
/// the same present shape as [`ClearPresent`], but the pixels come from GPU pipeline execution,
/// not a fixed clear. This is our own decoder — no Mesa venus / virglrenderer dependency, so it
/// runs on the stock host driver. `bg` is the clear colour (or the triangle's background).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct VulkanWorkload {
    /// [`vk_op`].
    pub op: u32,
    pub width: u32,
    pub height: u32,
    pub _pad: u32,
    pub bg: [f32; 4],
    pub scanout_addr: u64,
}

/// `DISPLAY_SCANOUT` payload (see [`encoding::DISPLAY_SCANOUT`]): the guest's real
/// DRM/KMS driver hands the host a contiguous framebuffer to present. `scanout_addr`
/// is the guest-physical base of the framebuffer (a `dma_addr_t` from
/// `drm_fb_dma_get_gem_addr`); the host reads `pitch * height` bytes and interprets
/// them as `format` ([`format`]). No render is implied — this is a pure 2D scan-out
/// of pixels the guest already produced.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct ScanoutPresent {
    pub width: u32,
    pub height: u32,
    /// Bytes per row (may exceed `width * 4` for alignment).
    pub pitch: u32,
    /// [`format`] tag; fbcon's default 32-bpp buffer is `XRGB8888` = [`format::B8G8R8X8`].
    pub format: u32,
    /// Guest-physical base address of the framebuffer.
    pub scanout_addr: u64,
}

/// `DISPLAY_SCANOUT_DAMAGE` payload (see [`encoding::DISPLAY_SCANOUT_DAMAGE`]): a
/// [`ScanoutPresent`] **superset** that appends a damage rect. The first four fields plus
/// `scanout_addr` are byte-identical to [`ScanoutPresent`] (same offsets), so a decoder can
/// read the common prefix from either. `dx,dy,dw,dh` describe the only region that changed
/// since the previous present, in the same pixel space as `width`/`height`. The guest fills
/// it from the merged clip `drm_atomic_helper_damage_merged` already computes; a full-frame
/// present sets `dx=dy=0, dw=width, dh=height` (e.g. the first flip after a modeset, or when
/// no damage is known).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct ScanoutPresentDamaged {
    pub width: u32,
    pub height: u32,
    /// Bytes per row (may exceed `width * 4` for alignment).
    pub pitch: u32,
    /// [`format`] tag; fbcon's default 32-bpp buffer is `XRGB8888` = [`format::B8G8R8X8`].
    pub format: u32,
    /// Guest-physical base address of the framebuffer.
    pub scanout_addr: u64,
    /// Damage rect origin x.
    pub dx: u32,
    /// Damage rect origin y.
    pub dy: u32,
    /// Damage rect width (`dw==width && dh==height` is a full-frame present).
    pub dw: u32,
    /// Damage rect height.
    pub dh: u32,
}

/// `CURSOR_UPDATE` (`msg_type::CURSOR_UPDATE = 0x0042`) body — the guest reports its cursor plane
/// out-of-band so the cursor leaves the primary framebuffer (see
/// `docs/adr/CLIENT-PLANE-COMPOSITOR.md`). The device forwards it to a client-side overlay (the
/// zero-lag path) or composites it host-side (deferred fallback). Position/hotspot are carried for
/// the fallback, for view-only viewers in the multi-client case, and for `WARP` correction — even
/// though a driving client-composite viewer normally draws at its own local pointer. Additive
/// (ABI 0.3); a peer that doesn't negotiate `caps::CURSOR_PLANE` never sends or receives it.
///
/// The decoder reads it with the `min(payload_len, size_of)` zero-filled rule (ADR-0004), so a
/// future field carved out of `_reserved` never breaks an older host.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct CursorUpdate {
    /// Which head (`MAX_SCANOUTS == 1` today; cheap future-proofing for multi-head).
    pub scanout_id: u32,
    /// [`cursor_flags`] bits.
    pub flags: u32,
    /// Cursor origin x (`crtc_x`) — **signed**: the hotspot pushes the origin negative at a screen
    /// edge, which a `u32` would silently drop.
    pub pos_x: i32,
    /// Cursor origin y (`crtc_y`).
    pub pos_y: i32,
    /// Hotspot x within the sprite.
    pub hot_x: u16,
    /// Hotspot y within the sprite.
    pub hot_y: u16,
    /// Sprite width in pixels (`0` when `MOVE_ONLY` / hidden).
    pub width: u16,
    /// Sprite height in pixels.
    pub height: u16,
    /// Sprite bytes per row (validated `>= width*4`).
    pub pitch: u32,
    /// [`format`] tag; the DRM cursor default is premultiplied ARGB8888 = [`format::B8G8R8A8`]
    /// (the device accepts only this and fails closed otherwise).
    pub format: u32,
    /// Guest-physical address of the ARGB sprite, or a `ResourceTable` res_id when
    /// [`cursor_flags::SHAPE_BY_RESID`] is set.
    pub shape_ref: u64,
    /// Additive headroom (0 today).
    pub _reserved: u64,
}
