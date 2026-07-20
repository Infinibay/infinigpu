/* SPDX-License-Identifier: MIT
 *
 * Implementation of the forwarded-draw wire encoder (see infinigpu_forwarded.h). Uses the generated
 * wire-ABI structs (guest/include/infinigpu_abi.h) so the on-wire byte layout is the one the host
 * decoder expects; the _Static_asserts pin it the same way abi_conformance.c does.
 */
#include "infinigpu_forwarded.h"

#include <string.h>

#include "infinigpu_abi.h"

/* The two structs whose layout this encoder depends on must match the Rust ABI (host decoder reads
 * VulkanWorkload at offset 0 and ForwardedDrawTail immediately after). cbindgen pins them; assert
 * the sizes/offsets here too so a drift is a compile error in the guest build. */
_Static_assert(sizeof(struct VulkanWorkload) == 40, "VulkanWorkload is 40 bytes");
_Static_assert(sizeof(struct ForwardedDrawTail) == 24, "ForwardedDrawTail is 24 bytes");
_Static_assert(offsetof(struct VulkanWorkload, scanout_addr) == 32, "scanout_addr@32");
/* Phase-2b command-list structs — the cmdlist encoder copies these; drift is a compile error.
 * The tail grew to 52 B in ABI 0.9 (push_const_len) and to 56 B in ABI 0.10 (tex_count). */
_Static_assert(sizeof(struct ForwardedCmdListTail) == 56, "ForwardedCmdListTail is 56 bytes");
_Static_assert(sizeof(struct VertexAttrWire) == 12, "VertexAttrWire is 12 bytes");
_Static_assert(sizeof(struct DrawCmdWire) == 32, "DrawCmdWire is 32 bytes");
_Static_assert(sizeof(struct TextureDescWire) == 16, "TextureDescWire is 16 bytes");

size_t infinigpu_encode_forwarded(uint8_t *out, size_t cap,
                                  uint32_t width, uint32_t height,
                                  const float bg[4], uint64_t scanout_addr,
                                  uint32_t vertex_count, uint32_t topology,
                                  const uint32_t *vspirv, uint32_t vspirv_words,
                                  const uint32_t *fspirv, uint32_t fspirv_words,
                                  const char *vertex_entry, const char *fragment_entry)
{
	const size_t vbytes = (size_t)vspirv_words * 4u;
	const size_t fbytes = (size_t)fspirv_words * 4u;
	const size_t velen = strlen(vertex_entry) + 1u;   /* incl. NUL */
	const size_t felen = strlen(fragment_entry) + 1u;

	const size_t total = sizeof(struct VulkanWorkload) + sizeof(struct ForwardedDrawTail) +
	                     vbytes + fbytes + velen + felen;
	if (out == NULL || total > cap)
		return 0;

	size_t o = 0;

	struct VulkanWorkload wl;
	memset(&wl, 0, sizeof wl);
	wl.op = INFINIGPU_VK_OP_FORWARDED;
	wl.width = width;
	wl.height = height;
	memcpy(wl.bg, bg, sizeof wl.bg); /* float[4] clear/background colour */
	wl.scanout_addr = scanout_addr;
	memcpy(out + o, &wl, sizeof wl);
	o += sizeof wl;

	struct ForwardedDrawTail tail;
	memset(&tail, 0, sizeof tail);
	tail.vertex_count = vertex_count;
	tail.topology = topology;
	tail.vertex_spirv_len = (uint32_t)vbytes;
	tail.fragment_spirv_len = (uint32_t)fbytes;
	tail.vertex_entry_len = (uint32_t)velen;
	tail.fragment_entry_len = (uint32_t)felen;
	memcpy(out + o, &tail, sizeof tail);
	o += sizeof tail;

	memcpy(out + o, vspirv, vbytes);
	o += vbytes;
	memcpy(out + o, fspirv, fbytes);
	o += fbytes;
	memcpy(out + o, vertex_entry, velen);
	o += velen;
	memcpy(out + o, fragment_entry, felen);
	o += felen;

	return o;
}

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
    const struct DrawCmdWire *draws, uint32_t draw_count,
    const struct TextureDescWire *texs, uint32_t tex_count,
    const uint8_t *texpix, uint32_t texpix_len)
{
	const size_t vbytes = (size_t)vspirv_words * 4u;
	const size_t fbytes = (size_t)fspirv_words * 4u;
	const size_t velen = strlen(vertex_entry) + 1u; /* incl. NUL */
	const size_t felen = strlen(fragment_entry) + 1u;
	const size_t attrs_bytes = (size_t)attr_count * sizeof(struct VertexAttrWire);
	const size_t draws_bytes = (size_t)draw_count * sizeof(struct DrawCmdWire);
	const size_t texs_bytes = (size_t)tex_count * sizeof(struct TextureDescWire);

	/* A command list is the geometry path: it must carry a vertex buffer and at least one draw
	 * (the host's decode_forwarded_cmdlist rejects a degenerate list — fail here too, early). */
	if (vertex_stride == 0u || vertex_data_len == 0u || draw_count == 0u)
		return 0;

	const size_t total = sizeof(struct VulkanWorkload) + sizeof(struct ForwardedCmdListTail) +
	                     attrs_bytes + draws_bytes + texs_bytes + vbytes + fbytes +
	                     (size_t)vertex_data_len + (size_t)index_data_len + velen + felen +
	                     (size_t)push_const_len + (size_t)texpix_len;
	if (out == NULL || total > cap)
		return 0;

	size_t o = 0;

	struct VulkanWorkload wl;
	memset(&wl, 0, sizeof wl);
	wl.op = INFINIGPU_VK_OP_FORWARDED_CMDLIST;
	wl.width = width;
	wl.height = height;
	memcpy(wl.bg, bg, sizeof wl.bg);
	wl.scanout_addr = scanout_addr;
	memcpy(out + o, &wl, sizeof wl);
	o += sizeof wl;

	struct ForwardedCmdListTail tail;
	memset(&tail, 0, sizeof tail);
	tail.vertex_spirv_len = (uint32_t)vbytes;
	tail.fragment_spirv_len = (uint32_t)fbytes;
	tail.vertex_entry_len = (uint32_t)velen;
	tail.fragment_entry_len = (uint32_t)felen;
	tail.vertex_stride = vertex_stride;
	tail.attr_count = attr_count;
	tail.vertex_data_len = vertex_data_len;
	tail.index_data_len = index_data_len;
	tail.index_type = index_type;
	tail.draw_count = draw_count;
	tail.topology = topology;
	tail.depth_flags = depth_flags;
	tail.push_const_len = push_const_len;
	tail.tex_count = tex_count;
	memcpy(out + o, &tail, sizeof tail);
	o += sizeof tail;

	/* Sections in the order the host decoder reads them: attrs, draws, texdescs, vSPIR-V, fSPIR-V,
	 * vertex data, index data, vertex entry, fragment entry, push constants, texture pixels. Each
	 * memcpy is guarded so a zero-length section with a NULL pointer isn't passed to memcpy
	 * (undefined behaviour). The texdescs are in the fixed-array region (after draws); their pixels
	 * are the trailing region (after push constants). */
	if (attrs_bytes) {
		memcpy(out + o, attrs, attrs_bytes);
		o += attrs_bytes;
	}
	if (draws_bytes) {
		memcpy(out + o, draws, draws_bytes);
		o += draws_bytes;
	}
	if (texs_bytes) {
		memcpy(out + o, texs, texs_bytes);
		o += texs_bytes;
	}
	memcpy(out + o, vspirv, vbytes);
	o += vbytes;
	memcpy(out + o, fspirv, fbytes);
	o += fbytes;
	memcpy(out + o, vertex_data, vertex_data_len);
	o += vertex_data_len;
	if (index_data_len) {
		memcpy(out + o, index_data, index_data_len);
		o += index_data_len;
	}
	memcpy(out + o, vertex_entry, velen);
	o += velen;
	memcpy(out + o, fragment_entry, felen);
	o += felen;
	if (push_const_len) {
		memcpy(out + o, push_const, push_const_len);
		o += push_const_len;
	}
	if (texpix_len) {
		memcpy(out + o, texpix, texpix_len);
		o += texpix_len;
	}

	return o;
}
