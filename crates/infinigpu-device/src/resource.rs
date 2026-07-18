//! Per-VM host-side resource table — the 2D-ADR **PR4** foundation (and the 3D foundation).
//!
//! Tracks the guest's blob resources (`RESOURCE_CREATE_BLOB`), their guest-physical backing
//! (`RESOURCE_ATTACH_BACKING`), and scanout bindings (`SET_SCANOUT_BLOB`). Every operation is
//! **fail-closed** with hard caps, so a hostile guest can neither exhaust host memory (resource
//! count / blob size) nor set up an out-of-bounds access (backing that doesn't cover the blob,
//! oversized scanout dims, a stride too small for a row). It is a pure data structure — no DMA, no
//! GPU — so it is fully unit-testable off-hardware; the ring drainer (PR4b) resolves the recorded
//! backing through `DmaTable` before any host dereference.

use std::collections::HashMap;

/// Max live resources per VM. Beyond this, `create_blob` is denied — a bound on host bookkeeping.
pub const MAX_RESOURCES: usize = 1024;
/// Largest blob a VM may create (64 MiB) — mirrors the geometry cap the present path already uses.
pub const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;
/// Largest scanout dimension (matches the host's existing framebuffer geometry guard).
pub const MAX_DIM: u32 = 16384;

/// One guest-backing segment (a `MemEntry` from `ATTACH_BACKING`): a guest-physical range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackingSegment {
    pub addr: u64,
    pub len: u64,
}

/// A host-tracked blob resource. `backing` is empty until `ATTACH_BACKING`.
#[derive(Debug, Clone)]
pub struct HostResource {
    pub res_id: u32,
    pub blob_mem: u32,
    pub size: u64,
    pub backing: Vec<BackingSegment>,
}

/// A scanout head bound to a resource (`SET_SCANOUT_BLOB`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanoutBinding {
    pub res_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub stride: u32,
}

/// Why a resource operation was rejected (all fail-closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceError {
    /// At `MAX_RESOURCES`.
    TableFull,
    /// This `res_id` already exists.
    Duplicate(u32),
    /// Zero, or larger than `MAX_BLOB_BYTES`.
    BadSize { size: u64, cap: u64 },
    /// No such `res_id`.
    Unknown(u32),
    /// Zero / oversized scanout dims, or a stride too small for one row.
    BadDims,
    /// Backing segments don't cover the blob, or a length sum overflowed.
    BackingTooSmall,
}

/// Per-VM resource table. Constructed empty; reset on device reset.
#[derive(Debug, Default)]
pub struct ResourceTable {
    resources: HashMap<u32, HostResource>,
    /// Scanout bindings keyed by `scanout_id`.
    scanouts: HashMap<u32, ScanoutBinding>,
}

impl ResourceTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// `RESOURCE_CREATE_BLOB`: register a new blob. Fail-closed on table-full, duplicate id, and a
    /// zero/oversized size.
    pub fn create_blob(&mut self, res_id: u32, blob_mem: u32, size: u64) -> Result<(), ResourceError> {
        if self.resources.len() >= MAX_RESOURCES {
            return Err(ResourceError::TableFull);
        }
        if size == 0 || size > MAX_BLOB_BYTES {
            return Err(ResourceError::BadSize { size, cap: MAX_BLOB_BYTES });
        }
        if self.resources.contains_key(&res_id) {
            return Err(ResourceError::Duplicate(res_id));
        }
        self.resources.insert(
            res_id,
            HostResource { res_id, blob_mem, size, backing: Vec::new() },
        );
        Ok(())
    }

    /// `RESOURCE_ATTACH_BACKING`: record the guest-physical segments backing a blob. The segment
    /// lengths must sum to at least the blob size (overflow-safe), or it is rejected — so a later
    /// backing-relative access can never read past what the guest actually provided.
    pub fn attach_backing(&mut self, res_id: u32, segments: &[BackingSegment]) -> Result<(), ResourceError> {
        let res = self.resources.get_mut(&res_id).ok_or(ResourceError::Unknown(res_id))?;
        let mut total: u64 = 0;
        for s in segments {
            total = total.checked_add(s.len).ok_or(ResourceError::BackingTooSmall)?;
        }
        if total < res.size {
            return Err(ResourceError::BackingTooSmall);
        }
        res.backing = segments.to_vec();
        Ok(())
    }

    /// `SET_SCANOUT_BLOB`: bind a scanout head to a resource. Fail-closed on unknown resource,
    /// zero/oversized dims, and a stride that can't cover a `width`-pixel BGRA row.
    #[allow(clippy::too_many_arguments)]
    pub fn set_scanout(
        &mut self,
        scanout_id: u32,
        res_id: u32,
        width: u32,
        height: u32,
        format: u32,
        stride: u32,
    ) -> Result<(), ResourceError> {
        if !self.resources.contains_key(&res_id) {
            return Err(ResourceError::Unknown(res_id));
        }
        if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
            return Err(ResourceError::BadDims);
        }
        if (stride as u64) < (width as u64) * 4 {
            return Err(ResourceError::BadDims);
        }
        self.scanouts.insert(
            scanout_id,
            ScanoutBinding { res_id, width, height, format, stride },
        );
        Ok(())
    }

    /// `RESOURCE_DESTROY`: drop a resource and any scanout bound to it.
    pub fn destroy(&mut self, res_id: u32) -> Result<(), ResourceError> {
        if self.resources.remove(&res_id).is_none() {
            return Err(ResourceError::Unknown(res_id));
        }
        self.scanouts.retain(|_, b| b.res_id != res_id);
        Ok(())
    }

    pub fn get(&self, res_id: u32) -> Option<&HostResource> {
        self.resources.get(&res_id)
    }

    pub fn scanout(&self, scanout_id: u32) -> Option<&ScanoutBinding> {
        self.scanouts.get(&scanout_id)
    }

    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    pub fn clear(&mut self) {
        self.resources.clear();
        self.scanouts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_blob_is_fail_closed() {
        let mut t = ResourceTable::new();
        assert!(t.create_blob(1, 1, 4096).is_ok());
        // Duplicate id.
        assert_eq!(t.create_blob(1, 1, 4096), Err(ResourceError::Duplicate(1)));
        // Zero / oversized size.
        assert!(matches!(t.create_blob(2, 1, 0), Err(ResourceError::BadSize { .. })));
        assert!(matches!(
            t.create_blob(3, 1, MAX_BLOB_BYTES + 1),
            Err(ResourceError::BadSize { .. })
        ));
        // Exactly at the cap is allowed.
        assert!(t.create_blob(4, 1, MAX_BLOB_BYTES).is_ok());
        assert_eq!(t.resource_count(), 2);
    }

    #[test]
    fn table_full_is_rejected() {
        let mut t = ResourceTable::new();
        for i in 0..MAX_RESOURCES as u32 {
            t.create_blob(i, 1, 64).unwrap();
        }
        assert_eq!(t.create_blob(9999, 1, 64), Err(ResourceError::TableFull));
    }

    #[test]
    fn attach_backing_must_cover_the_blob() {
        let mut t = ResourceTable::new();
        t.create_blob(1, 1, 8192).unwrap();
        // Backing on an unknown resource.
        assert_eq!(
            t.attach_backing(2, &[BackingSegment { addr: 0x1000, len: 8192 }]),
            Err(ResourceError::Unknown(2))
        );
        // Segments that don't cover the size are rejected.
        assert_eq!(
            t.attach_backing(1, &[BackingSegment { addr: 0x1000, len: 4096 }]),
            Err(ResourceError::BackingTooSmall)
        );
        // Enough coverage (possibly split) is accepted.
        assert!(t
            .attach_backing(
                1,
                &[
                    BackingSegment { addr: 0x1000, len: 4096 },
                    BackingSegment { addr: 0x9000, len: 4096 },
                ],
            )
            .is_ok());
        assert_eq!(t.get(1).unwrap().backing.len(), 2);
        // A hostile length sum that overflows u64 is rejected, not wrapped.
        assert_eq!(
            t.attach_backing(
                1,
                &[
                    BackingSegment { addr: 0, len: u64::MAX },
                    BackingSegment { addr: 0, len: 8192 },
                ],
            ),
            Err(ResourceError::BackingTooSmall)
        );
    }

    #[test]
    fn set_scanout_validates_dims_and_stride() {
        let mut t = ResourceTable::new();
        t.create_blob(1, 1, 1920 * 1080 * 4).unwrap();
        // Unknown resource.
        assert_eq!(t.set_scanout(0, 2, 1920, 1080, 1, 1920 * 4), Err(ResourceError::Unknown(2)));
        // Zero / oversized dims.
        assert_eq!(t.set_scanout(0, 1, 0, 1080, 1, 0), Err(ResourceError::BadDims));
        assert_eq!(t.set_scanout(0, 1, MAX_DIM + 1, 1080, 1, (MAX_DIM as u64 * 4) as u32), Err(ResourceError::BadDims));
        // Stride too small for a row.
        assert_eq!(t.set_scanout(0, 1, 1920, 1080, 1, 1920 * 4 - 1), Err(ResourceError::BadDims));
        // Well-formed binding.
        assert!(t.set_scanout(0, 1, 1920, 1080, 1, 1920 * 4).is_ok());
        assert_eq!(t.scanout(0).unwrap().res_id, 1);
    }

    #[test]
    fn destroy_drops_resource_and_its_scanout() {
        let mut t = ResourceTable::new();
        t.create_blob(1, 1, 4096).unwrap();
        t.set_scanout(0, 1, 32, 32, 1, 128).unwrap();
        assert!(t.scanout(0).is_some());
        assert!(t.destroy(1).is_ok());
        assert!(t.get(1).is_none());
        assert!(t.scanout(0).is_none(), "scanout bound to a destroyed resource is dropped");
        assert_eq!(t.destroy(1), Err(ResourceError::Unknown(1)));
    }
}
