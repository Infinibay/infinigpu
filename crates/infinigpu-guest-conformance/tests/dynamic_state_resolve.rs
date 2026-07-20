//! Phase-2d-A5 dynamic-state (EDS1) resolve: the guest ICD folds the app's `vkCmdSet*` values into the
//! forwarded `raster_flags`/`depth_flags`/`topology` for the states a pipeline declared DYNAMIC. DXVK/
//! VKD3D (the realistic path to Windows games) drive cull/front-face/depth/topology this way and leave
//! the static pipeline fields at defaults, so a wrong resolve would forward the wrong (static) state and
//! silently break culling / depth / primitive assembly. This drives the exact C resolver the ICD's
//! `driver_submit` calls (`infinigpu_resolve_forwarded_state`) — pinning the mask-select + bit-repack +
//! the depth compare-only-when-test guard off-hardware, the one piece the GPU replay tests can't reach
//! (the resolve happens inside the ICD before the encoder).

use infinigpu_abi::wire::{cull_mode, depth_compare, depth_flags, raster_flags, vk_topology};
use infinigpu_guest_conformance::resolve_forwarded_state;

// INFINIGPU_DYN_* bits (mirror of guest/icd/infinigpu_forwarded.h — an ICD-internal concept, not wire).
const DYN_CULL: u32 = 1 << 0;
const DYN_FRONT: u32 = 1 << 1;
const DYN_DEPTH_TEST: u32 = 1 << 2;
const DYN_DEPTH_WRITE: u32 = 1 << 3;
const DYN_DEPTH_COMPARE: u32 = 1 << 4;
const DYN_TOPO: u32 = 1 << 5;

#[test]
fn nothing_dynamic_passes_static_through_unchanged() {
    let sr = raster_flags::pack(cull_mode::BACK, true, true);
    let sd = depth_flags::pack(true, true, depth_compare::LESS);
    let st = vk_topology::TRIANGLE_STRIP;
    // dynamic_mask = 0 ⇒ the app declared nothing dynamic ⇒ static stands verbatim, whatever set_mask.
    let (r, d, t) = resolve_forwarded_state(sr, sd, st, 0, 0xFF, 3, 0, 0, 0, 7, 0);
    assert_eq!((r, d, t), (sr, sd, st), "no dynamic state ⇒ the pipeline's static capture is forwarded as-is");
}

#[test]
fn declared_dynamic_but_not_set_keeps_static() {
    // A pipeline declares cull dynamic but the app never called vkCmdSetCullMode (set_mask bit clear):
    // the override must NOT fire (dynamic_mask & set_mask == 0), so the static cull stands.
    let sr = raster_flags::pack(cull_mode::FRONT, false, false);
    let (r, _, _) = resolve_forwarded_state(sr, 0, 0, DYN_CULL, 0, cull_mode::BACK, 0, 0, 0, 0, 0);
    assert_eq!(r, sr, "declared-dynamic but unset ⇒ fall back to static, never a stale/garbage value");
}

#[test]
fn dynamic_cull_overrides_only_cull_and_preserves_siblings() {
    // Static: cull NONE, front-face CW, blend ON. App dynamically sets cull BACK (front-face/blend NOT
    // dynamic). Only the cull sub-field must change; front-face CW + blend must survive the repack.
    let sr = raster_flags::pack(cull_mode::NONE, true, true);
    let (r, _, _) = resolve_forwarded_state(sr, 0, 0, DYN_CULL, DYN_CULL, cull_mode::BACK, 0, 0, 0, 0, 0);
    assert_eq!(raster_flags::cull(r), cull_mode::BACK, "dynamic cull applied");
    assert_ne!(r & raster_flags::FRONT_FACE_CW, 0, "static front-face-CW preserved through the repack");
    assert_ne!(r & raster_flags::BLEND, 0, "static blend preserved through the repack");
}

#[test]
fn dynamic_front_face_overrides_only_winding() {
    // Static front-face CW + cull BACK; app dynamically flips front-face to CCW (bit clear). Cull stays.
    let sr = raster_flags::pack(cull_mode::BACK, true, false);
    let (r, _, _) = resolve_forwarded_state(sr, 0, 0, DYN_FRONT, DYN_FRONT, 0, 0, 0, 0, 0, 0);
    assert_eq!(r & raster_flags::FRONT_FACE_CW, 0, "dynamic front-face CCW applied");
    assert_eq!(raster_flags::cull(r), cull_mode::BACK, "static cull BACK preserved");
}

#[test]
fn dynamic_depth_enables_a_buffer_the_static_pipeline_left_off() {
    // The realistic DXVK case: the pipeline declares depth test+compare dynamic, so its STATIC depth_flags
    // are 0 (no depth). The app enables depth at draw time. The resolve must produce a real depth_flags
    // (TEST | compare) — otherwise depth is silently dropped and the scene has no hidden-surface removal.
    let mask = DYN_DEPTH_TEST | DYN_DEPTH_WRITE | DYN_DEPTH_COMPARE;
    let (_, d, _) = resolve_forwarded_state(
        0, 0, 0, mask, mask, 0, 0, /*test*/ 1, /*write*/ 1, /*compare*/ depth_compare::LESS_OR_EQUAL, 0,
    );
    assert_ne!(d & depth_flags::TEST, 0, "dynamic depth test enabled a buffer the static pipeline lacked");
    assert_ne!(d & depth_flags::WRITE, 0, "dynamic depth write enabled");
    assert_eq!((d & depth_flags::COMPARE_MASK) >> depth_flags::COMPARE_SHIFT, depth_compare::LESS_OR_EQUAL);
}

#[test]
fn dynamic_compare_without_test_or_write_does_not_force_a_depth_buffer() {
    // A lone dynamic compare-op with test AND write off must yield depth_flags == 0 (no attachment) — the
    // host adds a depth buffer iff TEST|WRITE, so packing a bare compare would spuriously force one.
    let mask = DYN_DEPTH_TEST | DYN_DEPTH_WRITE | DYN_DEPTH_COMPARE;
    let (_, d, _) = resolve_forwarded_state(
        0, 0, 0, mask, mask, 0, 0, /*test*/ 0, /*write*/ 0, /*compare*/ depth_compare::GREATER, 0,
    );
    assert_eq!(d, 0, "compare with no test/write ⇒ no depth buffer (compare packed only when test|write)");
}

#[test]
fn dynamic_test_write_read_static_compare() {
    // The adversarial-review bug (fixed): a pipeline declares depth TEST+WRITE dynamic but leaves the
    // compareOp STATIC. Because test/write are dynamic, the pipeline's create-info enables are ignored
    // placeholders, so the captured static depth_flags carries ONLY the compareOp (test/write bits 0).
    // The app enables test+write at draw time (but not compare). The resolver must read the STATIC
    // compare (LESS) — reading 0 would forward compare=NEVER and cull every fragment (geometry vanishes).
    let static_depth = depth_flags::pack(false, false, depth_compare::LESS); // only the compareOp survived
    let mask = DYN_DEPTH_TEST | DYN_DEPTH_WRITE; // compare is NOT dynamic
    let (_, d, _) = resolve_forwarded_state(
        0, static_depth, 0, mask, mask, 0, 0, /*test*/ 1, /*write*/ 1, /*compare unused*/ 0, 0,
    );
    assert_ne!(d & depth_flags::TEST, 0, "dynamic test enabled");
    assert_ne!(d & depth_flags::WRITE, 0, "dynamic write enabled");
    assert_eq!(
        (d & depth_flags::COMPARE_MASK) >> depth_flags::COMPARE_SHIFT,
        depth_compare::LESS,
        "the static compareOp must survive a dynamic test/write enable (else compare=NEVER culls all)"
    );
}

#[test]
fn compare_only_static_depth_normalizes_to_no_depth() {
    // A depth-present-but-disabled pipeline: the capture now records a compareOp even with test/write off
    // (so a later dynamic enable can read it), so static_depth is a lone compare with nothing dynamic.
    // The resolver must forward 0 (no depth attachment) — byte-identical to a no-depth pipeline pre-EDS1.
    let static_depth = depth_flags::pack(false, false, depth_compare::GREATER);
    let (_, d, _) = resolve_forwarded_state(0, static_depth, 0, 0, 0, 0, 0, 0, 0, 0, 0);
    assert_eq!(d, 0, "a lone static compare-op (depth disabled) forwards as no-depth");
}

#[test]
fn dynamic_topology_overrides_wire_topo() {
    // Static list, app dynamically switches to strip.
    let (_, _, t) = resolve_forwarded_state(
        0, 0, vk_topology::TRIANGLE_LIST, DYN_TOPO, DYN_TOPO, 0, 0, 0, 0, 0, vk_topology::TRIANGLE_STRIP,
    );
    assert_eq!(t, vk_topology::TRIANGLE_STRIP, "dynamic topology applied");
}
