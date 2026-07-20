//! Emit a C header with the TEXTURED shaders (SPIR-V) for the guest ICD's texture-validation app
//! (`guest/icd/infinigpu_tex_test.c`). Two modules, each entry `main`:
//!   - vertex: pos vec2 @location(0) + uv vec2 @location(1) -> position + uv,
//!   - fragment: `textureSample` of a `texture_2d`(set0/binding0) + `sampler`(set0/binding1).
//! These are byte-identical to the shaders the host replay's `forwarded_texture_samples_onto_a_quad`
//! test renders on the A5000, so the guest app drives the identical texture path end-to-end.
//!
//! Regenerate:  cargo run -p infinigpu-replay --example gen_tex_spv > guest/icd/infinigpu_tex_spv.h

const TEX_VS: &str = "struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };\n\
    @vertex fn main(@location(0) p: vec2<f32>, @location(1) uv: vec2<f32>) -> VOut {\n\
      return VOut(vec4<f32>(p, 0.0, 1.0), uv);\n}";

const TEX_FS: &str = "@group(0) @binding(0) var tex: texture_2d<f32>;\n\
    @group(0) @binding(1) var samp: sampler;\n\
    @fragment fn main(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {\n\
      return textureSample(tex, samp, uv);\n}";

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
    let vs = compile_wgsl(TEX_VS, naga::ShaderStage::Vertex);
    let fs = compile_wgsl(TEX_FS, naga::ShaderStage::Fragment);

    println!("/* SPDX-License-Identifier: MIT");
    println!(" *");
    println!(" * GENERATED — do not edit. Regenerate:");
    println!(" *   cargo run -p infinigpu-replay --example gen_tex_spv > guest/icd/infinigpu_tex_spv.h");
    println!(" *");
    println!(" * Textured shaders for infinigpu_tex_test.c. Two modules, each entry `main`:");
    println!(" *   vs: pos(vec2 @loc0) + uv(vec2 @loc1) -> position + uv");
    println!(" *   fs: textureSample(texture_2d @set0/b0, sampler @set0/b1, uv)");
    println!(" * Same source as replay's forwarded_texture_samples_onto_a_quad test. */");
    println!("#ifndef INFINIGPU_TEX_SPV_H");
    println!("#define INFINIGPU_TEX_SPV_H");
    println!("#include <stdint.h>");
    emit_array("infinigpu_tex_vs_spv", &vs);
    emit_array("infinigpu_tex_fs_spv", &fs);
    println!("#endif /* INFINIGPU_TEX_SPV_H */");
}
