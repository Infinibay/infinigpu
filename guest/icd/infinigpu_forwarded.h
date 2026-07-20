/* SPDX-License-Identifier: MIT
 *
 * Guest-side encoder for a vk_op::FORWARDED SUBMIT_CMD payload — the wire the infinigpu Vulkan ICD
 * emits from a recorded draw (docs/adr/GUEST-ICD-IMPLEMENTATION.md, Phase 1). The host decodes it
 * with infinigpu_device::decode_forwarded and replays it on the physical GPU. This is the single
 * source of the serialization: the ICD's driver_submit calls it, and the off-VM interop test
 * (crates/infinigpu-guest-conformance) drives it through the tested Rust decoder to prove the C↔Rust
 * wire agrees byte-for-byte — no guest VM needed.
 */
#ifndef INFINIGPU_FORWARDED_H
#define INFINIGPU_FORWARDED_H

#include <stddef.h>
#include <stdint.h>

/* Mirror of infinigpu_abi::wire::vk_op::FORWARDED / FORWARDED_CMDLIST. */
#define INFINIGPU_VK_OP_FORWARDED 2u
#define INFINIGPU_VK_OP_FORWARDED_CMDLIST 3u

/* Mirror of infinigpu_abi::wire::vk_topology. */
#define INFINIGPU_VK_TOPOLOGY_TRIANGLE_LIST 0u
#define INFINIGPU_VK_TOPOLOGY_TRIANGLE_STRIP 1u

/* Mirror of infinigpu_abi::wire::index_type. */
#define INFINIGPU_INDEX_TYPE_U16 0u
#define INFINIGPU_INDEX_TYPE_U32 1u

/* Mirror of infinigpu_abi::wire::depth_flags (Phase-2d — the ForwardedCmdListTail.depth_flags
 * bitfield: TEST | WRITE | (depth_compare << COMPARE_SHIFT)). 0 = no depth buffer. */
#define INFINIGPU_DEPTH_TEST 0x1u
#define INFINIGPU_DEPTH_WRITE 0x2u
#define INFINIGPU_DEPTH_COMPARE_SHIFT 4u
/* Mirror of infinigpu_abi::wire::depth_compare. */
#define INFINIGPU_DEPTH_CMP_NEVER 0u
#define INFINIGPU_DEPTH_CMP_LESS 1u
#define INFINIGPU_DEPTH_CMP_EQUAL 2u
#define INFINIGPU_DEPTH_CMP_LESS_OR_EQUAL 3u
#define INFINIGPU_DEPTH_CMP_GREATER 4u
#define INFINIGPU_DEPTH_CMP_NOT_EQUAL 5u
#define INFINIGPU_DEPTH_CMP_GREATER_OR_EQUAL 6u
#define INFINIGPU_DEPTH_CMP_ALWAYS 7u

/* Mirror of infinigpu_abi::wire::vk_vformat (vertex-attribute formats). */
#define INFINIGPU_VFORMAT_R32_SFLOAT 0u
#define INFINIGPU_VFORMAT_R32G32_SFLOAT 1u
#define INFINIGPU_VFORMAT_R32G32B32_SFLOAT 2u
#define INFINIGPU_VFORMAT_R32G32B32A32_SFLOAT 3u
#define INFINIGPU_VFORMAT_R8G8B8A8_UNORM 4u
#define INFINIGPU_VFORMAT_R32_UINT 5u

/* Mirror of infinigpu_abi::wire::sampler_flags (Phase-2c — TextureDescWire.sampler_flags). */
#define INFINIGPU_SAMPLER_LINEAR 0x1u
#define INFINIGPU_SAMPLER_REPEAT 0x2u

/* Wire structs whose bytes this encoder copies (defined in the generated infinigpu_abi.h). Only
 * pointers appear in the prototype, so a forward declaration keeps this header light. */
struct VertexAttrWire;
struct DrawCmdWire;
struct TextureDescWire;

/*
 * Serialize a forwarded draw into `out` (capacity `cap` bytes). Layout (matches ForwardedDrawTail):
 *   VulkanWorkload{op=FORWARDED, width, height, bg, scanout_addr}
 *   ForwardedDrawTail{vertex_count, topology, {vertex,fragment}_spirv_len, {vertex,fragment}_entry_len}
 *   vertex SPIR-V (vspirv_words*4 bytes) | fragment SPIR-V | vertex_entry\0 | fragment_entry\0
 * SPIR-V lengths are given in 32-bit WORDS. Returns the total byte length written, or 0 if the
 * payload would not fit `cap`. The caller wraps the result in a SUBMIT_CMD (encoding VULKAN_VENUSLIKE).
 */
size_t infinigpu_encode_forwarded(uint8_t *out, size_t cap,
                                  uint32_t width, uint32_t height,
                                  const float bg[4], uint64_t scanout_addr,
                                  uint32_t vertex_count, uint32_t topology,
                                  const uint32_t *vspirv, uint32_t vspirv_words,
                                  const uint32_t *fspirv, uint32_t fspirv_words,
                                  const char *vertex_entry, const char *fragment_entry);

/*
 * Serialize a Phase-2b forwarded COMMAND LIST (a real mesh) into `out` (capacity `cap`). Layout
 * (matches ForwardedCmdListTail + the host's decode_forwarded_cmdlist section order):
 *   VulkanWorkload{op=FORWARDED_CMDLIST, width, height, bg, scanout_addr}
 *   ForwardedCmdListTail{ spirv/entry lens, vertex_stride, attr_count, {vertex,index}_data_len,
 *                         index_type, draw_count, topology, depth_flags, push_const_len, tex_count }
 *   attrs[attr_count] | draws[draw_count] | texdescs[tex_count] | vertex SPIR-V | fragment SPIR-V |
 *   vertex data | index data | vertex_entry\0 | fragment_entry\0 | push constants | texture pixels
 * SPIR-V lengths are 32-bit WORDS; data lengths are BYTES. `attrs`/`draws`/`texs` are arrays of the
 * wire structs (the caller fills them from the recorded pipeline layout + vkCmdDraw* stream). The
 * texdescs sit in the fixed-array region after the draws; their RGBA8 pixels are the trailing region
 * (`texpix`). The UBO bytes (`ubo`, `ubo_len`) are a fixed-length blob after the push constants and
 * before `texpix`; the host uploads them into a UNIFORM_BUFFER at descriptor-set-0 binding `ubo_binding`
 * (VERTEX|FRAGMENT). `tex_binding` is the sampled image's binding (sampler at `tex_binding+1`); it lets
 * a UBO and a texture share set 0 at distinct bindings (e.g. UBO@0, image@1, sampler@2). Full section
 * order: attrs · draws · texdescs · vSPIR-V · fSPIR-V · vertex data · index data · vertex entry ·
 * fragment entry · push constants · UBO bytes · texture pixels. `index_data_len == 0` ⇒ non-indexed;
 * `tex_count == 0` ⇒ untextured; `ubo_len == 0` ⇒ no UBO. Returns the total byte length, or 0 if it
 * would not fit `cap` (or the geometry is degenerate). The caller wraps the result in a SUBMIT_CMD
 * (encoding VULKAN_VENUSLIKE), the same as the bufferless encoder.
 */
size_t infinigpu_encode_forwarded_cmdlist(
    uint8_t *out, size_t cap,
    uint32_t width, uint32_t height, const float bg[4], uint64_t scanout_addr,
    const uint32_t *vspirv, uint32_t vspirv_words,
    const uint32_t *fspirv, uint32_t fspirv_words,
    const char *vertex_entry, const char *fragment_entry,
    uint32_t vertex_stride,
    const struct VertexAttrWire *attrs, uint32_t attr_count,
    const uint8_t *vertex_data, uint32_t vertex_data_len,
    const uint8_t *index_data, uint32_t index_data_len, uint32_t index_type,
    uint32_t topology, uint32_t depth_flags,
    const uint8_t *push_const, uint32_t push_const_len,
    const uint8_t *ubo, uint32_t ubo_len, uint32_t ubo_binding,
    const struct DrawCmdWire *draws, uint32_t draw_count,
    const struct TextureDescWire *texs, uint32_t tex_count, uint32_t tex_binding,
    const uint8_t *texpix, uint32_t texpix_len);

#endif /* INFINIGPU_FORWARDED_H */
