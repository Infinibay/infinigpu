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

#include <drm/drm_drv.h>
#include <drm/drm_device.h>
#include <drm/drm_managed.h>
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
#define REG_DOORBELL_CMD0    0x3004

#define DEV_MAGIC            0x49475055u  /* "IGPU" */
#define GLOBAL_CTRL_ENABLE   0x1u

/* wire enums (infinigpu-abi wire) */
#define MSG_SUBMIT_CMD       0x0030u
#define MSG_CURSOR_UPDATE    0x0042u
#define ENC_DISPLAY_SCANOUT  0x0101u
#define ENC_DISPLAY_SCANOUT_DAMAGE 0x0102u
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
	u64 seqno, reserved;
};
struct igpu_submit_cmd {
	u32 ctx_id, encoding, payload_len, flags;
	u64 seqno, in_fence, out_fence;
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
	d->reserved = 0;

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

/* Emit a CURSOR_UPDATE (msg_type 0x0042, no SUBMIT_CMD wrapper): the body sits directly after
 * the descriptor at data_offset = sizeof(descriptor). The host forwards it to a client-side
 * cursor overlay, so the cursor leaves the primary framebuffer entirely. */
static u32 igpu_submit_cursor(struct igpu_device *idev, const struct igpu_cursor_update *cu)
{
	struct igpu_descriptor *d = idev->ring;
	void *p = idev->ring + sizeof(*d);
	u32 retired;
	u64 seq;

	mutex_lock(&idev->ring_lock);
	seq = ++idev->seqno;

	d->msg_type = MSG_CURSOR_UPDATE;
	d->flags = DESC_FLAG_FENCED;
	d->len = sizeof(*cu);
	d->data_offset = sizeof(*d);
	d->seqno = seq;
	d->reserved = 0;

	memcpy(p, cu, sizeof(*cu));

	wmb(); /* ring visible before the doorbell */
	iowrite32(lower_32_bits(idev->ring_dma), idev->bar0 + REG_CMD_RING_BASE_LO);
	iowrite32(upper_32_bits(idev->ring_dma), idev->bar0 + REG_CMD_RING_BASE_HI);
	iowrite32(1, idev->bar0 + REG_DOORBELL_CMD0);

	retired = ioread32(idev->bar0 + REG_RETIRED_LO);
	mutex_unlock(&idev->ring_lock);
	return retired;
}

static void igpu_flush(struct igpu_device *idev, struct drm_plane_state *state)
{
	struct drm_framebuffer *fb = state->fb;
	dma_addr_t addr;

	if (!fb)
		return;
	addr = drm_fb_dma_get_gem_addr(fb, state, 0);
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

	if (!fb)
		return;
	addr = drm_fb_dma_get_gem_addr(fb, state, 0);

	if (!drm_atomic_helper_damage_merged(old_state, state, &damage)) {
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

	igpu_submit_scanout_damaged(idev, addr, fb->width, fb->height,
				    fb->pitches[0], WIRE_FMT_XRGB8888,
				    damage.x1, damage.y1,
				    damage.x2 - damage.x1, damage.y2 - damage.y1);
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

static const struct drm_driver igpu_drm_driver = {
	.driver_features = DRIVER_MODESET | DRIVER_ATOMIC | DRIVER_GEM,
	.fops = &igpu_fops,
	DRM_GEM_DMA_DRIVER_OPS,
	DRM_FBDEV_DMA_DRIVER_OPS,
	.name = "infinigpu",
	.desc = "infinigpu paravirtual display (Phase-0)",
	.major = 1,
	.minor = 0,
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

	/* Prove the KMS ring path deterministically before fbcon starts flushing. */
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
