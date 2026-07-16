//! BAR0 register map (research/24 §2) and control-register bit definitions.
//! All offsets are byte offsets into the relevant BAR.

/// vfio-user region indices (a region per BAR + config space at index 6).
pub mod region {
    /// Control page + shared index page + doorbell page.
    pub const BAR0: u32 = 0;
    /// MSI-X table + PBA (hand-rolled; the crate provides no MSI-X helper).
    pub const BAR1: u32 = 1;
    /// Optional blob aperture (host memfd), gated by [`super::caps::BLOB_APERTURE`].
    pub const BAR2: u32 = 2;
    /// PCI config space is vfio-user region index 6.
    pub const CONFIG: u32 = 6;
}

pub const BAR0_SIZE: u64 = 64 * 1024;
pub const BAR1_SIZE: u64 = 16 * 1024;

/// Per-context register stride, for both the trapped config block and the shared
/// index page. Context `i` lives at `base + i * CMD_RING_STRIDE`.
pub const CMD_RING_STRIDE: u64 = 0x40;

/// Max command rings (contexts): `N <= 63`. MSI-X vector 0 is device/control;
/// vectors `1..=63` are per-context completion.
pub const MAX_CONTEXTS: u32 = 63;

/// Trapped control registers (`0x0000..0x1FFF`) — served on the socket path via
/// `region_read`/`region_write`. These are a *hole* in the sparse-mmap set.
pub mod ctrl {
    pub const DEV_MAGIC: u64 = 0x0000;
    pub const ABI_VERSION: u64 = 0x0004;
    pub const DEV_CAPS: u64 = 0x0008;
    pub const NUM_CONTEXTS: u64 = 0x000C;
    pub const MAX_RING_ENTRIES: u64 = 0x0010;
    pub const BAR2_APERTURE_MB: u64 = 0x0014;
    pub const GLOBAL_CTRL: u64 = 0x0020;
    pub const GLOBAL_STATUS: u64 = 0x0024;
    pub const DEVICE_RESET: u64 = 0x0028;
    pub const IRQ_STATUS: u64 = 0x0030;
    pub const IRQ_MASK: u64 = 0x0034;
    pub const CTRL_RING_BASE_LO: u64 = 0x0040;
    pub const CTRL_RING_BASE_HI: u64 = 0x0044;
    pub const CTRL_RING_SIZE: u64 = 0x0048;
    /// Command-ring 0 highest retired seqno (host-written), pollable via a trapped
    /// read for completion sync without depending on MSI-X setup. In the production
    /// design this word lives in the mmap'd index page ([`super::INDEX_PAGE`]); it is
    /// mirrored here as a trapped register for the Phase-0 guest driver bring-up.
    pub const CMD_RING0_RETIRED_LO: u64 = 0x0050;
    pub const CMD_RING0_RETIRED_HI: u64 = 0x0054;
    /// Base of the per-context ring-config block: context `i` at
    /// `CMD_RING_CFG + i * CMD_RING_STRIDE`.
    pub const CMD_RING_CFG: u64 = 0x0100;

    // Field offsets *within* one per-context config block:
    pub const CMD_RING_BASE_LO: u64 = 0x00;
    pub const CMD_RING_BASE_HI: u64 = 0x04;
    pub const CMD_RING_SIZE: u64 = 0x08;
    pub const CMD_RING_CTRL: u64 = 0x0C;
    pub const CMD_RING_CAPSET: u64 = 0x10;
    /// Highest retired seqno for *this* context (host-written), pollable per-ring for
    /// multi-ring completion sync. Context `i` reads it at
    /// `CMD_RING_CFG + i*CMD_RING_STRIDE + CMD_RING_RETIRED_LO`. Ring 0 is also mirrored
    /// at the fixed [`CMD_RING0_RETIRED_LO`] for the Phase-0 single-ring guest driver.
    pub const CMD_RING_RETIRED_LO: u64 = 0x14;
    pub const CMD_RING_RETIRED_HI: u64 = 0x18;
}

/// Shared, directly-mmapped index page (`0x2000..0x2FFF`): one
/// [`crate::wire::RingIndices`] per context at `INDEX_PAGE + i * CMD_RING_STRIDE`.
/// Declared as a `VFIO_REGION_INFO_CAP_SPARSE_MMAP` area backed by a device memfd,
/// so guest and host share the same physical pages — index/seqno traffic is pure
/// memory access, no socket round-trip.
pub const INDEX_PAGE: u64 = 0x2000;

/// Doorbell page (`0x3000..0x3FFF`).
///
/// IMPORTANT (verified against vfio-user v0.1.3): the crate rejects
/// `GET_REGION_IO_FDS`, so these writes are **not** cheap ioeventfd kicks — a
/// doorbell write traps as a `region_write` socket round-trip. In the steady
/// state the host **polls** the shared index-page `TAIL` (SQPOLL-style); the
/// doorbell is used only to *wake* an idle poller. See [`caps::POLL_SUBMIT`].
pub mod doorbell {
    pub const PAGE: u64 = 0x3000;
    pub const CTRL: u64 = 0x3000;
    /// Context `i` doorbell at `CMD_BASE + i * 4`.
    pub const CMD_BASE: u64 = 0x3004;
}

/// `DEV_CAPS` (`0x0008`) feature bits. The device advertises what it actually
/// supports on *this* host/crate build; the guest driver gates behavior on them.
pub mod caps {
    /// True ioeventfd doorbells (a BAR write is a bare eventfd kick). **Off by
    /// default** — the stock vfio-user v0.1.3 crate rejects `GET_REGION_IO_FDS`;
    /// only set this if running against a patched crate.
    pub const IOEVENTFD_DOORBELL: u32 = 1 << 0;
    /// BAR2 blob aperture is present.
    pub const BLOB_APERTURE: u32 = 1 << 1;
    /// Device supports > 1 command ring.
    pub const MULTI_RING: u32 = 1 << 2;
    /// 64-bit seqno words in the index page.
    pub const SEQNO64: u32 = 1 << 3;
    /// Host polls the shared index page for submissions; the guest need only ring
    /// the (trapped) doorbell to wake an idle poller, not on every submit. This is
    /// the default submission model given the crate's lack of ioeventfd.
    pub const POLL_SUBMIT: u32 = 1 << 4;
}

/// `GLOBAL_CTRL` (`0x0020`) bits.
pub mod global_ctrl {
    pub const DEVICE_ENABLE: u32 = 1 << 0;
    pub const CTRL_RING_ENABLE: u32 = 1 << 1;
}

/// `GLOBAL_STATUS` (`0x0024`) bits.
pub mod global_status {
    pub const READY: u32 = 1 << 0;
    pub const FATAL: u32 = 1 << 1;
    pub const NEEDS_RESET: u32 = 1 << 2;
}

/// `CMD_RING_CTRL` bits.
pub mod ring_ctrl {
    pub const ENABLE: u32 = 1 << 0;
    pub const RESET: u32 = 1 << 1;
}

/// The `DEV_CAPS` value the Phase-0 device advertises on the stock crate:
/// polled submission, 64-bit seqno, multi-ring-capable ABI. No ioeventfd, no BAR2.
pub const PHASE0_DEV_CAPS: u32 = caps::POLL_SUBMIT | caps::SEQNO64 | caps::MULTI_RING;
