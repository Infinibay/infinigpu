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
    // Control / async (host -> guest)
    pub const RESET: u32 = 0x0050;
    pub const EVENT: u32 = 0x0051;
}

/// Capset bitmap (which command sub-protocol a context speaks).
pub mod capset {
    pub const CAP_VULKAN: u32 = 1 << 0;
    pub const CAP_D3D12: u32 = 1 << 1;
    pub const CAP_DISPLAY_ONLY: u32 = 1 << 2;
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
