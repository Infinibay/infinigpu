//! FFI declarations for the guest PR4 ring-producer C reference (`csrc/guest_ring_ref.c`), plus
//! thin safe wrappers. The C reference is the exact wire-protocol logic the Linux `.ko`
//! (`guest/linux/infinigpu.c`) uses to drive a real DMA-resident ring; the value of this crate is
//! `tests/interop.rs`, which builds a ring + `RESOURCE_*` stream with these functions and drains it
//! with the *tested* Rust device consumer (`infinigpu_device::drain` + `dispatch`), proving the
//! guest↔device PR4 protocol interoperates entirely off-hardware.
//!
//! The wrappers take raw base pointers + byte offsets (a single caller-owned buffer stands in for
//! guest RAM) and are `unsafe` for the same reason the device's `host_ptr` path is: the caller must
//! guarantee the offsets stay in bounds of the backing allocation.

unsafe extern "C" {
    fn igpu_gref_push(
        idx_base: *mut u8,
        desc_base: *mut u8,
        cap: u32,
        msg_type: u32,
        data_offset: u32,
        len: u32,
    ) -> u64;
    fn igpu_gref_retired(idx_base: *const u8) -> u64;
    fn igpu_gref_create_blob(buf: *mut u8, res_id: u32, size: u64) -> u32;
    fn igpu_gref_attach_backing(buf: *mut u8, res_id: u32, addr: u64, len: u64) -> u32;
    fn igpu_gref_set_scanout(
        buf: *mut u8,
        scanout_id: u32,
        res_id: u32,
        w: u32,
        h: u32,
        fmt: u32,
        stride: u32,
    ) -> u32;
    fn igpu_gref_flush(buf: *mut u8, res_id: u32, x: u32, y: u32, w: u32, h: u32) -> u32;
}

/// Push one descriptor through the C SPSC producer. Returns the assigned seqno (0 if the ring is
/// full). `idx_base`/`desc_base` are the index page and descriptor array; `cap` is the (pow2) slot
/// count. `data_offset` is the payload offset relative to `desc_base` (the device's convention).
///
/// # Safety
/// `idx_base` must point to ≥64 bytes and `desc_base` to ≥ `cap * 32` bytes, both writable.
pub unsafe fn push(
    idx_base: *mut u8,
    desc_base: *mut u8,
    cap: u32,
    msg_type: u32,
    data_offset: u32,
    len: u32,
) -> u64 {
    unsafe { igpu_gref_push(idx_base, desc_base, cap, msg_type, data_offset, len) }
}

/// Read the host-published highest retired seqno from the index page.
///
/// # Safety
/// `idx_base` must point to ≥64 readable bytes laid out as the index page.
pub unsafe fn retired(idx_base: *const u8) -> u64 {
    unsafe { igpu_gref_retired(idx_base) }
}

/// Build a `RESOURCE_CREATE_BLOB` body at `buf`, returning its length.
///
/// # Safety
/// `buf` must have room for the body (24 bytes).
pub unsafe fn create_blob(buf: *mut u8, res_id: u32, size: u64) -> u32 {
    unsafe { igpu_gref_create_blob(buf, res_id, size) }
}

/// Build a single-segment `RESOURCE_ATTACH_BACKING` body (header + one `MemEntry`).
///
/// # Safety
/// `buf` must have room for 24 bytes.
pub unsafe fn attach_backing(buf: *mut u8, res_id: u32, addr: u64, len: u64) -> u32 {
    unsafe { igpu_gref_attach_backing(buf, res_id, addr, len) }
}

/// Build a `SET_SCANOUT_BLOB` body.
///
/// # Safety
/// `buf` must have room for 24 bytes.
#[allow(clippy::too_many_arguments)]
pub unsafe fn set_scanout(
    buf: *mut u8,
    scanout_id: u32,
    res_id: u32,
    w: u32,
    h: u32,
    fmt: u32,
    stride: u32,
) -> u32 {
    unsafe { igpu_gref_set_scanout(buf, scanout_id, res_id, w, h, fmt, stride) }
}

/// Build a `RESOURCE_FLUSH` body.
///
/// # Safety
/// `buf` must have room for 24 bytes.
pub unsafe fn flush(buf: *mut u8, res_id: u32, x: u32, y: u32, w: u32, h: u32) -> u32 {
    unsafe { igpu_gref_flush(buf, res_id, x, y, w, h) }
}
