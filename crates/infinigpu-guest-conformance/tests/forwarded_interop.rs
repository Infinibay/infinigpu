//! Cross-language forwarded-draw interop (Phase 1c): the **guest** serializes a `vk_op::FORWARDED`
//! payload with the C encoder the Mesa ICD's `driver_submit` uses
//! (`guest/icd/infinigpu_forwarded.c`); the **host** decodes it with the *tested*
//! `infinigpu_device::decode_forwarded`. If the guest's byte layout, field order, or length
//! encoding disagreed with the host by a single byte, the decode would drop it or return the wrong
//! fields. Proving it here verifies the guest half of the forwarded wire off-hardware — no guest VM,
//! before a line of the full Mesa ICD is written. Mirrors the PR4 ring interop (`interop.rs`).

use infinigpu_device::decode_forwarded;
use infinigpu_guest_conformance as guest;

const CAP: usize = 64 * 1024 * 1024;
const SPIRV_MAGIC: u32 = 0x0723_0203;

#[test]
fn c_encoder_bytes_decode_through_the_host_decoder() {
    // Distinct vertex/fragment blobs (different content AND length) + distinct entries + a
    // non-default topology/vertex_count, so any slot swap or field mis-placement surfaces.
    let vspirv: [u32; 3] = [SPIRV_MAGIC, 0x1111_1111, 0x2222_2222];
    let fspirv: [u32; 5] = [SPIRV_MAGIC, 0xA000, 0xB000, 0xC000, 0xD000];
    let payload = guest::encode_forwarded(
        320,
        200,
        [0.1, 0.2, 0.3, 1.0],
        0xDEAD_BEEF_0000,
        5,
        1, // vk_topology::TRIANGLE_STRIP
        &vspirv,
        &fspirv,
        c"vs_entry",
        c"fs_entry_longer",
    );

    let owned = decode_forwarded(&payload, CAP).expect("C-encoded forwarded payload must decode");
    assert_eq!(owned.vertex_spirv, vspirv, "vertex SPIR-V survives the C→Rust round-trip");
    assert_eq!(owned.fragment_spirv, fspirv, "fragment SPIR-V survives (not swapped with vertex)");
    assert_eq!(owned.vertex_entry.as_c_str(), c"vs_entry");
    assert_eq!(owned.fragment_entry.as_c_str(), c"fs_entry_longer");
    assert_eq!(owned.vertex_count, 5);
    assert_eq!(owned.topology, 1);
}

#[test]
fn c_encoder_length_prefix_is_exact() {
    // The encoder must return exactly the header + tail + both blobs + both NUL-terminated names,
    // with nothing trailing — the host relies on the aggregate-length check to reject short input,
    // so an over-long payload from the guest would carry unvalidated tail bytes.
    let vspirv: [u32; 2] = [SPIRV_MAGIC, 0x1234];
    let fspirv: [u32; 2] = [SPIRV_MAGIC, 0x5678];
    let payload = guest::encode_forwarded(8, 8, [0.0; 4], 0, 3, 0, &vspirv, &fspirv, c"a", c"bb");
    // 40 (VulkanWorkload) + 24 (ForwardedDrawTail) + 8 + 8 + 2 ("a\0") + 3 ("bb\0") = 85.
    assert_eq!(payload.len(), 40 + 24 + 8 + 8 + 2 + 3);
    assert!(decode_forwarded(&payload, CAP).is_some(), "exact-length payload decodes");
}
