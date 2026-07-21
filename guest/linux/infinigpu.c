// SPDX-License-Identifier: (GPL-2.0-only OR MIT)
//
// Copyright (c) 2026 Infinibay LLC <andres@infinibay.net>
//
// infinigpu — Linux guest DRM/KMS display driver (Phase-0, ADR-0005 Linux).
//
// Dual-licensed MIT/GPL: MIT is Infinibay's chosen license, and the Linux kernel
// requires a GPL-compatible MODULE_LICENSE to use the DRM/KMS EXPORT_SYMBOL_GPL
// stack — "Dual MIT/GPL" satisfies both (a pure "MIT" module would be denied those
// symbols and fail to load).
//
// Binds the infinigpu vfio-user device (1b36:0110) and exposes it as a real
// DRM/KMS display: a /dev/dri/card0 with one CRTC/plane/encoder/connector, dumb
// (contiguous DMA) framebuffers via the GEM-DMA helpers, and fbdev emulation so
// the kernel's fbcon renders the console onto our framebuffer. On every page-flip
// the driver hands the host the framebuffer's guest-physical address; the host
// scans it out (reads the pixels, presents them). This replaces the earlier
// plain-PCI self-test with the actual modeset path.
//
// Contiguous framebuffers (drm_gem_dma) are the deliberate choice: each buffer has
// a single dma_addr_t the host reads as one blob — no scatter-gather. That costs
// one extra module (drm_dma_helper.ko) in the guest; everything else in the DRM
// stack is built into the Ubuntu 6.14 kernel (CONFIG_DRM/KMS_HELPER/FBDEV/GEM_SHMEM=y).
//
// The register + wire layout mirrors crates/infinigpu-abi (kept in sync manually;
// guest/include/infinigpu_abi.h + abi_conformance.c pin the struct layout to Rust).

#include <linux/module.h>
#include <linux/pci.h>
#include <linux/dma-mapping.h>
#include <linux/delay.h>
#include <linux/io.h>
#include <linux/mutex.h>
#include <linux/uaccess.h>   /* copy_from_user, u64_to_user_ptr (forwarded-submit ioctl) */

#include <drm/drm_drv.h>
#include <drm/drm_device.h>
#include <drm/drm_managed.h>
#include <drm/drm_ioctl.h>
#include <drm/drm_file.h>
#include <drm/drm_gem.h>

#include "infinigpu_drm.h"   /* render-node uAPI (guest/include, -I via the Makefile) */
#include <linux/version.h>

#include <drm/drm_atomic.h>
#include <drm/drm_atomic_helper.h>
#include <drm/drm_atomic_state_helper.h>
#include <drm/drm_damage_helper.h>
#include <drm/drm_gem_atomic_helper.h>
#include <drm/drm_rect.h>
#include <drm/drm_probe_helper.h>
#include <drm/drm_gem_dma_helper.h>
#include <drm/drm_gem_framebuffer_helper.h>
#include <drm/drm_fb_dma_helper.h>
#include <drm/drm_fbdev_dma.h>
#include <drm/drm_simple_kms_helper.h>
#include <drm/drm_connector.h>
#include <drm/drm_crtc.h>
#include <drm/drm_plane.h>
#include <drm/drm_framebuffer.h>
#include <drm/drm_vblank.h>
#include <drm/drm_modeset_helper_vtables.h>
#include <drm/drm_fourcc.h>
#include <drm/drm_edid.h>
#include <drm/drm_modes.h>
#include <drm/drm_print.h>
#include <drm/clients/drm_client_setup.h>

#define IGPU_VENDOR 0x1b36
#define IGPU_DEVICE 0x0110

/* BAR0 registers (infinigpu-abi regs::ctrl) */
#define REG_DEV_MAGIC        0x0000
#define REG_ABI_VERSION      0x0004
#define REG_DEV_CAPS         0x0008
#define REG_GLOBAL_CTRL      0x0020
#define REG_RETIRED_LO       0x0050
#define REG_RETIRED_HI       0x0054
#define REG_CMD_RING_BASE_LO 0x0100
#define REG_CMD_RING_BASE_HI 0x0104
#define REG_CMD_RING_SIZE    0x0108  /* per-ctx capacity (entries, pow2) — PR4 real ring */
#define REG_CMD_RING_INDEX_LO 0x011C /* per-ctx RingIndices-page IOVA; non-zero => real drainer */
#define REG_CMD_RING_INDEX_HI 0x0120
#define REG_DOORBELL_CMD0    0x3004

#define DEV_MAGIC            0x49475055u  /* "IGPU" */
#define GLOBAL_CTRL_ENABLE   0x1u

/* wire enums (infinigpu-abi wire) */
#define MSG_SUBMIT_CMD       0x0030u
#define MSG_CURSOR_UPDATE    0x0042u
/* PR4 blob-resource messages (real ring-drainer path) */
#define MSG_RESOURCE_CREATE_BLOB   0x0020u
#define MSG_RESOURCE_ATTACH_BACKING 0x0021u
#define MSG_RESOURCE_DESTROY       0x0024u
#define MSG_SET_SCANOUT_BLOB       0x0040u
#define MSG_RESOURCE_FLUSH         0x0041u
#define ENC_DISPLAY_SCANOUT  0x0101u
#define ENC_DISPLAY_SCANOUT_DAMAGE 0x0102u
#define ENC_VULKAN_VENUSLIKE 1u   /* 3D own-remoting submit (host replays on the GPU) */
/* vk_op (infinigpu-abi wire::vk_op) — the hand-rolled Vulkan workload the host replays. */
#define VK_OP_CLEAR    0u
#define VK_OP_TRIANGLE 1u
#define WIRE_FMT_XRGB8888    3u  /* = wire::format::B8G8R8X8; XRGB8888 LE = [B,G,R,X] */
#define WIRE_FMT_B8G8R8A8    1u  /* = wire::format::B8G8R8A8; the DRM ARGB8888 cursor byte order */
#define DESC_FLAG_FENCED     0x1u

/* cursor_flags (infinigpu-abi wire::cursor_flags) */
#define CUR_VISIBLE          (1u << 0)
#define CUR_MOVE_ONLY        (1u << 1)
#define CUR_PREMULTIPLIED    (1u << 3)
#define CUR_WARP             (1u << 4)

/* DEV_CAPS bits (infinigpu-abi regs::caps) */
#define CAP_DISPLAY_ACCEL    (1u << 5)  /* device accepts DISPLAY_SCANOUT_DAMAGE */
#define CAP_CURSOR_PLANE     (1u << 6)  /* cursor is off the primary: emit CURSOR_UPDATE */

struct igpu_descriptor {
	u32 msg_type, flags, len, data_offset;
	u64 seqno, payload_addr;   /* payload_addr valid iff flags & IGPU_DESC_F_PAYLOAD_ABS */
};
/* descriptor flags (mirror infinigpu-abi wire::desc_flags) */
#define IGPU_DESC_F_PAYLOAD_ABS (1u << 2)   /* payload is at the absolute GPA `payload_addr`, not
					     * ring_base+data_offset — for bodies too large for the
					     * fixed per-slot ring payload (e.g. forwarded SPIR-V) */
struct igpu_submit_cmd {
	u32 ctx_id, encoding, payload_len, flags;
	u64 seqno, in_fence, out_fence;
};
/* Mirrors wire::VulkanWorkload (40B) — the VULKAN_VENUSLIKE SUBMIT_CMD body. `bg` is 4×f32 bits
 * (u32 here so the kernel never touches the FPU; the host reinterprets them as f32). */
struct igpu_vulkan_workload {
	u32 op, width, height, _pad;
	u32 bg[4];
	u64 scanout_addr;
};
struct igpu_scanout_present {
	u32 width, height, pitch, format;
	u64 scanout_addr;
};
/* Superset of igpu_scanout_present + a trailing damage rect (dx,dy,dw,dh). The prefix is
 * byte-identical (scanout_addr@16), so the host reads the common fields from either.
 * Mirrors wire::ScanoutPresentDamaged; pinned by BUILD_BUG_ON in igpu_probe. */
struct igpu_scanout_present_damaged {
	u32 width, height, pitch, format;
	u64 scanout_addr;
	u32 dx, dy, dw, dh;
};
/* Mirrors wire::CursorUpdate (48 bytes). The cursor plane emits this instead of baking the
 * cursor into the primary framebuffer, so the host forwards it to a client-side overlay. */
struct igpu_cursor_update {
	u32 scanout_id;
	u32 flags;
	s32 pos_x, pos_y;
	u16 hot_x, hot_y, width, height;
	u32 pitch, format;
	u64 shape_ref;
	u64 reserved;
};

/* ---- PR4 real ring-drainer wire structs (mirror infinigpu-abi wire; cross-language interop is
 * verified by crates/infinigpu-guest-conformance). Only used on the ring_drainer path. ---- */

/* Mirrors wire::RingIndices — one 64-byte cacheline shared with the host consumer. The guest owns
 * tail/seqno_submit; the host owns head/seqno_retired/status. */
struct igpu_ring_indices {
	u32 tail;
	u32 head;
	u64 seqno_submit;
	u64 seqno_retired;
	u32 status;
	u32 reserved[9];
};
struct igpu_resource_create_blob {
	u32 res_id, ctx_id, blob_mem, blob_flags;
	u64 size;
};
struct igpu_attach_backing {
	u32 res_id, num_entries;
};
struct igpu_mem_entry {
	u64 addr, length;
};
struct igpu_set_scanout_blob {
	u32 scanout_id, res_id, width, height, format, stride;
};
struct igpu_resource_flush {
	u32 res_id, x, y, w, h, reserved;
};

/* Real-ring geometry: a power-of-two descriptor ring with a per-slot payload region, all in one
 * coherent buffer laid out [descriptors: CAP*32][payloads: CAP*IGPU_RING2_PSTRIDE]. */
#define IGPU_RING2_CAP     16u   /* descriptor slots (pow2) */
/* per-slot payload bytes. Largest body is a SUBMIT_CMD (40B header + a 40B VulkanWorkload = 80B for
 * the 3D render-node path); the RESOURCE_* bodies are ≤48B. 128 covers it with headroom. */
#define IGPU_RING2_PSTRIDE 128u
#define IGPU_FBCACHE       4     /* framebuffer->res_id registrations kept live */

/* PR4 real ring-drainer path is off by default (bring-up gate): the tested-and-shipped default is
 * the single-descriptor DISPLAY_SCANOUT[_DAMAGE] path. Enable with infinigpu.ring_drainer=1 once a
 * host advertising the drainer is running, so nothing regresses on today's stack. */
static bool ring_drainer_param;
module_param_named(ring_drainer, ring_drainer_param, bool, 0444);
MODULE_PARM_DESC(ring_drainer,
		 "Use the PR4 real ring drainer + RESOURCE_* blob present path (default: off)");

struct igpu_device {
	struct drm_device drm;
	struct drm_simple_display_pipe pipe;    /* used when !accel_cursor (single primary plane) */
	struct drm_connector connector;

	/* Explicit atomic pipeline, used when accel_cursor (primary + cursor plane). */
	struct drm_plane primary;
	struct drm_plane cursor;
	struct drm_crtc crtc;
	struct drm_encoder encoder;

	void __iomem *bar0;
	void *ring;            /* coherent: descriptor + submit_cmd + payload */
	dma_addr_t ring_dma;
	struct mutex ring_lock; /* serialises submissions (selftest vs fbcon) */
	u64 seqno;
	bool accel_2d;         /* device advertised CAP_DISPLAY_ACCEL → send damage rects */
	bool accel_cursor;     /* device advertised CAP_CURSOR_PLANE → cursor plane + CURSOR_UPDATE */

	/* Cursor-plane tracking (accel_cursor path), to distinguish MOVE_ONLY from a shape
	 * DEFINE and to flag a guest-initiated WARP (a jump with no matching pointer motion). */
	bool cursor_had_fb;
	int cursor_last_x, cursor_last_y;

	/* PR4 real ring-drainer state (only when `ring_drainer`). The index page + the
	 * descriptor/payload ring are DMA-coherent, shared with the host consumer. */
	bool ring_drainer;                     /* module param gate: use the real ring + RESOURCE_* */
	struct igpu_ring_indices *ring2_idx;   /* shared index page (CMD_RING_INDEX) */
	dma_addr_t ring2_idx_dma;
	void *ring2;                           /* [descriptors][payloads] (CMD_RING_BASE) */
	dma_addr_t ring2_dma;
	u32 next_res_id;
	/* res_id currently bound to scanout head 0. The compositor triple-buffers (a distinct
	 * blob res per FB), so the head must follow the flipped buffer: SET_SCANOUT_BLOB is
	 * re-issued whenever the flushed res differs from this, else the host head keeps pointing
	 * at the last-registered buffer and every OTHER buffer's RESOURCE_FLUSH is dropped
	 * ("res N not bound to a scanout") — ~2/3 of frames lost => stale/torn glitches. */
	u32 scanout_res_id;
	/* Framebuffer -> res_id registrations, so a re-flip of the same FB skips re-registration. */
	struct {
		dma_addr_t addr;
		u32 res_id, w, h, pitch;
		bool valid;
	} fbcache[IGPU_FBCACHE];
	u32 fbcache_next;                      /* round-robin eviction cursor */
};

#define to_igpu(d) container_of(d, struct igpu_device, drm)

static const u32 igpu_formats[] = { DRM_FORMAT_XRGB8888 };

/* ---- device submission: hand the host one framebuffer to scan out ---- */

/* Build a SUBMIT_CMD descriptor around an opaque `encoding` payload, ring the doorbell,
 * and return the retired seqno. Shared by the full-frame (DISPLAY_SCANOUT) and damage
 * (DISPLAY_SCANOUT_DAMAGE) paths, which differ only in the payload struct + encoding. */
static u32 igpu_ring_submit(struct igpu_device *idev, u32 encoding,
			    const void *payload, u32 payload_len)
{
	struct igpu_descriptor *d = idev->ring;
	struct igpu_submit_cmd *s = idev->ring + sizeof(*d);
	void *p = idev->ring + sizeof(*d) + sizeof(*s);
	u32 retired;
	u64 seq;

	mutex_lock(&idev->ring_lock);
	seq = ++idev->seqno;

	d->msg_type = MSG_SUBMIT_CMD;
	d->flags = DESC_FLAG_FENCED;
	d->len = payload_len;
	d->data_offset = sizeof(*d) + sizeof(*s);
	d->seqno = seq;
	d->payload_addr = 0;

	s->ctx_id = 0;
	s->encoding = encoding;
	s->payload_len = payload_len;
	s->flags = 0;
	s->seqno = seq;
	s->in_fence = 0;
	s->out_fence = seq;

	memcpy(p, payload, payload_len);

	wmb(); /* ring visible before the doorbell */

	iowrite32(lower_32_bits(idev->ring_dma), idev->bar0 + REG_CMD_RING_BASE_LO);
	iowrite32(upper_32_bits(idev->ring_dma), idev->bar0 + REG_CMD_RING_BASE_HI);
	/* The host processes the ring inside the (non-posted) doorbell write, so
	 * when this iowrite32 returns the present is already retired. */
	iowrite32(1, idev->bar0 + REG_DOORBELL_CMD0);

	retired = ioread32(idev->bar0 + REG_RETIRED_LO);
	mutex_unlock(&idev->ring_lock);
	return retired;
}

static u32 igpu_submit_scanout(struct igpu_device *idev, dma_addr_t fb, u32 w,
			       u32 h, u32 pitch, u32 fmt)
{
	struct igpu_scanout_present p = {
		.width = w, .height = h, .pitch = pitch, .format = fmt,
		.scanout_addr = fb,
	};
	return igpu_ring_submit(idev, ENC_DISPLAY_SCANOUT, &p, sizeof(p));
}

/* Like igpu_submit_scanout but tells the host only (dx,dy,dw,dh) changed, so it
 * DMA-reads/repacks just that sub-rectangle into its persistent scanout surface. */
static u32 igpu_submit_scanout_damaged(struct igpu_device *idev, dma_addr_t fb,
				       u32 w, u32 h, u32 pitch, u32 fmt,
				       u32 dx, u32 dy, u32 dw, u32 dh)
{
	struct igpu_scanout_present_damaged p = {
		.width = w, .height = h, .pitch = pitch, .format = fmt,
		.scanout_addr = fb, .dx = dx, .dy = dy, .dw = dw, .dh = dh,
	};
	return igpu_ring_submit(idev, ENC_DISPLAY_SCANOUT_DAMAGE, &p, sizeof(p));
}

/* Forward decl: the ring-drainer producer is defined below, but the cursor submit (Phase-0 by
 * default) delegates to it in ring_drainer mode. */
static u32 igpu_ring2_push(struct igpu_device *idev, u32 msg_type,
			   const void *payload, u32 payload_len);

/* Emit a CURSOR_UPDATE (msg_type 0x0042, no SUBMIT_CMD wrapper): the body sits directly after
 * the descriptor at data_offset = sizeof(descriptor). The host forwards it to a client-side
 * cursor overlay, so the cursor leaves the primary framebuffer entirely. In ring_drainer mode
 * it rides the real ring instead (see below). */
static u32 igpu_submit_cursor(struct igpu_device *idev, const struct igpu_cursor_update *cu)
{
	struct igpu_descriptor *d = idev->ring;
	void *p = idev->ring + sizeof(*d);
	u32 retired;
	u64 seq;

	mutex_lock(&idev->ring_lock);

	/* In ring_drainer mode the cursor rides the real ring like every other submission
	 * (igpu_flush does the same for display). The legacy path below reprograms
	 * CMD_RING_BASE, which the display path is careful never to do here because it would
	 * corrupt the real ring's base — and the host routes every DOORBELL_CMD0 to the ring
	 * drainer, so a cursor published outside ring2 is never drained (drain_ctx sees the
	 * unadvanced ring2 tail and pops nothing). The 48-byte body fits a ring2 payload slot
	 * (IGPU_RING2_PSTRIDE). */
	if (idev->ring_drainer) {
		retired = igpu_ring2_push(idev, MSG_CURSOR_UPDATE, cu, sizeof(*cu));
		mutex_unlock(&idev->ring_lock);
		return retired;
	}

	seq = ++idev->seqno;

	d->msg_type = MSG_CURSOR_UPDATE;
	d->flags = DESC_FLAG_FENCED;
	d->len = sizeof(*cu);
	d->data_offset = sizeof(*d);
	d->seqno = seq;
	d->payload_addr = 0;

	memcpy(p, cu, sizeof(*cu));

	wmb(); /* ring visible before the doorbell */
	iowrite32(lower_32_bits(idev->ring_dma), idev->bar0 + REG_CMD_RING_BASE_LO);
	iowrite32(upper_32_bits(idev->ring_dma), idev->bar0 + REG_CMD_RING_BASE_HI);
	iowrite32(1, idev->bar0 + REG_DOORBELL_CMD0);

	retired = ioread32(idev->bar0 + REG_RETIRED_LO);
	mutex_unlock(&idev->ring_lock);
	return retired;
}

/* ---- PR4 real ring-drainer producer (mirrors the interop-verified C reference in
 * crates/infinigpu-guest-conformance; here the tail publish is smp_store_release). Only used on
 * the `ring_drainer` path; the descriptor array + a per-slot payload region are DMA-coherent. ---- */

/* SPSC push one message onto the real ring: stage the payload in this slot's payload region, fill
 * the descriptor, publish seqno_submit + tail (release), ring the doorbell (the host drains
 * synchronously in the non-posted write), and return the host-retired seqno. Caller holds
 * ring_lock. Returns 0 (and drops the message) if the ring is full — caller degrades to full-frame. */
static u32 igpu_ring2_push(struct igpu_device *idev, u32 msg_type,
			   const void *payload, u32 payload_len)
{
	struct igpu_ring_indices *idx = idev->ring2_idx;
	u8 *descs = idev->ring2;
	u8 *payloads = descs + IGPU_RING2_CAP * sizeof(struct igpu_descriptor);
	u32 tail = idx->tail;                       /* producer owns tail */
	u32 head = smp_load_acquire(&idx->head);    /* observe host-freed slots */
	struct igpu_descriptor *d;
	u32 slot;
	u64 seq;

	if ((u32)(tail - head) >= IGPU_RING2_CAP)
		return 0;
	if (payload_len > IGPU_RING2_PSTRIDE)
		return 0;

	slot = tail & (IGPU_RING2_CAP - 1);
	d = (struct igpu_descriptor *)(descs + slot * sizeof(*d));
	seq = (u64)tail + 1;

	memcpy(payloads + slot * IGPU_RING2_PSTRIDE, payload, payload_len);
	d->msg_type = msg_type;
	d->flags = 0;
	d->len = payload_len;
	d->data_offset = IGPU_RING2_CAP * sizeof(*d) + slot * IGPU_RING2_PSTRIDE;
	d->seqno = seq;
	d->payload_addr = 0;

	idx->seqno_submit = seq;
	smp_store_release(&idx->tail, tail + 1);    /* slot visible before tail */

	iowrite32(1, idev->bar0 + REG_DOORBELL_CMD0);
	return (u32)smp_load_acquire(&idx->seqno_retired);
}

/* Like igpu_ring2_push, but the payload lives OUT-OF-LINE at the absolute guest-physical address
 * `payload_dma` (a coherent DMA buffer the caller owns) instead of the ring's fixed per-slot region.
 * The host DMA-reads `payload_len` bytes from that address (flags & IGPU_DESC_F_PAYLOAD_ABS). This is
 * how a body too large for IGPU_RING2_PSTRIDE — a forwarded draw carrying the app's SPIR-V — reaches
 * the host. Caller holds ring_lock and must keep `payload_dma` alive until the submit retires.
 * Returns the host-retired seqno, or leaves the ring unchanged (returns retired) if full. */
static u32 igpu_ring2_push_abs(struct igpu_device *idev, u32 msg_type,
			       dma_addr_t payload_dma, u32 payload_len)
{
	struct igpu_ring_indices *idx = idev->ring2_idx;
	u8 *descs = idev->ring2;
	u32 tail = idx->tail;
	u32 head = smp_load_acquire(&idx->head);
	struct igpu_descriptor *d;
	u32 slot;
	u64 seq;

	if ((u32)(tail - head) >= IGPU_RING2_CAP)
		return (u32)smp_load_acquire(&idx->seqno_retired);

	slot = tail & (IGPU_RING2_CAP - 1);
	d = (struct igpu_descriptor *)(descs + slot * sizeof(*d));
	seq = (u64)tail + 1;

	d->msg_type = msg_type;
	d->flags = IGPU_DESC_F_PAYLOAD_ABS;
	d->len = payload_len;
	d->data_offset = 0;                 /* ignored under PAYLOAD_ABS */
	d->seqno = seq;
	d->payload_addr = (u64)payload_dma;

	idx->seqno_submit = seq;
	smp_store_release(&idx->tail, tail + 1);    /* descriptor visible before tail */

	iowrite32(1, idev->bar0 + REG_DOORBELL_CMD0);
	return (u32)smp_load_acquire(&idx->seqno_retired);
}

/* 3D render-node submit: wrap a VulkanWorkload as a SUBMIT_CMD{VULKAN_VENUSLIKE} on the command
 * ring so the host replays it on the physical GPU. The vfio-user doorbell write is synchronous
 * (the host has drained the ring by the time igpu_ring2_push returns), so the retire poll below
 * almost always sees completion immediately; it's a bounded belt-and-suspenders wait. Returns 0 on
 * completion, -EBUSY if the ring is full, -ETIMEDOUT if the host never retired. */
static int igpu_submit_3d(struct igpu_device *idev, const struct igpu_vulkan_workload *wl)
{
	u8 buf[sizeof(struct igpu_submit_cmd) + sizeof(struct igpu_vulkan_workload)];
	struct igpu_submit_cmd sc = {
		.ctx_id = 0,
		.encoding = ENC_VULKAN_VENUSLIKE,
		.payload_len = sizeof(*wl),
	};
	u64 want = 0;
	int i;

	memcpy(buf, &sc, sizeof(sc));
	memcpy(buf + sizeof(sc), wl, sizeof(*wl));

	/* The command ring is shared with the display path and the host drains it asynchronously, so
	 * a burst of fbcon flips can transiently fill the 16-slot ring. Detect a real push via the
	 * seqno_submit delta (igpu_ring2_push returns the *retired* seqno, which can legitimately be 0
	 * right after a valid push — so its return can't distinguish "pushed" from "full"). When full,
	 * kick the doorbell to prompt the host to drain, wait, and retry. */
	for (i = 0; i < 64; i++) {
		u64 before, after;

		mutex_lock(&idev->ring_lock);
		before = idev->ring2_idx->seqno_submit;
		igpu_ring2_push(idev, MSG_SUBMIT_CMD, buf, sizeof(buf));
		after = idev->ring2_idx->seqno_submit;
		mutex_unlock(&idev->ring_lock);

		if (after != before) {
			want = after;   /* the descriptor seqno this submit will retire at */
			break;
		}
		iowrite32(1, idev->bar0 + REG_DOORBELL_CMD0);   /* nudge the drain, then wait for a slot */
		udelay(50);
	}
	if (!want)
		return -EBUSY;   /* ring stayed full — a real ICD would back-pressure further */

	/* Wait for the host to retire our seqno. The first submit host-wide pays a one-time GPU
	 * (Vulkan) init, so allow up to ~2s; steady-state completes in microseconds. */
	for (i = 0; i < 20000; i++) {
		if (smp_load_acquire(&idev->ring2_idx->seqno_retired) >= want)
			return 0;
		udelay(100);
	}
	return -ETIMEDOUT;
}

/* DRM_IOCTL_INFINIGPU_SUBMIT3D: the render-node entrypoint. Replay one hand-rolled Vulkan workload
 * on the host GPU and receive the R8G8B8A8 result into the caller's GEM buffer. This is the guest
 * side of the own-remoting 3D datapath — a thin ICD (or the test submitter) drives it; the host's
 * submit_vulkan executes it on the physical GPU and DMA-writes the pixels back to bo's dma_addr. */
static int igpu_ioctl_submit3d(struct drm_device *dev, void *data, struct drm_file *file)
{
	struct igpu_device *idev = to_igpu(dev);
	struct drm_infinigpu_submit3d *args = data;
	struct drm_gem_object *obj;
	struct drm_gem_dma_object *dma_obj;
	struct igpu_vulkan_workload wl;
	u64 need;
	int ret;

	if (!idev->ring_drainer)
		return -ENODEV;   /* 3D submit rides the real command ring (infinigpu.ring_drainer=1) */
	if (args->op > VK_OP_TRIANGLE)
		return -EINVAL;
	if (args->width == 0 || args->height == 0 ||
	    args->width > 16384 || args->height > 16384)
		return -EINVAL;

	need = (u64)args->width * args->height * 4;

	obj = drm_gem_object_lookup(file, args->bo_handle);
	if (!obj)
		return -ENOENT;
	if (obj->size < need) {
		drm_gem_object_put(obj);
		return -EINVAL;   /* result buffer too small for the requested geometry */
	}
	dma_obj = to_drm_gem_dma_obj(obj);

	memset(&wl, 0, sizeof(wl));
	wl.op = args->op;
	wl.width = args->width;
	wl.height = args->height;
	memcpy(wl.bg, args->bg, sizeof(wl.bg));   /* opaque f32 bits — passed straight through */
	wl.scanout_addr = dma_obj->dma_addr;      /* host DMA-writes the render here */

	ret = igpu_submit_3d(idev, &wl);
	drm_gem_object_put(obj);
	return ret;
}

/* Out-of-line SUBMIT_CMD{VULKAN_VENUSLIKE}: publish `staging_dma` (a coherent DMA buffer holding a
 * submit_cmd header + forwarded body) on the command ring via PAYLOAD_ABS and wait for the host to
 * retire it. Same push-detect-retire dance as igpu_submit_3d; the caller must keep the DMA buffer
 * alive until this returns (the host DMA-reads it during the synchronous doorbell). */
static int igpu_submit_forwarded(struct igpu_device *idev, dma_addr_t staging_dma, u32 total_len)
{
	u64 want = 0;
	int i;

	for (i = 0; i < 64; i++) {
		u64 before, after;

		mutex_lock(&idev->ring_lock);
		before = idev->ring2_idx->seqno_submit;
		igpu_ring2_push_abs(idev, MSG_SUBMIT_CMD, staging_dma, total_len);
		after = idev->ring2_idx->seqno_submit;
		mutex_unlock(&idev->ring_lock);

		if (after != before) {
			want = after;
			break;
		}
		iowrite32(1, idev->bar0 + REG_DOORBELL_CMD0);   /* nudge the drain, then retry */
		udelay(50);
	}
	if (!want)
		return -EBUSY;

	/* First submit host-wide pays a one-time GPU (Vulkan) init — allow ~2s; steady state is µs. */
	for (i = 0; i < 20000; i++) {
		if (smp_load_acquire(&idev->ring2_idx->seqno_retired) >= want)
			return 0;
		udelay(100);
	}
	return -ETIMEDOUT;
}

/* Largest forwarded body we stage in one physically-contiguous coherent buffer. Real vertex+fragment
 * SPIR-V for a first app (triangle, vkcube, small DXVK shaders) is a few KB; 1 MiB is generous while
 * bounding a hostile allocation. Larger shader sets are a scatter-gather follow-up. */
#define IGPU_FWD_MAX_PAYLOAD (1u << 20)

/* DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED: the thin Mesa ICD's submit entrypoint. The caller hands us a
 * pre-serialized forwarded-draw body (VulkanWorkload + ForwardedDrawTail + the app's SPIR-V); we wrap
 * it as a SUBMIT_CMD{VULKAN_VENUSLIKE}, patch the workload's geometry + scanout target to the named
 * GEM buffer (so the host DMA-writes the render into a buffer we validated the size of), and replay
 * it on the host GPU. The body is too large for the ring's inline slot, so it rides out-of-line. */
static int igpu_ioctl_submit_forwarded(struct drm_device *dev, void *data, struct drm_file *file)
{
	struct igpu_device *idev = to_igpu(dev);
	struct drm_infinigpu_submit_forwarded *args = data;
	struct drm_gem_object *obj;
	struct drm_gem_dma_object *dma_obj;
	struct igpu_submit_cmd *sc;
	struct igpu_vulkan_workload *wl;
	dma_addr_t staging_dma;
	void *staging;
	u32 total_len;
	u64 need;
	int ret;

	if (!idev->ring_drainer)
		return -ENODEV;   /* 3D submit rides the real command ring (infinigpu.ring_drainer=1) */
	if (args->width == 0 || args->height == 0 ||
	    args->width > 16384 || args->height > 16384)
		return -EINVAL;
	/* The body must at least contain the VulkanWorkload we patch; cap it to bound the DMA alloc. */
	if (args->payload_len < sizeof(struct igpu_vulkan_workload) ||
	    args->payload_len > IGPU_FWD_MAX_PAYLOAD)
		return -EINVAL;

	need = (u64)args->width * args->height * 4;

	obj = drm_gem_object_lookup(file, args->bo_handle);
	if (!obj)
		return -ENOENT;
	if (obj->size < need) {
		drm_gem_object_put(obj);
		return -EINVAL;   /* result buffer too small for the requested geometry */
	}
	dma_obj = to_drm_gem_dma_obj(obj);

	total_len = sizeof(struct igpu_submit_cmd) + args->payload_len;
	staging = dma_alloc_coherent(idev->drm.dev, total_len, &staging_dma, GFP_KERNEL);
	if (!staging) {
		drm_gem_object_put(obj);
		return -ENOMEM;
	}

	/* [submit_cmd header][forwarded body]. Copy the body from userspace first, then stamp the
	 * header and the fields the host trusts for its DMA writeback (width/height/scanout_addr) — we
	 * OVERWRITE whatever the ICD encoded so the host's write size can't exceed the buffer we sized-
	 * checked above. A hostile/buggy userspace can't make the host DMA past the target BO. */
	if (copy_from_user(staging + sizeof(struct igpu_submit_cmd),
			   u64_to_user_ptr(args->payload_ptr), args->payload_len)) {
		ret = -EFAULT;
		goto out;
	}

	sc = staging;
	memset(sc, 0, sizeof(*sc));
	sc->ctx_id = 0;
	sc->encoding = ENC_VULKAN_VENUSLIKE;
	sc->payload_len = args->payload_len;

	wl = staging + sizeof(struct igpu_submit_cmd);
	wl->width = args->width;
	wl->height = args->height;
	wl->scanout_addr = dma_obj->dma_addr;   /* host DMA-writes the render here (validated size) */

	ret = igpu_submit_forwarded(idev, staging_dma, total_len);

out:
	dma_free_coherent(idev->drm.dev, total_len, staging, staging_dma);
	drm_gem_object_put(obj);
	return ret;
}

/* Register `fb`'s backing as a host blob (CREATE_BLOB + ATTACH_BACKING) once, caching addr->res_id
 * so a re-flip of the same FB skips it. The scanout bind (SET_SCANOUT_BLOB) is NOT done here — it is
 * per-flip in igpu_flush_resource, because the head must follow whichever buffer is being flipped,
 * not stick to the last one registered. Round-robin evicts (with DESTROY) when the cache is full.
 * Returns the res_id, or 0 on a full ring. Caller holds ring_lock. */
static u32 igpu_resource_register(struct igpu_device *idev, dma_addr_t addr,
				  u32 w, u32 h, u32 pitch)
{
	struct igpu_resource_create_blob cb;
	struct { struct igpu_attach_backing h; struct igpu_mem_entry e; } __packed ab;
	u64 size = (u64)pitch * h;
	u32 res_id, i;

	for (i = 0; i < IGPU_FBCACHE; i++)
		if (idev->fbcache[i].valid && idev->fbcache[i].addr == addr &&
		    idev->fbcache[i].pitch == pitch && idev->fbcache[i].h == h &&
		    idev->fbcache[i].w == w)
			return idev->fbcache[i].res_id;

	res_id = ++idev->next_res_id;

	cb.res_id = res_id; cb.ctx_id = 0; cb.blob_mem = 1; cb.blob_flags = 0; cb.size = size;
	if (!igpu_ring2_push(idev, MSG_RESOURCE_CREATE_BLOB, &cb, sizeof(cb)))
		return 0;
	ab.h.res_id = res_id; ab.h.num_entries = 1;
	ab.e.addr = addr; ab.e.length = size;
	if (!igpu_ring2_push(idev, MSG_RESOURCE_ATTACH_BACKING, &ab, sizeof(ab)))
		return 0;

	i = idev->fbcache_next;
	if (idev->fbcache[i].valid) {
		u32 old = idev->fbcache[i].res_id;

		/* If the buffer we're evicting is the one currently bound to the head, forget the
		 * binding so the next flip re-issues SET_SCANOUT_BLOB (the res is about to be
		 * DESTROYed host-side). */
		if (old == idev->scanout_res_id)
			idev->scanout_res_id = 0;
		igpu_ring2_push(idev, MSG_RESOURCE_DESTROY, &old, sizeof(old));
	}
	idev->fbcache[i].addr = addr;
	idev->fbcache[i].res_id = res_id;
	idev->fbcache[i].w = w;
	idev->fbcache[i].h = h;
	idev->fbcache[i].pitch = pitch;
	idev->fbcache[i].valid = true;
	idev->fbcache_next = (i + 1) % IGPU_FBCACHE;
	return res_id;
}

/* PR4 present: register the FB as a blob (once) and flush only the damage rect via RESOURCE_FLUSH.
 * Returns false (caller falls back to the legacy full-frame path) if the ring couldn't take it. */
static bool igpu_flush_resource(struct igpu_device *idev, dma_addr_t addr,
				struct drm_framebuffer *fb, u32 dx, u32 dy, u32 dw, u32 dh)
{
	struct igpu_resource_flush rf;
	struct igpu_set_scanout_blob sb;
	u32 res_id;
	bool ok = false;

	mutex_lock(&idev->ring_lock);
	res_id = igpu_resource_register(idev, addr, fb->width, fb->height, fb->pitches[0]);
	if (res_id) {
		/* Point scanout head 0 at the buffer actually being flipped, so its RESOURCE_FLUSH
		 * resolves to a bound scanout host-side. Only when it changed (the compositor cycles
		 * a few buffers), so a static screen re-flushing one buffer costs no extra descriptor. */
		if (res_id != idev->scanout_res_id) {
			sb.scanout_id = 0; sb.res_id = res_id;
			sb.width = fb->width; sb.height = fb->height;
			sb.format = WIRE_FMT_XRGB8888; sb.stride = fb->pitches[0];
			igpu_ring2_push(idev, MSG_SET_SCANOUT_BLOB, &sb, sizeof(sb));
			idev->scanout_res_id = res_id;
		}
		rf.res_id = res_id; rf.x = dx; rf.y = dy; rf.w = dw; rf.h = dh; rf.reserved = 0;
		igpu_ring2_push(idev, MSG_RESOURCE_FLUSH, &rf, sizeof(rf));
		ok = true;
	}
	mutex_unlock(&idev->ring_lock);
	return ok;
}

static void igpu_flush(struct igpu_device *idev, struct drm_plane_state *state)
{
	struct drm_framebuffer *fb = state->fb;
	dma_addr_t addr;

	if (!fb)
		return;
	addr = drm_fb_dma_get_gem_addr(fb, state, 0);
	if (idev->ring_drainer) {
		/* Best-effort; a full ring drops this frame (the next flip retries). Never fall back
		 * to the legacy path here — it reprograms CMD_RING_BASE away from the real ring. */
		igpu_flush_resource(idev, addr, fb, 0, 0, fb->width, fb->height);
		return;
	}
	igpu_submit_scanout(idev, addr, fb->width, fb->height, fb->pitches[0],
			    WIRE_FMT_XRGB8888);
}

/* Accelerated present: submit only the merged damage rect the compositor/fbcon attached.
 * Falls back to a full-frame present when there is no usable damage — a framebuffer swap,
 * a scaled plane, or the first flip after modeset (drm_atomic_helper_damage_merged returns
 * false), which is exactly when the host's persistent surface must be fully refreshed. */
static void igpu_flush_damaged(struct igpu_device *idev,
			       struct drm_plane_state *old_state,
			       struct drm_plane_state *state)
{
	struct drm_framebuffer *fb = state->fb;
	struct drm_rect damage;
	dma_addr_t addr;

	u32 dx, dy, dw, dh;

	if (!fb)
		return;
	addr = drm_fb_dma_get_gem_addr(fb, state, 0);

	if (!drm_atomic_helper_damage_merged(old_state, state, &damage)) {
		/* Full-frame: framebuffer swap / scaled plane / first flip after modeset. */
		if (idev->ring_drainer) {
			igpu_flush_resource(idev, addr, fb, 0, 0, fb->width, fb->height);
			return;
		}
		igpu_submit_scanout(idev, addr, fb->width, fb->height,
				    fb->pitches[0], WIRE_FMT_XRGB8888);
		return;
	}

	/* Clamp the merged box to the framebuffer (defensive; the host clamps too). */
	if (damage.x1 < 0)
		damage.x1 = 0;
	if (damage.y1 < 0)
		damage.y1 = 0;
	if (damage.x2 > (int)fb->width)
		damage.x2 = fb->width;
	if (damage.y2 > (int)fb->height)
		damage.y2 = fb->height;
	if (damage.x2 <= damage.x1 || damage.y2 <= damage.y1)
		return; /* zero-area after clamp: nothing changed */

	dx = damage.x1;
	dy = damage.y1;
	dw = damage.x2 - damage.x1;
	dh = damage.y2 - damage.y1;

	if (idev->ring_drainer) {
		igpu_flush_resource(idev, addr, fb, dx, dy, dw, dh);
		return;
	}

	igpu_submit_scanout_damaged(idev, addr, fb->width, fb->height,
				    fb->pitches[0], WIRE_FMT_XRGB8888,
				    dx, dy, dw, dh);
}

/* ---- simple display pipe ---- */

static void igpu_pipe_enable(struct drm_simple_display_pipe *pipe,
			     struct drm_crtc_state *crtc_state,
			     struct drm_plane_state *plane_state)
{
	igpu_flush(to_igpu(pipe->crtc.dev), plane_state);
}

static void igpu_pipe_disable(struct drm_simple_display_pipe *pipe)
{
	/* nothing to tear down on the host for a stateless scan-out */
}

static void igpu_pipe_update(struct drm_simple_display_pipe *pipe,
			     struct drm_plane_state *old_state)
{
	struct drm_crtc *crtc = &pipe->crtc;
	struct igpu_device *idev = to_igpu(crtc->dev);

	if (idev->accel_2d)
		igpu_flush_damaged(idev, old_state, pipe->plane.state);
	else
		igpu_flush(idev, pipe->plane.state);

	/* No hardware vblank: complete any flip event immediately. */
	if (crtc->state->event) {
		spin_lock_irq(&crtc->dev->event_lock);
		drm_crtc_send_vblank_event(crtc, crtc->state->event);
		crtc->state->event = NULL;
		spin_unlock_irq(&crtc->dev->event_lock);
	}
}

static const struct drm_simple_display_pipe_funcs igpu_pipe_funcs = {
	.enable = igpu_pipe_enable,
	.disable = igpu_pipe_disable,
	.update = igpu_pipe_update,
	/* prepare_fb left NULL: DRM calls drm_gem_plane_helper_prepare_fb() for us */
};

/* ---- explicit atomic pipeline (accel_cursor: primary + cursor plane + CRTC + encoder) ----
 *
 * When the device advertises CAP_CURSOR_PLANE we abandon the single-plane simple pipe for an
 * explicit primary + DRM_PLANE_TYPE_CURSOR plane, so compositors offload the cursor to the plane
 * (a cursor-only commit touches only the cursor plane and does NOT reflush the primary). The
 * cursor plane emits CURSOR_UPDATE instead of baking the sprite into the primary framebuffer.
 * Gated on kernel >= 6.6 (drm_plane_state.hotspot_x/y). Ships behind CAP_CURSOR_PLANE; the
 * simple-pipe path stays the default and the fallback. See docs/adr/CLIENT-PLANE-COMPOSITOR.md. */
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 6, 0)

static const u32 igpu_cursor_formats[] = { DRM_FORMAT_ARGB8888 };

static int igpu_check_plane(struct drm_plane *plane, struct drm_atomic_state *state,
			    bool can_position)
{
	struct drm_plane_state *new = drm_atomic_get_new_plane_state(state, plane);
	struct drm_crtc_state *crtc_state;

	if (!new->crtc || !new->fb)
		return 0;
	crtc_state = drm_atomic_get_new_crtc_state(state, new->crtc);
	return drm_atomic_helper_check_plane_state(new, crtc_state,
						   DRM_PLANE_NO_SCALING,
						   DRM_PLANE_NO_SCALING,
						   can_position, true);
}

static int igpu_primary_check(struct drm_plane *plane, struct drm_atomic_state *state)
{
	return igpu_check_plane(plane, state, false);
}

static void igpu_primary_update(struct drm_plane *plane, struct drm_atomic_state *state)
{
	struct igpu_device *idev = container_of(plane, struct igpu_device, primary);
	struct drm_plane_state *new = drm_atomic_get_new_plane_state(state, plane);
	struct drm_plane_state *old = drm_atomic_get_old_plane_state(state, plane);

	if (!new->fb)
		return;
	if (idev->accel_2d)
		igpu_flush_damaged(idev, old, new);
	else
		igpu_flush(idev, new);
}

static const struct drm_plane_helper_funcs igpu_primary_helper_funcs = {
	.prepare_fb = drm_gem_plane_helper_prepare_fb,
	.atomic_check = igpu_primary_check,
	.atomic_update = igpu_primary_update,
};

static int igpu_cursor_check(struct drm_plane *plane, struct drm_atomic_state *state)
{
	return igpu_check_plane(plane, state, true);
}

static void igpu_cursor_hide(struct igpu_device *idev)
{
	struct igpu_cursor_update cu = { 0 };

	cu.pos_x = idev->cursor_last_x;
	cu.pos_y = idev->cursor_last_y; /* flags=0 → VISIBLE clear */
	idev->cursor_had_fb = false;
	igpu_submit_cursor(idev, &cu);
}

static void igpu_cursor_update_plane(struct drm_plane *plane, struct drm_atomic_state *state)
{
	struct igpu_device *idev = container_of(plane, struct igpu_device, cursor);
	struct drm_plane_state *new = drm_atomic_get_new_plane_state(state, plane);
	struct drm_plane_state *old = drm_atomic_get_old_plane_state(state, plane);
	struct drm_framebuffer *fb = new->fb;
	struct igpu_cursor_update cu = { 0 };
	int dx, dy;

	if (!fb) {
		igpu_cursor_hide(idev);
		return;
	}

	cu.scanout_id = 0;
	cu.flags = CUR_VISIBLE | CUR_PREMULTIPLIED;
	cu.pos_x = new->crtc_x;
	cu.pos_y = new->crtc_y;
	cu.hot_x = new->hotspot_x;
	cu.hot_y = new->hotspot_y;
	cu.width = fb->width;
	cu.height = fb->height;
	cu.pitch = fb->pitches[0];
	cu.format = WIRE_FMT_B8G8R8A8;
	cu.shape_ref = drm_fb_dma_get_gem_addr(fb, new, 0);

	/* Pure move (same fb pointer) vs a new shape DEFINE (fb changed). */
	if (old && old->fb == fb)
		cu.flags |= CUR_MOVE_ONLY;

	/* Best-effort WARP: a jump with no incremental pointer motion (menu recenter, XWarpPointer).
	 * At the KMS layer there is no input provenance, so we approximate with a jump threshold. */
	dx = new->crtc_x - idev->cursor_last_x;
	dy = new->crtc_y - idev->cursor_last_y;
	if (idev->cursor_had_fb && (abs(dx) > 64 || abs(dy) > 64))
		cu.flags |= CUR_WARP;
	idev->cursor_last_x = new->crtc_x;
	idev->cursor_last_y = new->crtc_y;
	idev->cursor_had_fb = true;

	igpu_submit_cursor(idev, &cu);
}

static void igpu_cursor_disable(struct drm_plane *plane, struct drm_atomic_state *state)
{
	igpu_cursor_hide(container_of(plane, struct igpu_device, cursor));
}

static const struct drm_plane_helper_funcs igpu_cursor_helper_funcs = {
	.prepare_fb = drm_gem_plane_helper_prepare_fb,
	.atomic_check = igpu_cursor_check,
	.atomic_update = igpu_cursor_update_plane,
	.atomic_disable = igpu_cursor_disable,
};

static const struct drm_plane_funcs igpu_plane_funcs = {
	.update_plane = drm_atomic_helper_update_plane,
	.disable_plane = drm_atomic_helper_disable_plane,
	.destroy = drm_plane_cleanup,
	.reset = drm_atomic_helper_plane_reset,
	.atomic_duplicate_state = drm_atomic_helper_plane_duplicate_state,
	.atomic_destroy_state = drm_atomic_helper_plane_destroy_state,
};

static void igpu_crtc_atomic_flush(struct drm_crtc *crtc, struct drm_atomic_state *state)
{
	/* No hardware vblank: complete any flip event immediately (mirrors the simple-pipe path). */
	if (crtc->state->event) {
		spin_lock_irq(&crtc->dev->event_lock);
		drm_crtc_send_vblank_event(crtc, crtc->state->event);
		crtc->state->event = NULL;
		spin_unlock_irq(&crtc->dev->event_lock);
	}
}

static const struct drm_crtc_helper_funcs igpu_crtc_helper_funcs = {
	.atomic_flush = igpu_crtc_atomic_flush,
};

static const struct drm_crtc_funcs igpu_crtc_funcs = {
	.set_config = drm_atomic_helper_set_config,
	.page_flip = drm_atomic_helper_page_flip,
	.destroy = drm_crtc_cleanup,
	.reset = drm_atomic_helper_crtc_reset,
	.atomic_duplicate_state = drm_atomic_helper_crtc_duplicate_state,
	.atomic_destroy_state = drm_atomic_helper_crtc_destroy_state,
};

/* Build the explicit primary + cursor + CRTC + encoder pipeline. Returns 0 or a negative errno. */
static int igpu_init_cursor_pipeline(struct igpu_device *idev)
{
	struct drm_device *drm = &idev->drm;
	int ret;

	ret = drm_universal_plane_init(drm, &idev->primary, 0, &igpu_plane_funcs,
				       igpu_formats, ARRAY_SIZE(igpu_formats), NULL,
				       DRM_PLANE_TYPE_PRIMARY, NULL);
	if (ret)
		return ret;
	drm_plane_helper_add(&idev->primary, &igpu_primary_helper_funcs);
	if (idev->accel_2d)
		drm_plane_enable_fb_damage_clips(&idev->primary);

	ret = drm_universal_plane_init(drm, &idev->cursor, 0, &igpu_plane_funcs,
				       igpu_cursor_formats, ARRAY_SIZE(igpu_cursor_formats),
				       NULL, DRM_PLANE_TYPE_CURSOR, NULL);
	if (ret)
		return ret;
	drm_plane_helper_add(&idev->cursor, &igpu_cursor_helper_funcs);

	ret = drm_crtc_init_with_planes(drm, &idev->crtc, &idev->primary, &idev->cursor,
					&igpu_crtc_funcs, NULL);
	if (ret)
		return ret;
	drm_crtc_helper_add(&idev->crtc, &igpu_crtc_helper_funcs);

	ret = drm_simple_encoder_init(drm, &idev->encoder, DRM_MODE_ENCODER_VIRTUAL);
	if (ret)
		return ret;
	idev->encoder.possible_crtcs = drm_crtc_mask(&idev->crtc);

	return drm_connector_attach_encoder(&idev->connector, &idev->encoder);
}

#endif /* LINUX_VERSION_CODE >= 6.6 */

/* ---- connector: one virtual output with a fixed preferred mode ---- */

static int igpu_conn_get_modes(struct drm_connector *connector)
{
	int count = drm_add_modes_noedid(connector, 2048, 2048);

	drm_set_preferred_mode(connector, 1024, 768);
	return count;
}

static const struct drm_connector_helper_funcs igpu_conn_helper_funcs = {
	.get_modes = igpu_conn_get_modes,
};

static const struct drm_connector_funcs igpu_connector_funcs = {
	.fill_modes = drm_helper_probe_single_connector_modes,
	.destroy = drm_connector_cleanup,
	.reset = drm_atomic_helper_connector_reset,
	.atomic_duplicate_state = drm_atomic_helper_connector_duplicate_state,
	.atomic_destroy_state = drm_atomic_helper_connector_destroy_state,
};

static const struct drm_mode_config_funcs igpu_mode_config_funcs = {
	/* _with_dirty wires drm_atomic_helper_dirtyfb, so fbcon/compositor damage
	 * (post-modeset console writes) triggers an atomic commit → a present. Without
	 * it, a directly-scanned-out DMA framebuffer only presents on the boot modeset,
	 * and a live desktop would freeze after boot. */
	.fb_create = drm_gem_fb_create_with_dirty,
	.atomic_check = drm_atomic_helper_check,
	.atomic_commit = drm_atomic_helper_commit,
};

/* ---- driver ---- */

DEFINE_DRM_GEM_DMA_FOPS(igpu_fops);

/* Render-node ioctls (DRIVER_RENDER). SUBMIT3D replays a Vulkan workload on the host GPU — the
 * guest half of the own-remoting 3D datapath. DRM_RENDER_ALLOW lets an unprivileged render-node
 * client (/dev/dri/renderD128) call it, not just the card node's DRM master. */
static const struct drm_ioctl_desc igpu_ioctls[] = {
	DRM_IOCTL_DEF_DRV(INFINIGPU_SUBMIT3D, igpu_ioctl_submit3d, DRM_RENDER_ALLOW),
	DRM_IOCTL_DEF_DRV(INFINIGPU_SUBMIT_FORWARDED, igpu_ioctl_submit_forwarded, DRM_RENDER_ALLOW),
};

static const struct drm_driver igpu_drm_driver = {
	.driver_features = DRIVER_MODESET | DRIVER_ATOMIC | DRIVER_GEM | DRIVER_RENDER,
	.fops = &igpu_fops,
	.ioctls = igpu_ioctls,
	.num_ioctls = ARRAY_SIZE(igpu_ioctls),
	DRM_GEM_DMA_DRIVER_OPS,
	DRM_FBDEV_DMA_DRIVER_OPS,
	.name = "infinigpu",
	.desc = "infinigpu paravirtual display (Phase-0)",
	.major = 1,
	.minor = 1,   /* +INFINIGPU_SUBMIT_FORWARDED (forwarded-draw render-node ioctl) */
};

/* Deterministic bring-up check: present a recognizable gradient through the KMS
 * ring path and confirm the host retired it. Runs before fbdev is up so it can't
 * race a concurrent console flush. The host also dumps it as frame-0001.ppm. */
static void igpu_kms_selftest(struct igpu_device *idev)
{
	const u32 w = 128, h = 128;
	dma_addr_t dma;
	u32 *px, i, retired;
	void *buf;

	buf = dma_alloc_coherent(idev->drm.dev, w * h * 4, &dma, GFP_KERNEL);
	if (!buf) {
		dev_warn(idev->drm.dev, "INFINIGPU-KMS: selftest buffer alloc failed\n");
		return;
	}
	px = buf;
	for (i = 0; i < w * h; i++) {
		u32 x = i % w, y = i / w;
		/* XRGB8888: X=0xff, R=x*2, G=y*2, B=0x40 (non-blank everywhere) */
		px[i] = (0xffu << 24) | ((x * 2) << 16) | ((y * 2) << 8) | 0x40u;
	}
	wmb();
	retired = igpu_submit_scanout(idev, dma, w, h, w * 4, WIRE_FMT_XRGB8888);

	if (retired >= idev->seqno)
		dev_info(idev->drm.dev,
			 "INFINIGPU-KMS: PASS pipe present retired=%u seqno=%llu\n",
			 retired, idev->seqno);
	else
		dev_err(idev->drm.dev,
			"INFINIGPU-KMS: FAIL retired=%u seqno=%llu\n",
			retired, idev->seqno);

	dma_free_coherent(idev->drm.dev, w * h * 4, buf, dma);
}

static int igpu_probe(struct pci_dev *pdev, const struct pci_device_id *id)
{
	struct igpu_device *idev;
	struct drm_device *drm;
	void __iomem *bar0;
	u32 magic, abi, caps;
	int ret;

	BUILD_BUG_ON(sizeof(struct igpu_descriptor) != 32);
	BUILD_BUG_ON(sizeof(struct igpu_submit_cmd) != 40);
	BUILD_BUG_ON(sizeof(struct igpu_scanout_present) != 24);
	BUILD_BUG_ON(sizeof(struct igpu_scanout_present_damaged) != 40);
	BUILD_BUG_ON(offsetof(struct igpu_scanout_present_damaged, scanout_addr) != 16);
	BUILD_BUG_ON(offsetof(struct igpu_scanout_present_damaged, dx) != 24);
	BUILD_BUG_ON(sizeof(struct igpu_cursor_update) != 48);
	BUILD_BUG_ON(offsetof(struct igpu_cursor_update, pos_x) != 8);
	BUILD_BUG_ON(offsetof(struct igpu_cursor_update, shape_ref) != 32);
	/* PR4 real-ring wire structs (mirror infinigpu-abi; interop-verified by the guest-conformance
	 * crate). Pin the shared layouts here too. */
	BUILD_BUG_ON(sizeof(struct igpu_ring_indices) != 64);
	BUILD_BUG_ON(offsetof(struct igpu_ring_indices, seqno_retired) != 16);
	BUILD_BUG_ON(sizeof(struct igpu_resource_create_blob) != 24);
	BUILD_BUG_ON(sizeof(struct igpu_attach_backing) != 8);
	BUILD_BUG_ON(sizeof(struct igpu_mem_entry) != 16);
	BUILD_BUG_ON(sizeof(struct igpu_set_scanout_blob) != 24);
	BUILD_BUG_ON(sizeof(struct igpu_resource_flush) != 24);
	BUILD_BUG_ON(sizeof(struct igpu_vulkan_workload) != 40);
	BUILD_BUG_ON(offsetof(struct igpu_vulkan_workload, scanout_addr) != 32);
	/* Forwarded-submit uAPI: the naturally-packed args (4×u32 + u64) the ICD passes. */
	BUILD_BUG_ON(sizeof(struct drm_infinigpu_submit_forwarded) != 24);
	BUILD_BUG_ON(offsetof(struct drm_infinigpu_submit_forwarded, payload_ptr) != 16);
	/* The 3D SUBMIT_CMD body (header + workload) must fit one ring payload slot. */
	BUILD_BUG_ON(sizeof(struct igpu_submit_cmd) + sizeof(struct igpu_vulkan_workload) > IGPU_RING2_PSTRIDE);
	BUILD_BUG_ON((IGPU_RING2_CAP & (IGPU_RING2_CAP - 1)) != 0); /* power of two */

	ret = pcim_enable_device(pdev);
	if (ret)
		return ret;
	pci_set_master(pdev);

	ret = dma_set_mask_and_coherent(&pdev->dev, DMA_BIT_MASK(64));
	if (ret)
		return ret;

	ret = pcim_iomap_regions(pdev, BIT(0), KBUILD_MODNAME);
	if (ret)
		return ret;
	bar0 = pcim_iomap_table(pdev)[0];

	magic = ioread32(bar0 + REG_DEV_MAGIC);
	abi = ioread32(bar0 + REG_ABI_VERSION);
	caps = ioread32(bar0 + REG_DEV_CAPS);
	if (magic != DEV_MAGIC) {
		dev_err(&pdev->dev, "bad magic %#x (not an infinigpu device)\n", magic);
		return -ENODEV;
	}
	dev_info(&pdev->dev, "infinigpu magic=%#x abi=%#x caps=%#x\n", magic, abi, caps);

	idev = devm_drm_dev_alloc(&pdev->dev, &igpu_drm_driver, struct igpu_device, drm);
	if (IS_ERR(idev))
		return PTR_ERR(idev);
	drm = &idev->drm;
	idev->bar0 = bar0;
	idev->accel_2d = !!(caps & CAP_DISPLAY_ACCEL);
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 6, 0)
	/* The explicit cursor-plane pipeline needs drm_plane_state.hotspot_x/y (6.6+). On older
	 * kernels we always fall back to the simple pipe (software cursor). */
	idev->accel_cursor = !!(caps & CAP_CURSOR_PLANE);
#endif
	mutex_init(&idev->ring_lock);
	pci_set_drvdata(pdev, idev);

	idev->ring = dmam_alloc_coherent(&pdev->dev, PAGE_SIZE, &idev->ring_dma, GFP_KERNEL);
	if (!idev->ring)
		return -ENOMEM;

	/* PR4 real ring drainer (opt-in): a shared RingIndices page + a descriptor/payload ring.
	 * Programming a non-zero CMD_RING_INDEX switches the host to the bounded two-phase drainer;
	 * the scanout path then registers each FB as a blob and flips via RESOURCE_FLUSH. */
	idev->ring_drainer = ring_drainer_param;
	if (idev->ring_drainer) {
		size_t ring2_bytes = IGPU_RING2_CAP * sizeof(struct igpu_descriptor) +
				     IGPU_RING2_CAP * IGPU_RING2_PSTRIDE;

		idev->ring2_idx = dmam_alloc_coherent(&pdev->dev, PAGE_SIZE,
						      &idev->ring2_idx_dma, GFP_KERNEL);
		idev->ring2 = dmam_alloc_coherent(&pdev->dev, ring2_bytes,
						  &idev->ring2_dma, GFP_KERNEL);
		if (!idev->ring2_idx || !idev->ring2)
			return -ENOMEM;
		memset(idev->ring2_idx, 0, sizeof(*idev->ring2_idx));
		idev->next_res_id = 0;
		idev->scanout_res_id = 0;
		idev->fbcache_next = 0;

		iowrite32(lower_32_bits(idev->ring2_dma), bar0 + REG_CMD_RING_BASE_LO);
		iowrite32(upper_32_bits(idev->ring2_dma), bar0 + REG_CMD_RING_BASE_HI);
		iowrite32(IGPU_RING2_CAP, bar0 + REG_CMD_RING_SIZE);
		iowrite32(lower_32_bits(idev->ring2_idx_dma), bar0 + REG_CMD_RING_INDEX_LO);
		iowrite32(upper_32_bits(idev->ring2_idx_dma), bar0 + REG_CMD_RING_INDEX_HI);
		dev_info(&pdev->dev, "infinigpu: PR4 ring drainer enabled (cap=%u)\n",
			 IGPU_RING2_CAP);
	}

	iowrite32(GLOBAL_CTRL_ENABLE, bar0 + REG_GLOBAL_CTRL);

	ret = drmm_mode_config_init(drm);
	if (ret)
		return ret;
	drm->mode_config.funcs = &igpu_mode_config_funcs;
	drm->mode_config.min_width = 0;
	drm->mode_config.min_height = 0;
	drm->mode_config.max_width = 2048;
	drm->mode_config.max_height = 2048;
	drm->mode_config.preferred_depth = 24;

	ret = drm_connector_init(drm, &idev->connector, &igpu_connector_funcs,
				 DRM_MODE_CONNECTOR_VIRTUAL);
	if (ret)
		return ret;
	drm_connector_helper_add(&idev->connector, &igpu_conn_helper_funcs);

	if (idev->accel_cursor) {
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 6, 0)
		/* Explicit primary + cursor plane so the compositor offloads the cursor to a plane
		 * (advertised size) and the cursor leaves the primary framebuffer. */
		drm->mode_config.cursor_width = 256;
		drm->mode_config.cursor_height = 256;
		ret = igpu_init_cursor_pipeline(idev);
		if (ret)
			return ret;
#endif
	} else {
		ret = drm_simple_display_pipe_init(drm, &idev->pipe, &igpu_pipe_funcs,
						   igpu_formats, ARRAY_SIZE(igpu_formats),
						   NULL, &idev->connector);
		if (ret)
			return ret;

		/* Expose FB_DAMAGE_CLIPS so an atomic compositor (weston/mutter) can attach damage
		 * rects; the fbcon dirtyfb path sets damage directly on the plane state regardless.
		 * Without this, compositors commit full-frame and the accel path degrades to full
		 * presents — correct, just not accelerated. */
		if (idev->accel_2d)
			drm_plane_enable_fb_damage_clips(&idev->pipe.plane);
	}

	drm_mode_config_reset(drm);

	ret = drm_dev_register(drm, 0);
	if (ret)
		return ret;

	/* Prove the KMS ring path deterministically before fbcon starts flushing. Skipped on the
	 * ring_drainer path: the selftest uses the legacy single-descriptor submit, which reprograms
	 * CMD_RING_BASE away from the real ring (the ring2 wire path is covered off-hardware by the
	 * infinigpu-guest-conformance interop test instead). */
	if (!idev->ring_drainer)
		igpu_kms_selftest(idev);

	/* Bring up fbdev emulation → fbcon renders the console on our framebuffer. */
	drm_client_setup(drm, NULL);

	dev_info(&pdev->dev,
		 "INFINIGPU-KMS: registered /dev/dri/card%d (2D accel %s, cursor plane %s)\n",
		 drm->primary->index, idev->accel_2d ? "on" : "off",
		 idev->accel_cursor ? "on" : "off");
	return 0;
}

static void igpu_remove(struct pci_dev *pdev)
{
	struct igpu_device *idev = pci_get_drvdata(pdev);

	drm_dev_unregister(&idev->drm);
	drm_atomic_helper_shutdown(&idev->drm);
}

static void igpu_shutdown(struct pci_dev *pdev)
{
	struct igpu_device *idev = pci_get_drvdata(pdev);

	drm_atomic_helper_shutdown(&idev->drm);
}

static const struct pci_device_id igpu_ids[] = {
	{ PCI_DEVICE(IGPU_VENDOR, IGPU_DEVICE) },
	{ 0 }
};
MODULE_DEVICE_TABLE(pci, igpu_ids);

static struct pci_driver igpu_driver = {
	.name = KBUILD_MODNAME,
	.id_table = igpu_ids,
	.probe = igpu_probe,
	.remove = igpu_remove,
	.shutdown = igpu_shutdown,
};
module_pci_driver(igpu_driver);

MODULE_LICENSE("Dual MIT/GPL");
MODULE_DESCRIPTION("infinigpu guest DRM/KMS display driver (Phase-0)");
MODULE_AUTHOR("Infinibay LLC <andres@infinibay.net>");
