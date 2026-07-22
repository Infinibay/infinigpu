//! # infinigpu-abi
//!
//! The `no_std`, no-alloc **wire ABI** shared by the infinigpu host backend and
//! both guest drivers (Linux DRM/KMS, Windows WDDM/IddCx). It is the single
//! source of truth for:
//!
//! - the PCI identity the guest binds on ([`ids`]),
//! - the BAR0 register map + control bits ([`regs`]),
//! - the fixed-layout framing structs ([`wire`]).
//!
//! This crate changes only through the versioning discipline in ADR-0004:
//! ABI `major.minor` in the ring header, capset negotiation, reserved trailing
//! padding, and TLV message headers. It is exported to the C Linux KMD via a
//! cbindgen header + a round-trip conformance test.
//!
//! Design references: ADR-0001 (host device seam), ADR-0004 (wire protocol),
//! research/11 (protocol design), research/24 (register-level device spec).

#![cfg_attr(not(test), no_std)]

pub mod ids;
pub mod regs;
pub mod wire;

pub use ids::{abi_version, ABI_MAJOR, ABI_MINOR, DEV_MAGIC, PCI_DEVICE_ID, PCI_VENDOR_ID};

/// Compile-time guarantees on struct sizes and field offsets. A mismatch here is
/// a hard build error — the ABI cannot silently drift, and the C header generated
/// from these structs (cbindgen) is pinned to the same layout.
mod layout_asserts {
    use crate::wire::*;
    use core::mem::{align_of, offset_of, size_of};

    // RingIndices: exactly one 64-byte cacheline, fields at the research/24 offsets.
    const _: () = assert!(size_of::<RingIndices>() == 64);
    const _: () = assert!(align_of::<RingIndices>() == 64);
    const _: () = assert!(offset_of!(RingIndices, tail) == 0x00);
    const _: () = assert!(offset_of!(RingIndices, head) == 0x04);
    const _: () = assert!(offset_of!(RingIndices, seqno_submit) == 0x08);
    const _: () = assert!(offset_of!(RingIndices, seqno_retired) == 0x10);
    const _: () = assert!(offset_of!(RingIndices, status) == 0x18);

    // Descriptor is a 32-byte power-of-two record.
    const _: () = assert!(size_of::<Descriptor>() == DESCRIPTOR_SIZE);
    const _: () = assert!(offset_of!(Descriptor, seqno) == 16);

    const _: () = assert!(size_of::<MsgHeader>() == 8);
    const _: () = assert!(size_of::<RingGeometry>() == 32);

    // Control-message bodies (no internal padding — required for zerocopy IntoBytes).
    const _: () = assert!(size_of::<Negotiate>() == 16);
    const _: () = assert!(size_of::<CtxCreate>() == 16);
    const _: () = assert!(size_of::<ResourceCreateBlob>() == 24);
    // AttachBacking header (8B) + a MemEntry array (16B each, addr@0/length@8) — padding-free so a
    // `[MemEntry]` reads directly out of the payload after the header.
    const _: () = assert!(size_of::<AttachBacking>() == 8);
    const _: () = assert!(size_of::<MemEntry>() == 16);
    const _: () = assert!(offset_of!(MemEntry, length) == 8);
    const _: () = assert!(align_of::<MemEntry>() == 8);
    const _: () = assert!(size_of::<MapBlob>() == 16);
    const _: () = assert!(size_of::<SubmitCmd>() == 40);
    const _: () = assert!(offset_of!(SubmitCmd, seqno) == 16);
    const _: () = assert!(size_of::<Fence>() == 16);
    const _: () = assert!(size_of::<SetScanoutBlob>() == 24);
    const _: () = assert!(size_of::<ResourceFlush>() == 24);
    const _: () = assert!(size_of::<ClearPresent>() == 32);
    const _: () = assert!(offset_of!(ClearPresent, scanout_addr) == 24);
    // VulkanWorkload (VULKAN_VENUSLIKE payload): op/w/h/_pad (16) + bg[4] (16) + scanout_addr (8);
    // scanout_addr must stay 8-aligned so the u64 reads directly out of the payload.
    const _: () = assert!(size_of::<VulkanWorkload>() == 40);
    const _: () = assert!(offset_of!(VulkanWorkload, scanout_addr) == 32);
    // ForwardedDrawTail (vk_op::FORWARDED): 6×u32 = 24, 4-byte aligned; the SPIR-V blobs +
    // entry-name strings follow it in the payload (variable length).
    const _: () = assert!(size_of::<ForwardedDrawTail>() == 24);
    const _: () = assert!(align_of::<ForwardedDrawTail>() == 4);
    // ForwardedCmdListTail (vk_op::FORWARDED_CMDLIST): 20×u32 = 80, 4-byte aligned (ABI 0.9 added
    // push_const_len; 0.10 tex_count; 0.11 ubo_len/ubo_binding/tex_binding; 0.12 raster_flags; 0.13
    // ssbo_len/ssbo_binding; 0.14 renamed ubo_binding→ubo_count, same offset — the UBO block is now
    // `ubo_count` self-describing [binding][len][bytes] records). Its trailing VertexAttrWire[]/
    // DrawCmdWire[]/TextureDescWire[] arrays are 4-multiples so they stay 4-aligned.
    const _: () = assert!(size_of::<ForwardedCmdListTail>() == 80);
    const _: () = assert!(align_of::<ForwardedCmdListTail>() == 4);
    const _: () = assert!(offset_of!(ForwardedCmdListTail, push_const_len) == 48);
    const _: () = assert!(offset_of!(ForwardedCmdListTail, tex_count) == 52);
    const _: () = assert!(offset_of!(ForwardedCmdListTail, ubo_len) == 56);
    const _: () = assert!(offset_of!(ForwardedCmdListTail, ubo_count) == 60);
    const _: () = assert!(offset_of!(ForwardedCmdListTail, tex_binding) == 64);
    const _: () = assert!(offset_of!(ForwardedCmdListTail, raster_flags) == 68);
    const _: () = assert!(offset_of!(ForwardedCmdListTail, ssbo_len) == 72);
    const _: () = assert!(offset_of!(ForwardedCmdListTail, ssbo_binding) == 76);
    const _: () = assert!(size_of::<TextureDescWire>() == 16);
    const _: () = assert!(align_of::<TextureDescWire>() == 4);
    const _: () = assert!(size_of::<VertexAttrWire>() == 12);
    const _: () = assert!(align_of::<VertexAttrWire>() == 4);
    const _: () = assert!(size_of::<DrawCmdWire>() == 32);
    const _: () = assert!(align_of::<DrawCmdWire>() == 4);
    const _: () = assert!(size_of::<ScanoutPresent>() == 24);
    const _: () = assert!(offset_of!(ScanoutPresent, scanout_addr) == 16);
    // ScanoutPresentDamaged is a ScanoutPresent superset (same prefix + scanout_addr@16)
    // with a trailing damage rect. Its prefix MUST stay byte-identical so a decoder can read
    // the common fields from either.
    const _: () = assert!(size_of::<ScanoutPresentDamaged>() == 40);
    const _: () = assert!(offset_of!(ScanoutPresentDamaged, scanout_addr) == 16);
    const _: () = assert!(offset_of!(ScanoutPresentDamaged, dx) == 24);
    const _: () = assert!(offset_of!(ScanoutPresentDamaged, dh) == 36);
    // CursorUpdate: 48-byte, padding-free (pos_x signed@8, hot_x@16, pitch@24, format@28,
    // shape_ref u64@32, _reserved@40). Frozen now so device/viewer build against a stable body.
    const _: () = assert!(size_of::<CursorUpdate>() == 48);
    const _: () = assert!(offset_of!(CursorUpdate, pos_x) == 8);
    const _: () = assert!(offset_of!(CursorUpdate, hot_x) == 16);
    const _: () = assert!(offset_of!(CursorUpdate, pitch) == 24);
    const _: () = assert!(offset_of!(CursorUpdate, format) == 28);
    const _: () = assert!(offset_of!(CursorUpdate, shape_ref) == 32);
    const _: () = assert!(offset_of!(CursorUpdate, _reserved) == 40);
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::*;
    use zerocopy::{FromBytes, IntoBytes};

    #[test]
    fn magic_is_igpu_ascii() {
        assert_eq!(DEV_MAGIC, 0x4947_5055);
        assert_eq!(&DEV_MAGIC.to_be_bytes(), b"IGPU");
    }

    #[test]
    fn abi_version_packs_major_minor() {
        assert_eq!(
            abi_version(),
            (u32::from(ABI_MAJOR) << 16) | u32::from(ABI_MINOR)
        );
        assert_eq!(abi_version(), 0x0000_000D);
    }

    #[test]
    fn device_id_avoids_qxl_collision() {
        // ERRATA #6: 0x0100 is QXL under vendor 0x1B36; we must not use it.
        assert_ne!(PCI_DEVICE_ID, 0x0100);
        assert_eq!(PCI_VENDOR_ID, 0x1B36);
    }

    #[test]
    fn phase0_caps_are_poll_not_ioeventfd() {
        let c = regs::PHASE0_DEV_CAPS;
        assert!(c & regs::caps::POLL_SUBMIT != 0);
        // The stock vfio-user v0.1.3 crate has no ioeventfd — must not advertise it.
        assert!(c & regs::caps::IOEVENTFD_DOORBELL == 0);
    }

    #[test]
    fn submit_cmd_round_trips_through_bytes() {
        let cmd = SubmitCmd {
            ctx_id: 7,
            encoding: encoding::VULKAN_VENUSLIKE,
            payload_len: 512,
            flags: 0,
            seqno: 0xDEAD_BEEF_0000_0001,
            in_fence: 0,
            out_fence: 42,
        };
        let bytes = cmd.as_bytes();
        assert_eq!(bytes.len(), 40);
        let back = SubmitCmd::read_from_bytes(bytes).unwrap();
        assert_eq!(back.ctx_id, 7);
        assert_eq!(back.seqno, 0xDEAD_BEEF_0000_0001);
        assert_eq!(back.out_fence, 42);
    }

    #[test]
    fn ring_indices_default_zeroed_is_valid() {
        // FromBytes guarantees an all-zero page is a valid, empty ring.
        let zero = [0u8; 64];
        let idx = RingIndices::read_from_bytes(&zero).unwrap();
        assert_eq!(idx.tail, 0);
        assert_eq!(idx.head, 0);
        assert_eq!(idx.seqno_retired, 0);
    }

    #[test]
    fn hostile_short_buffer_is_rejected_not_ub() {
        // A truncated descriptor must fail cleanly, never read out of bounds.
        let short = [0u8; 8];
        assert!(Descriptor::read_from_bytes(&short).is_err());
    }
}
