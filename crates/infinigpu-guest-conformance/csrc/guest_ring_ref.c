/*
 * Freestanding reference implementation of the infinigpu Linux guest's PR4 wire protocol: the SPSC
 * ring PRODUCER (mirrors infinigpu_ring::Ring::push, loom-verified) plus the RESOURCE_* payload
 * builders. This is the exact logic the .ko (guest/linux/infinigpu.c) uses to drive a real
 * DMA-resident ring; kept kernel-independent (stdint only, no iowrite32/dma) so the companion Rust
 * test can drive the *tested device consumer* (infinigpu_device::drain + dispatch) over this code's
 * output and prove guest<->device PR4 interop entirely off-hardware.
 *
 * Layout is pinned two ways: the _Static_asserts below (like guest/include/abi_conformance.c) and,
 * transitively, the interop test itself — any mismatch makes the Rust drain fail.
 */
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include "infinigpu_abi.h"

/*
 * The ring index page — mirrors wire::RingIndices (host consumer: infinigpu_ring::Indices). One
 * 64-byte cacheline; the producer owns tail/seqno_submit, the host consumer owns head/seqno_retired.
 */
struct igpu_ring_indices {
	uint32_t tail;
	uint32_t head;
	uint64_t seqno_submit;
	uint64_t seqno_retired;
	uint32_t status;
	uint32_t reserved[9];
};
_Static_assert(sizeof(struct igpu_ring_indices) == 64, "ring indices are one cacheline");
_Static_assert(offsetof(struct igpu_ring_indices, tail) == 0, "tail@0");
_Static_assert(offsetof(struct igpu_ring_indices, head) == 4, "head@4");
_Static_assert(offsetof(struct igpu_ring_indices, seqno_submit) == 8, "seqno_submit@8");
_Static_assert(offsetof(struct igpu_ring_indices, seqno_retired) == 16, "seqno_retired@16");
_Static_assert(offsetof(struct igpu_ring_indices, status) == 24, "status@24");

/*
 * SPSC producer push — mirrors infinigpu_ring::Ring::push exactly: observe tail (producer-owned)
 * and head (Acquire), reject when full, write the descriptor slot at (tail & (cap-1)), then publish
 * seqno_submit and tail. Returns the assigned 1-based seqno, or 0 if the ring is full.
 *
 * In the .ko the final `tail` store is smp_store_release(&idx->tail, tail+1) (and the head load is
 * smp_load_acquire); here the single-threaded test uses plain stores — what this verifies is the
 * LAYOUT + index arithmetic + slot encoding, i.e. the transcription-risk half. The ordering is
 * inherited by construction from the loom-verified Rust reference.
 */
uint64_t igpu_gref_push(uint8_t *idx_base, uint8_t *desc_base, uint32_t cap,
			uint32_t msg_type, uint32_t data_offset, uint32_t len)
{
	struct igpu_ring_indices *idx = (struct igpu_ring_indices *)idx_base;
	uint32_t tail = idx->tail;
	uint32_t head = idx->head;
	struct Descriptor *slot;
	uint64_t seqno;

	if ((uint32_t)(tail - head) >= cap)
		return 0; /* full — retry after the host drains */

	slot = (struct Descriptor *)(desc_base +
				     (size_t)(tail & (cap - 1)) * sizeof(struct Descriptor));
	seqno = (uint64_t)tail + 1u;
	slot->msg_type = msg_type;
	slot->flags = 0;
	slot->len = len;
	slot->data_offset = data_offset;
	slot->seqno = seqno;
	slot->payload_addr = 0;

	idx->seqno_submit = seqno;
	idx->tail = tail + 1u; /* .ko: smp_store_release(&idx->tail, tail + 1) */
	return seqno;
}

/* Read the host-published highest retired seqno (the guest polls this to resolve fences). */
uint64_t igpu_gref_retired(const uint8_t *idx_base)
{
	return ((const struct igpu_ring_indices *)idx_base)->seqno_retired;
}

/* ---- RESOURCE_* payload builders: write the body into `buf`, return its byte length ---- */

uint32_t igpu_gref_create_blob(uint8_t *buf, uint32_t res_id, uint64_t size)
{
	struct ResourceCreateBlob b;
	memset(&b, 0, sizeof(b));
	b.res_id = res_id;
	b.ctx_id = 0;
	b.blob_mem = 1;
	b.blob_flags = 0;
	b.size = size;
	memcpy(buf, &b, sizeof(b));
	return (uint32_t)sizeof(b);
}

/* Single contiguous segment (dma_alloc_coherent) — the phase-1 backing shortcut. */
uint32_t igpu_gref_attach_backing(uint8_t *buf, uint32_t res_id, uint64_t addr, uint64_t len)
{
	struct AttachBacking h;
	struct MemEntry e;
	memset(&h, 0, sizeof(h));
	memset(&e, 0, sizeof(e));
	h.res_id = res_id;
	h.num_entries = 1;
	e.addr = addr;
	e.length = len;
	memcpy(buf, &h, sizeof(h));
	memcpy(buf + sizeof(h), &e, sizeof(e));
	return (uint32_t)(sizeof(h) + sizeof(e));
}

uint32_t igpu_gref_set_scanout(uint8_t *buf, uint32_t scanout_id, uint32_t res_id,
			       uint32_t w, uint32_t h, uint32_t fmt, uint32_t stride)
{
	struct SetScanoutBlob b;
	memset(&b, 0, sizeof(b));
	b.scanout_id = scanout_id;
	b.res_id = res_id;
	b.width = w;
	b.height = h;
	b.format = fmt;
	b.stride = stride;
	memcpy(buf, &b, sizeof(b));
	return (uint32_t)sizeof(b);
}

uint32_t igpu_gref_flush(uint8_t *buf, uint32_t res_id, uint32_t x, uint32_t y,
			 uint32_t w, uint32_t h)
{
	struct ResourceFlush b;
	memset(&b, 0, sizeof(b));
	b.res_id = res_id;
	b.x = x;
	b.y = y;
	b.w = w;
	b.h = h;
	b._reserved = 0;
	memcpy(buf, &b, sizeof(b));
	return (uint32_t)sizeof(b);
}
