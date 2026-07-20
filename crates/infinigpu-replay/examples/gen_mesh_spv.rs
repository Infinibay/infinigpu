//! Emit a C header with the VBO-reading mesh shaders (SPIR-V) for the guest ICD's mesh-validation
//! app (`guest/icd/infinigpu_mesh_test.c`). Two separate modules, each entry point `main`:
//!   - vertex: reads `pos: vec2 @location(0)` + `color: vec3 @location(1)` from the vertex buffer,
//!   - fragment: emits the interpolated colour.
//! These are the exact shaders the host replay's `forwarded_vbo_triangle_renders_mesh_colors` test
//! renders on the A5000, so the guest app drives the identical mesh path end-to-end.
//!
//! Regenerate:  cargo run -p infinigpu-replay --example gen_mesh_spv > guest/icd/infinigpu_mesh_spv.h

const MESH_VS: &str = r#"
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec3<f32> };
@vertex
fn main(@location(0) p: vec2<f32>, @location(1) c: vec3<f32>) -> VOut {
    return VOut(vec4<f32>(p, 0.0, 1.0), c);
}
"#;

const MESH_FS: &str = r#"
@fragment
fn main(@location(0) c: vec3<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(c, 1.0);
}
"#;

fn compile_wgsl(src: &str, stage: naga::ShaderStage) -> Vec<u32> {
    let module = naga::front::wgsl::parse_str(src).expect("wgsl parse");
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::IMMEDIATES,
    )
    .validate(&module)
    .expect("wgsl validate");
    let pipe = naga::back::spv::PipelineOptions {
        shader_stage: stage,
        entry_point: "main".to_string(),
    };
    naga::back::spv::write_vec(&module, &info, &naga::back::spv::Options::default(), Some(&pipe))
        .expect("spv emit")
}

fn emit_array(name: &str, words: &[u32]) {
    println!("static const uint32_t {}[{}] = {{", name, words.len());
    for chunk in words.chunks(8) {
        print!("   ");
        for w in chunk {
            print!(" 0x{:08x},", w);
        }
        println!();
    }
    println!("}};");
}

fn main() {
    let vs = compile_wgsl(MESH_VS, naga::ShaderStage::Vertex);
    let fs = compile_wgsl(MESH_FS, naga::ShaderStage::Fragment);

    println!("/* SPDX-License-Identifier: MIT");
    println!(" *");
    println!(" * GENERATED — do not edit. Regenerate:");
    println!(" *   cargo run -p infinigpu-replay --example gen_mesh_spv > guest/icd/infinigpu_mesh_spv.h");
    println!(" *");
    println!(" * VBO-reading mesh shaders for infinigpu_mesh_test.c. Two modules, each entry `main`:");
    println!(" *   vs: pos(vec2 @loc0) + color(vec3 @loc1) -> position + colour");
    println!(" *   fs: interpolated colour. Same source as replay's forwarded_vbo_triangle test. */");
    println!("#ifndef INFINIGPU_MESH_SPV_H");
    println!("#define INFINIGPU_MESH_SPV_H");
    println!("#include <stdint.h>");
    emit_array("infinigpu_mesh_vs_spv", &vs);
    emit_array("infinigpu_mesh_fs_spv", &fs);
    println!("#endif /* INFINIGPU_MESH_SPV_H */");
}
