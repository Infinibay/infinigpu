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

    // The forwarded-draw encoder shared with the Mesa ICD (guest/icd/infinigpu_forwarded.c).
    fn infinigpu_encode_forwarded(
        out: *mut u8,
        cap: usize,
        width: u32,
        height: u32,
        bg: *const f32,
        scanout_addr: u64,
        vertex_count: u32,
        topology: u32,
        vspirv: *const u32,
        vspirv_words: u32,
        fspirv: *const u32,
        fspirv_words: u32,
        vertex_entry: *const std::os::raw::c_char,
        fragment_entry: *const std::os::raw::c_char,
    ) -> usize;

    // The Phase-2b forwarded command-list encoder (a real mesh) — same source file, shared with the
    // Mesa ICD. attrs/draws are passed as raw bytes (repr(C) wire structs) to avoid re-declaring the
    // C structs in Rust; the C reads them as VertexAttrWire[]/DrawCmdWire[].
    #[allow(clippy::too_many_arguments)]
    fn infinigpu_encode_forwarded_cmdlist(
        out: *mut u8,
        cap: usize,
        width: u32,
        height: u32,
        bg: *const f32,
        scanout_addr: u64,
        vspirv: *const u32,
        vspirv_words: u32,
        fspirv: *const u32,
        fspirv_words: u32,
        vertex_entry: *const std::os::raw::c_char,
        fragment_entry: *const std::os::raw::c_char,
        vertex_stride: u32,
        attrs: *const u8,
        attr_count: u32,
        vertex_data: *const u8,
        vertex_data_len: u32,
        index_data: *const u8,
        index_data_len: u32,
        index_type: u32,
        topology: u32,
        depth_flags: u32,
        push_const: *const u8,
        push_const_len: u32,
        ubo: *const u8,
        ubo_len: u32,
        ubo_binding: u32,
        ssbo: *const u8,
        ssbo_len: u32,
        ssbo_binding: u32,
        draws: *const u8,
        draw_count: u32,
        texs: *const u8,
        tex_count: u32,
        tex_binding: u32,
        texpix: *const u8,
        texpix_len: u32,
        raster_flags: u32,
    ) -> usize;

    // The EDS1 static-vs-dynamic state resolver the ICD's driver_submit uses to fold the app's
    // vkCmdSet* values into the forwarded raster_flags/depth_flags/topology (guest/icd/infinigpu_forwarded.c).
    #[allow(clippy::too_many_arguments)]
    fn infinigpu_resolve_forwarded_state(
        static_raster: u32,
        static_depth: u32,
        static_topo: u32,
        dynamic_mask: u32,
        set_mask: u32,
        dyn_cull: u32,
        dyn_front_cw: u32,
        dyn_depth_test: u32,
        dyn_depth_write: u32,
        dyn_depth_compare: u32,
        dyn_topo: u32,
        out_raster: *mut u32,
        out_depth: *mut u32,
        out_topo: *mut u32,
    );
}

/// The resolved `(raster_flags, depth_flags, topology)` from a pipeline's static capture + a command
/// buffer's dynamic values (EDS1), computed by the exact C resolver the ICD's `driver_submit` uses.
/// `dynamic_mask`/`set_mask` are `INFINIGPU_DYN_*` bitfields; a state is overridden only where both are
/// set. Inputs are already wire-normalized (see the C prototype). Proves the resolve logic off-hardware.
#[allow(clippy::too_many_arguments)]
pub fn resolve_forwarded_state(
    static_raster: u32,
    static_depth: u32,
    static_topo: u32,
    dynamic_mask: u32,
    set_mask: u32,
    dyn_cull: u32,
    dyn_front_cw: u32,
    dyn_depth_test: u32,
    dyn_depth_write: u32,
    dyn_depth_compare: u32,
    dyn_topo: u32,
) -> (u32, u32, u32) {
    let (mut r, mut d, mut t) = (0u32, 0u32, 0u32);
    unsafe {
        infinigpu_resolve_forwarded_state(
            static_raster,
            static_depth,
            static_topo,
            dynamic_mask,
            set_mask,
            dyn_cull,
            dyn_front_cw,
            dyn_depth_test,
            dyn_depth_write,
            dyn_depth_compare,
            dyn_topo,
            &mut r,
            &mut d,
            &mut t,
        );
    }
    (r, d, t)
}

/// Build a `vk_op::FORWARDED_CMDLIST` SUBMIT_CMD payload (a real mesh) with the exact C encoder the
/// guest ICD's `driver_submit` uses — proving the Phase-2b guest↔host wire byte-for-byte. `attrs`
/// and `draws` are the wire structs from `infinigpu_abi`; SPIR-V slices are u32 words; index data
/// empty ⇒ non-indexed. Panics if the encoder rejects the geometry (degenerate / doesn't fit).
#[allow(clippy::too_many_arguments)]
pub fn encode_forwarded_cmdlist(
    width: u32,
    height: u32,
    bg: [f32; 4],
    scanout_addr: u64,
    vspirv: &[u32],
    fspirv: &[u32],
    vertex_entry: &std::ffi::CStr,
    fragment_entry: &std::ffi::CStr,
    vertex_stride: u32,
    attrs: &[infinigpu_abi::wire::VertexAttrWire],
    vertex_data: &[u8],
    index_data: &[u8],
    index_u32: bool,
    topology: u32,
    depth_flags: u32,
    push_const: &[u8],
    draws: &[infinigpu_abi::wire::DrawCmdWire],
    texs: &[infinigpu_abi::wire::TextureDescWire],
    texpix: &[u8],
    ubo: &[u8],
    ubo_binding: u32,
    ssbo: &[u8],
    ssbo_binding: u32,
    tex_binding: u32,
    raster_flags: u32,
) -> Vec<u8> {
    let cap = 128
        + vspirv.len() * 4
        + fspirv.len() * 4
        + attrs.len() * 12
        + draws.len() * 32
        + texs.len() * 16
        + vertex_data.len()
        + index_data.len()
        + push_const.len()
        + ubo.len()
        + ssbo.len()
        + texpix.len()
        + vertex_entry.to_bytes_with_nul().len()
        + fragment_entry.to_bytes_with_nul().len();
    let mut out = vec![0u8; cap];
    let n = unsafe {
        infinigpu_encode_forwarded_cmdlist(
            out.as_mut_ptr(),
            out.len(),
            width,
            height,
            bg.as_ptr(),
            scanout_addr,
            vspirv.as_ptr(),
            vspirv.len() as u32,
            fspirv.as_ptr(),
            fspirv.len() as u32,
            vertex_entry.as_ptr(),
            fragment_entry.as_ptr(),
            vertex_stride,
            attrs.as_ptr() as *const u8,
            attrs.len() as u32,
            vertex_data.as_ptr(),
            vertex_data.len() as u32,
            index_data.as_ptr(),
            index_data.len() as u32,
            if index_u32 { 1 } else { 0 },
            topology,
            depth_flags,
            push_const.as_ptr(),
            push_const.len() as u32,
            ubo.as_ptr(),
            ubo.len() as u32,
            ubo_binding,
            ssbo.as_ptr(),
            ssbo.len() as u32,
            ssbo_binding,
            draws.as_ptr() as *const u8,
            draws.len() as u32,
            texs.as_ptr() as *const u8,
            texs.len() as u32,
            tex_binding,
            texpix.as_ptr(),
            texpix.len() as u32,
            raster_flags,
        )
    };
    assert!(n > 0, "C cmdlist encoder returned 0 (degenerate geometry or did not fit)");
    out.truncate(n);
    out
}

/// Build a `vk_op::FORWARDED` SUBMIT_CMD payload with the C encoder the guest ICD uses — the exact
/// bytes `driver_submit` will emit. Returns the serialized payload (panics if the encoder rejects
/// the geometry). `topology` is a `vk_topology` value; SPIR-V slices are u32 words.
#[allow(clippy::too_many_arguments)]
pub fn encode_forwarded(
    width: u32,
    height: u32,
    bg: [f32; 4],
    scanout_addr: u64,
    vertex_count: u32,
    topology: u32,
    vspirv: &[u32],
    fspirv: &[u32],
    vertex_entry: &std::ffi::CStr,
    fragment_entry: &std::ffi::CStr,
) -> Vec<u8> {
    let cap = 64
        + vspirv.len() * 4
        + fspirv.len() * 4
        + vertex_entry.to_bytes_with_nul().len()
        + fragment_entry.to_bytes_with_nul().len()
        + 16;
    let mut out = vec![0u8; cap];
    let n = unsafe {
        infinigpu_encode_forwarded(
            out.as_mut_ptr(),
            out.len(),
            width,
            height,
            bg.as_ptr(),
            scanout_addr,
            vertex_count,
            topology,
            vspirv.as_ptr(),
            vspirv.len() as u32,
            fspirv.as_ptr(),
            fspirv.len() as u32,
            vertex_entry.as_ptr(),
            fragment_entry.as_ptr(),
        )
    };
    assert!(n > 0, "C encoder returned 0 (payload did not fit the buffer)");
    out.truncate(n);
    out
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
