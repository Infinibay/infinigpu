//! Cross-language forwarded-draw interop (Phase 1c): the **guest** serializes a `vk_op::FORWARDED`
//! payload with the C encoder the Mesa ICD's `driver_submit` uses
//! (`guest/icd/infinigpu_forwarded.c`); the **host** decodes it with the *tested*
//! `infinigpu_device::decode_forwarded`. If the guest's byte layout, field order, or length
//! encoding disagreed with the host by a single byte, the decode would drop it or return the wrong
//! fields. Proving it here verifies the guest half of the forwarded wire off-hardware — no guest VM,
//! before a line of the full Mesa ICD is written. Mirrors the PR4 ring interop (`interop.rs`).

use infinigpu_device::{decode_forwarded, decode_forwarded_cmdlist};
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

/// Phase-2b: the **guest** serializes a real mesh (`vk_op::FORWARDED_CMDLIST`) with the C encoder the
/// Mesa ICD's `driver_submit` uses (`infinigpu_encode_forwarded_cmdlist`); the **host** decodes it
/// with the tested `decode_forwarded_cmdlist`. Proves the command-list wire — the vertex-input
/// attributes, the multi-draw array with per-draw viewport + vertex_offset, the vertex/index buffers,
/// and both SPIR-V blobs — agrees byte-for-byte across C↔Rust, off-hardware, before the Mesa ICD
/// recording is wired up. A single-byte disagreement in field order or section placement would drop
/// the decode or scramble a field.
#[test]
fn c_cmdlist_encoder_decodes_through_the_host_decoder() {
    use infinigpu_abi::wire::{vk_vformat, DrawCmdWire, VertexAttrWire};

    let vspirv: [u32; 4] = [SPIRV_MAGIC, 0x1111_1111, 0x2222_2222, 0x3333_3333];
    let fspirv: [u32; 3] = [SPIRV_MAGIC, 0xAAAA_AAAA, 0xBBBB_BBBB];
    let attrs = [
        VertexAttrWire { location: 0, format: vk_vformat::R32G32_SFLOAT, offset: 0 },
        VertexAttrWire { location: 1, format: vk_vformat::R32G32B32_SFLOAT, offset: 8 },
    ];
    // 3 vertices × stride 20; a u16 index buffer with a non-trivial pattern.
    let vertex_data: Vec<u8> = (0u8..60).collect();
    let indices: [u16; 3] = [2, 0, 1];
    let index_data: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
    // Two draws with distinct fields so a swap/mis-placement surfaces (per-draw viewport + offset).
    let draws = [
        DrawCmdWire { count: 3, instance_count: 1, first: 0, vertex_offset: 0, vp_x: 0.0, vp_y: 0.0, vp_w: 0.0, vp_h: 0.0 },
        DrawCmdWire { count: 3, instance_count: 4, first: 1, vertex_offset: -2, vp_x: 5.0, vp_y: 6.0, vp_w: 7.0, vp_h: 8.0 },
    ];

    // Depth: TEST | WRITE | (LESS_OR_EQUAL << COMPARE_SHIFT) — exercises the Phase-2d bitfield too.
    use infinigpu_abi::wire::{depth_compare, depth_flags, sampler_flags, TextureDescWire};
    let df = depth_flags::pack(true, true, depth_compare::LESS_OR_EQUAL);
    // A 2×2 RGBA8 texture with nearest+clamp sampling (exercises the Phase-2c texdesc + pixel region).
    let texpix: Vec<u8> = vec![
        10, 20, 30, 40, /**/ 50, 60, 70, 80, /**/ 90, 100, 110, 120, /**/ 130, 140, 150, 160,
    ];
    let texs = [TextureDescWire {
        width: 2,
        height: 2,
        data_len: texpix.len() as u32,
        sampler_flags: sampler_flags::LINEAR, // linear filtering, clamp addressing
    }];
    // A UBO at binding 0 composing with the texture at image@1 / sampler@2 (tex_binding=1) — exercises
    // the Phase-2c UBO byte blob (after push-const, before texpix) + the binding-composition fields.
    let ubo: Vec<u8> = (100u8..140).collect(); // 40 recognizable bytes
    let payload = guest::encode_forwarded_cmdlist(
        640,
        480,
        [0.1, 0.2, 0.3, 1.0],
        0xCAFE_0000,
        &vspirv,
        &fspirv,
        c"vmain",
        c"fmain",
        20,
        &attrs,
        &vertex_data,
        &index_data,
        false,
        1, // vk_topology::TRIANGLE_STRIP
        df,
        &[9u8, 8, 7, 6, 5, 4, 3, 2], // push-constant bytes (a stand-in transform block)
        &draws,
        &texs,
        &texpix,
        &ubo,
        0, // ubo_binding
        1, // tex_binding (image@1, sampler@2)
        infinigpu_abi::wire::raster_flags::pack(infinigpu_abi::wire::cull_mode::BACK, true, true),
    );

    let o = decode_forwarded_cmdlist(&payload, CAP).expect("C-encoded cmdlist must decode");
    assert_eq!(o.vertex_spirv, vspirv, "vertex SPIR-V survives C→Rust");
    assert_eq!(o.fragment_spirv, fspirv, "fragment SPIR-V survives (not swapped)");
    assert_eq!(o.vertex_entry.as_c_str(), c"vmain");
    assert_eq!(o.fragment_entry.as_c_str(), c"fmain");
    assert_eq!(o.topology, 1);
    let g = o.geometry.expect("cmdlist carries geometry");
    assert_eq!(g.vertex_stride, 20);
    assert_eq!(g.vertex_data, vertex_data, "vertex buffer round-trips");
    assert_eq!(g.index_data, index_data, "index buffer round-trips");
    assert!(!g.index_u32);
    assert_eq!(g.attrs.len(), 2);
    assert_eq!((g.attrs[0].location, g.attrs[0].format, g.attrs[0].offset), (0, vk_vformat::R32G32_SFLOAT, 0));
    assert_eq!((g.attrs[1].location, g.attrs[1].format, g.attrs[1].offset), (1, vk_vformat::R32G32B32_SFLOAT, 8));
    assert_eq!(g.draws.len(), 2, "multi-draw round-trips");
    assert_eq!(g.draws[1].instance_count, 4);
    assert_eq!(g.draws[1].first, 1);
    assert_eq!(g.draws[1].vertex_offset, -2, "signed vertex_offset survives");
    assert_eq!(g.draws[1].viewport, [5.0, 6.0, 7.0, 8.0], "per-draw viewport survives");
    let d = g.depth.expect("depth state decodes from depth_flags");
    assert!(d.test && d.write, "depth test+write survive the C→Rust round-trip");
    assert_eq!(d.compare, depth_compare::LESS_OR_EQUAL, "depth compare-op survives");
    assert_eq!(g.push_constants, [9, 8, 7, 6, 5, 4, 3, 2], "push-constant bytes survive C→Rust");
    let t = g.texture.expect("texdesc + pixel region decode to a texture");
    assert_eq!((t.width, t.height), (2, 2), "texture dims survive C→Rust");
    assert_eq!(t.rgba, texpix, "texture pixels survive C→Rust (trailing region placement)");
    assert!(t.linear && !t.repeat, "sampler flags survive (linear on, repeat off)");
    assert_eq!(g.tex_binding, 1, "tex_binding survives C→Rust (image@1 / sampler@2)");
    let u = g.uniform.expect("ubo bytes decode to a uniform");
    assert_eq!(u.binding, 0, "ubo binding survives C→Rust");
    assert_eq!(u.bytes, ubo, "ubo bytes survive C→Rust (blob after push-const, before texpix)");
    assert_eq!(
        g.raster_flags,
        infinigpu_abi::wire::raster_flags::pack(infinigpu_abi::wire::cull_mode::BACK, true, true),
        "raster_flags survives C→Rust (cull BACK / front-face CW / blend on)"
    );
}
