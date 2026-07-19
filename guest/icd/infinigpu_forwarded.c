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
