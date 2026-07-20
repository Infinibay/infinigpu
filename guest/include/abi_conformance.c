/* Cross-language ABI conformance: the C view (generated from infinigpu-abi by
 * cbindgen) must have the exact byte layout the Rust side asserts. Compile with:
 *   cc -std=c11 -I guest/include guest/include/abi_conformance.c -o /tmp/abiconf
 * A build failure here means the Rust ABI and the generated C header drifted.
 * Mirrors infiniservice's cross-language HMAC test. */
#include <stddef.h>
#include "infinigpu_abi.h"

_Static_assert(sizeof(struct Descriptor) == 32, "Descriptor size");
_Static_assert(offsetof(struct Descriptor, seqno) == 16, "Descriptor.seqno offset");

_Static_assert(sizeof(struct MsgHeader) == 8, "MsgHeader size");

_Static_assert(sizeof(struct SubmitCmd) == 40, "SubmitCmd size");
_Static_assert(offsetof(struct SubmitCmd, seqno) == 16, "SubmitCmd.seqno offset");
_Static_assert(offsetof(struct SubmitCmd, out_fence) == 32, "SubmitCmd.out_fence offset");

_Static_assert(sizeof(struct ClearPresent) == 32, "ClearPresent size");
_Static_assert(offsetof(struct ClearPresent, rgba) == 8, "ClearPresent.rgba offset");
_Static_assert(offsetof(struct ClearPresent, scanout_addr) == 24, "ClearPresent.scanout_addr offset");

_Static_assert(sizeof(struct ScanoutPresent) == 24, "ScanoutPresent size");
_Static_assert(offsetof(struct ScanoutPresent, scanout_addr) == 16, "ScanoutPresent.scanout_addr offset");

/* ScanoutPresentDamaged is a ScanoutPresent superset: same prefix + scanout_addr@16,
 * with a trailing damage rect (dx,dy,dw,dh). The shared prefix MUST stay byte-identical. */
_Static_assert(sizeof(struct ScanoutPresentDamaged) == 40, "ScanoutPresentDamaged size");
_Static_assert(offsetof(struct ScanoutPresentDamaged, scanout_addr) == 16, "ScanoutPresentDamaged.scanout_addr offset");
_Static_assert(offsetof(struct ScanoutPresentDamaged, dx) == 24, "ScanoutPresentDamaged.dx offset");
_Static_assert(offsetof(struct ScanoutPresentDamaged, dh) == 36, "ScanoutPresentDamaged.dh offset");

/* CursorUpdate: 48 bytes, padding-free — the cursor-plane sideband body (ABI 0.3). */
_Static_assert(sizeof(struct CursorUpdate) == 48, "CursorUpdate size");
_Static_assert(offsetof(struct CursorUpdate, pos_x) == 8, "CursorUpdate.pos_x offset");
_Static_assert(offsetof(struct CursorUpdate, hot_x) == 16, "CursorUpdate.hot_x offset");
_Static_assert(offsetof(struct CursorUpdate, pitch) == 24, "CursorUpdate.pitch offset");
_Static_assert(offsetof(struct CursorUpdate, format) == 28, "CursorUpdate.format offset");
_Static_assert(offsetof(struct CursorUpdate, shape_ref) == 32, "CursorUpdate.shape_ref offset");
_Static_assert(offsetof(struct CursorUpdate, _reserved) == 40, "CursorUpdate._reserved offset");

_Static_assert(sizeof(struct ResourceCreateBlob) == 24, "ResourceCreateBlob size");
_Static_assert(sizeof(struct SetScanoutBlob) == 24, "SetScanoutBlob size");
_Static_assert(sizeof(struct ResourceFlush) == 24, "ResourceFlush size");
/* AttachBacking header + MemEntry array — RESOURCE_ATTACH_BACKING payload (ABI 0.4). */
_Static_assert(sizeof(struct AttachBacking) == 8, "AttachBacking size");
_Static_assert(sizeof(struct MemEntry) == 16, "MemEntry size");
_Static_assert(offsetof(struct MemEntry, length) == 8, "MemEntry.length offset");

/* VulkanWorkload — VULKAN_VENUSLIKE submit payload, Phase-0 own-remoting 3D (ABI 0.5). */
_Static_assert(sizeof(struct VulkanWorkload) == 40, "VulkanWorkload size");
_Static_assert(offsetof(struct VulkanWorkload, bg) == 16, "VulkanWorkload.bg offset");
_Static_assert(offsetof(struct VulkanWorkload, scanout_addr) == 32, "VulkanWorkload.scanout_addr offset");

/* ForwardedDrawTail — Phase-1 own-ICD bufferless forwarded draw (ABI 0.6). */
_Static_assert(sizeof(struct ForwardedDrawTail) == 24, "ForwardedDrawTail size");

/* Phase-2b forwarded command list — real mesh (ABI 0.8; grew to 52 B in 0.9 with push_const_len,
 * to 56 B in 0.10 with tex_count, to 68 B in 0.11 with ubo_len/ubo_binding/tex_binding, to 72 B in
 * 0.12 with raster_flags, then to 80 B in 0.13 with ssbo_len/ssbo_binding). The encoder in
 * infinigpu_forwarded.c depends on these exact sizes so the trailing sections land where the host
 * decoder reads them. */
_Static_assert(sizeof(struct ForwardedCmdListTail) == 80, "ForwardedCmdListTail size");
_Static_assert(offsetof(struct ForwardedCmdListTail, vertex_stride) == 16, "ForwardedCmdListTail.vertex_stride offset");
_Static_assert(offsetof(struct ForwardedCmdListTail, draw_count) == 36, "ForwardedCmdListTail.draw_count offset");
_Static_assert(offsetof(struct ForwardedCmdListTail, push_const_len) == 48, "ForwardedCmdListTail.push_const_len offset");
_Static_assert(offsetof(struct ForwardedCmdListTail, tex_count) == 52, "ForwardedCmdListTail.tex_count offset");
_Static_assert(offsetof(struct ForwardedCmdListTail, ubo_len) == 56, "ForwardedCmdListTail.ubo_len offset");
_Static_assert(offsetof(struct ForwardedCmdListTail, ubo_binding) == 60, "ForwardedCmdListTail.ubo_binding offset");
_Static_assert(offsetof(struct ForwardedCmdListTail, tex_binding) == 64, "ForwardedCmdListTail.tex_binding offset");
_Static_assert(offsetof(struct ForwardedCmdListTail, raster_flags) == 68, "ForwardedCmdListTail.raster_flags offset");
_Static_assert(offsetof(struct ForwardedCmdListTail, ssbo_len) == 72, "ForwardedCmdListTail.ssbo_len offset");
_Static_assert(offsetof(struct ForwardedCmdListTail, ssbo_binding) == 76, "ForwardedCmdListTail.ssbo_binding offset");
_Static_assert(sizeof(struct VertexAttrWire) == 12, "VertexAttrWire size");
_Static_assert(sizeof(struct DrawCmdWire) == 32, "DrawCmdWire size");
_Static_assert(offsetof(struct DrawCmdWire, vp_x) == 16, "DrawCmdWire.vp_x offset");
_Static_assert(sizeof(struct TextureDescWire) == 16, "TextureDescWire size");
_Static_assert(offsetof(struct TextureDescWire, data_len) == 8, "TextureDescWire.data_len offset");
_Static_assert(offsetof(struct TextureDescWire, sampler_flags) == 12, "TextureDescWire.sampler_flags offset");

int main(void) { return 0; }
