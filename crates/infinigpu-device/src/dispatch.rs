//! Descriptor execution — **phase 2** of the PR4 drain (`drain.rs` is phase 1). Maps one drained
//! [`Descriptor`] plus its DMA-read payload to a per-VM [`ResourceTable`] operation, fail-closed.
//!
//! This is the host-side decode for the blob-resource control messages
//! (`RESOURCE_CREATE_BLOB` / `RESOURCE_ATTACH_BACKING` / `SET_SCANOUT_BLOB` / `RESOURCE_FLUSH` /
//! `RESOURCE_DESTROY`). It is a **pure function over borrowed bytes** — it never dereferences guest
//! memory (the caller resolves a flushed blob's backing later via `DmaTable::host_ptr`, under
//! `&mut self`), so the whole decode/validation spine is unit-testable off-hardware. Every
//! malformed or hostile input yields a typed [`Executed`] variant, never a panic:
//!
//! - a short payload → [`Executed::ShortPayload`] (dropped, ring still retires),
//! - a table rejection (dup id, oversized, unknown res, bad dims, backing too small) →
//!   [`Executed::Rejected`],
//! - a `msg_type` this dispatcher doesn't own (display / vulkan submits) → [`Executed::NotResource`]
//!   so the caller routes it to `process_ring`'s existing arms.

use crate::resource::{BackingSegment, ResourceError, ResourceTable};
use infinigpu_abi::wire::{
    msg_type, AttachBacking, Descriptor, MemEntry, ResourceCreateBlob, ResourceFlush, SetScanoutBlob,
};
use zerocopy::FromBytes;

/// Fail-closed bound on the `MemEntry` count in one `ATTACH_BACKING`. A 64 MiB blob over 4 KiB
/// pages is ≤16Ki segments; this caps the array read so a hostile `num_entries` can't drive an
/// unbounded allocation even before the `ResourceTable`'s coverage check runs.
pub const MAX_BACKING_ENTRIES: u32 = 16 * 1024;

/// Outcome of executing one descriptor against the [`ResourceTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Executed {
    /// `RESOURCE_CREATE_BLOB` accepted for this res_id.
    CreatedBlob(u32),
    /// `RESOURCE_ATTACH_BACKING` recorded `segments` guest-physical segments for this res_id.
    AttachedBacking { res_id: u32, segments: usize },
    /// `SET_SCANOUT_BLOB` bound this scanout head.
    SetScanout(u32),
    /// `RESOURCE_DESTROY` dropped this res_id (and any scanout bound to it).
    Destroyed(u32),
    /// `RESOURCE_FLUSH`: a validated present request — resource `res_id` exists and is backed;
    /// damage `rect` is `(x, y, w, h)` as the guest sent it (the caller clamps + reads + streams
    /// under `&mut self`). This dispatcher does not touch guest memory.
    Flush { res_id: u32, rect: (u32, u32, u32, u32) },
    /// Recognized resource op, rejected fail-closed by the table (or a hostile entry count).
    Rejected(ResourceError),
    /// A `msg_type` this dispatcher doesn't own (e.g. `SUBMIT_CMD`) — route it elsewhere.
    NotResource,
    /// Payload shorter than the declared body / entry array — dropped fail-closed.
    ShortPayload,
}

/// Execute one drained descriptor's resource op. `payload` is the descriptor body already DMA-read
/// from guest RAM (`desc.len` bytes). See the module docs for the fail-closed contract.
pub fn execute_resource(desc: &Descriptor, payload: &[u8], table: &mut ResourceTable) -> Executed {
    match desc.msg_type {
        msg_type::RESOURCE_CREATE_BLOB => {
            let Ok((b, _)) = ResourceCreateBlob::read_from_prefix(payload) else {
                return Executed::ShortPayload;
            };
            match table.create_blob(b.res_id, b.blob_mem, b.size) {
                Ok(()) => Executed::CreatedBlob(b.res_id),
                Err(e) => Executed::Rejected(e),
            }
        }
        msg_type::RESOURCE_ATTACH_BACKING => {
            let Ok((hdr, rest)) = AttachBacking::read_from_prefix(payload) else {
                return Executed::ShortPayload;
            };
            if hdr.num_entries > MAX_BACKING_ENTRIES {
                return Executed::Rejected(ResourceError::BackingTooSmall);
            }
            let entry_sz = core::mem::size_of::<MemEntry>();
            let need = hdr.num_entries as usize * entry_sz;
            if rest.len() < need {
                return Executed::ShortPayload;
            }
            let mut segs = Vec::with_capacity(hdr.num_entries as usize);
            for i in 0..hdr.num_entries as usize {
                // Exact-size read of each fixed 16-byte MemEntry (bounds pre-checked above).
                let e = MemEntry::read_from_bytes(&rest[i * entry_sz..(i + 1) * entry_sz]).unwrap();
                segs.push(BackingSegment { addr: e.addr, len: e.length });
            }
            match table.attach_backing(hdr.res_id, &segs) {
                Ok(()) => Executed::AttachedBacking { res_id: hdr.res_id, segments: segs.len() },
                Err(e) => Executed::Rejected(e),
            }
        }
        msg_type::SET_SCANOUT_BLOB => {
            let Ok((b, _)) = SetScanoutBlob::read_from_prefix(payload) else {
                return Executed::ShortPayload;
            };
            match table.set_scanout(b.scanout_id, b.res_id, b.width, b.height, b.format, b.stride) {
                Ok(()) => Executed::SetScanout(b.scanout_id),
                Err(e) => Executed::Rejected(e),
            }
        }
        msg_type::RESOURCE_FLUSH => {
            let Ok((b, _)) = ResourceFlush::read_from_prefix(payload) else {
                return Executed::ShortPayload;
            };
            // Validate the target exists and is backed before routing to the present path — a flush
            // on an unknown or un-backed resource can't produce pixels.
            match table.get(b.res_id) {
                None => Executed::Rejected(ResourceError::Unknown(b.res_id)),
                Some(r) if r.backing.is_empty() => {
                    Executed::Rejected(ResourceError::BackingTooSmall)
                }
                Some(_) => Executed::Flush { res_id: b.res_id, rect: (b.x, b.y, b.w, b.h) },
            }
        }
        msg_type::RESOURCE_DESTROY => {
            // Destroy carries a bare leading `res_id: u32` (no dedicated wire body).
            let Ok((res_id, _)) = u32::read_from_prefix(payload) else {
                return Executed::ShortPayload;
            };
            match table.destroy(res_id) {
                Ok(()) => Executed::Destroyed(res_id),
                Err(e) => Executed::Rejected(e),
            }
        }
        _ => Executed::NotResource,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use infinigpu_abi::wire::{format, msg_type};
    use zerocopy::IntoBytes;

    fn desc(msg_type: u32) -> Descriptor {
        Descriptor { msg_type, flags: 0, len: 0, data_offset: 0, seqno: 1, payload_addr: 0 }
    }

    fn create_blob_payload(res_id: u32, size: u64) -> Vec<u8> {
        ResourceCreateBlob { res_id, ctx_id: 1, blob_mem: 1, blob_flags: 0, size }
            .as_bytes()
            .to_vec()
    }

    fn attach_payload(res_id: u32, entries: &[(u64, u64)]) -> Vec<u8> {
        let mut v = AttachBacking { res_id, num_entries: entries.len() as u32 }.as_bytes().to_vec();
        for &(addr, length) in entries {
            v.extend_from_slice(MemEntry { addr, length }.as_bytes());
        }
        v
    }

    fn set_scanout_payload(scanout_id: u32, res_id: u32, w: u32, h: u32, stride: u32) -> Vec<u8> {
        SetScanoutBlob { scanout_id, res_id, width: w, height: h, format: format::B8G8R8A8, stride }
            .as_bytes()
            .to_vec()
    }

    fn flush_payload(res_id: u32, rect: (u32, u32, u32, u32)) -> Vec<u8> {
        ResourceFlush { res_id, x: rect.0, y: rect.1, w: rect.2, h: rect.3, _reserved: 0 }
            .as_bytes()
            .to_vec()
    }

    #[test]
    fn full_lifecycle_create_attach_scanout_flush_destroy() {
        let mut t = ResourceTable::new();
        let sz = 1920 * 1080 * 4;
        assert_eq!(
            execute_resource(&desc(msg_type::RESOURCE_CREATE_BLOB), &create_blob_payload(7, sz), &mut t),
            Executed::CreatedBlob(7)
        );
        // Backing that fully covers the blob (two segments).
        assert_eq!(
            execute_resource(
                &desc(msg_type::RESOURCE_ATTACH_BACKING),
                &attach_payload(7, &[(0x1000, sz / 2), (0x900000, sz / 2)]),
                &mut t
            ),
            Executed::AttachedBacking { res_id: 7, segments: 2 }
        );
        assert_eq!(
            execute_resource(
                &desc(msg_type::SET_SCANOUT_BLOB),
                &set_scanout_payload(0, 7, 1920, 1080, 1920 * 4),
                &mut t
            ),
            Executed::SetScanout(0)
        );
        // Flush on the backed resource routes to a present with the guest's rect.
        assert_eq!(
            execute_resource(&desc(msg_type::RESOURCE_FLUSH), &flush_payload(7, (10, 20, 100, 50)), &mut t),
            Executed::Flush { res_id: 7, rect: (10, 20, 100, 50) }
        );
        // Destroy drops it (and its scanout).
        assert_eq!(
            execute_resource(&desc(msg_type::RESOURCE_DESTROY), 7u32.as_bytes(), &mut t),
            Executed::Destroyed(7)
        );
        assert!(t.get(7).is_none());
    }

    #[test]
    fn flush_on_unknown_or_unbacked_resource_is_rejected() {
        let mut t = ResourceTable::new();
        // Unknown resource.
        assert_eq!(
            execute_resource(&desc(msg_type::RESOURCE_FLUSH), &flush_payload(9, (0, 0, 1, 1)), &mut t),
            Executed::Rejected(ResourceError::Unknown(9))
        );
        // Created but no backing yet → can't present.
        execute_resource(&desc(msg_type::RESOURCE_CREATE_BLOB), &create_blob_payload(9, 4096), &mut t);
        assert_eq!(
            execute_resource(&desc(msg_type::RESOURCE_FLUSH), &flush_payload(9, (0, 0, 1, 1)), &mut t),
            Executed::Rejected(ResourceError::BackingTooSmall)
        );
    }

    #[test]
    fn short_payloads_are_dropped_fail_closed() {
        let mut t = ResourceTable::new();
        // Truncated create-blob body.
        assert_eq!(
            execute_resource(&desc(msg_type::RESOURCE_CREATE_BLOB), &[0u8; 4], &mut t),
            Executed::ShortPayload
        );
        // Header claims 3 entries but only one is present.
        let mut p = AttachBacking { res_id: 1, num_entries: 3 }.as_bytes().to_vec();
        p.extend_from_slice(MemEntry { addr: 0, length: 8 }.as_bytes());
        // create the blob first so the header parse is the only failure surface
        execute_resource(&desc(msg_type::RESOURCE_CREATE_BLOB), &create_blob_payload(1, 4096), &mut t);
        assert_eq!(
            execute_resource(&desc(msg_type::RESOURCE_ATTACH_BACKING), &p, &mut t),
            Executed::ShortPayload
        );
    }

    #[test]
    fn hostile_entry_count_is_rejected_before_allocation() {
        let mut t = ResourceTable::new();
        execute_resource(&desc(msg_type::RESOURCE_CREATE_BLOB), &create_blob_payload(1, 4096), &mut t);
        // A huge num_entries (with no actual entries) must be rejected by the cap, not OOM.
        let hostile = AttachBacking { res_id: 1, num_entries: u32::MAX }.as_bytes().to_vec();
        assert_eq!(
            execute_resource(&desc(msg_type::RESOURCE_ATTACH_BACKING), &hostile, &mut t),
            Executed::Rejected(ResourceError::BackingTooSmall)
        );
    }

    #[test]
    fn duplicate_and_oversized_blobs_are_rejected() {
        let mut t = ResourceTable::new();
        assert_eq!(
            execute_resource(&desc(msg_type::RESOURCE_CREATE_BLOB), &create_blob_payload(1, 4096), &mut t),
            Executed::CreatedBlob(1)
        );
        // Duplicate id.
        assert_eq!(
            execute_resource(&desc(msg_type::RESOURCE_CREATE_BLOB), &create_blob_payload(1, 4096), &mut t),
            Executed::Rejected(ResourceError::Duplicate(1))
        );
        // Oversized (> MAX_BLOB_BYTES).
        let big = create_blob_payload(2, crate::resource::MAX_BLOB_BYTES + 1);
        assert!(matches!(
            execute_resource(&desc(msg_type::RESOURCE_CREATE_BLOB), &big, &mut t),
            Executed::Rejected(ResourceError::BadSize { .. })
        ));
    }

    #[test]
    fn non_resource_msg_types_are_passed_through() {
        let mut t = ResourceTable::new();
        assert_eq!(
            execute_resource(&desc(msg_type::SUBMIT_CMD), &[], &mut t),
            Executed::NotResource
        );
        assert_eq!(
            execute_resource(&desc(msg_type::CURSOR_UPDATE), &[], &mut t),
            Executed::NotResource
        );
    }
}
