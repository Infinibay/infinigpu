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

/* Mirror of infinigpu_abi::wire::vk_op::FORWARDED. */
#define INFINIGPU_VK_OP_FORWARDED 2u

/* Mirror of infinigpu_abi::wire::vk_topology. */
#define INFINIGPU_VK_TOPOLOGY_TRIANGLE_LIST 0u
#define INFINIGPU_VK_TOPOLOGY_TRIANGLE_STRIP 1u

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

#endif /* INFINIGPU_FORWARDED_H */
