// SPDX-License-Identifier: GPL-2.0
//
// infinigpu — Linux guest DRM/KMS display driver (Phase-0, ADR-0005 Linux).
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
#include <drm/drm_atomic_helper.h>
#include <drm/drm_atomic_state_helper.h>
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
#define ENC_DISPLAY_SCANOUT  0x0101u
#define WIRE_FMT_XRGB8888    3u  /* = wire::format::B8G8R8X8; XRGB8888 LE = [B,G,R,X] */
#define DESC_FLAG_FENCED     0x1u

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

struct igpu_device {
	struct drm_device drm;
	struct drm_simple_display_pipe pipe;
	struct drm_connector connector;

	void __iomem *bar0;
	void *ring;            /* coherent: descriptor + submit_cmd + payload */
	dma_addr_t ring_dma;
	struct mutex ring_lock; /* serialises submissions (selftest vs fbcon) */
	u64 seqno;
};

#define to_igpu(d) container_of(d, struct igpu_device, drm)

static const u32 igpu_formats[] = { DRM_FORMAT_XRGB8888 };

/* ---- device submission: hand the host one framebuffer to scan out ---- */

static u32 igpu_submit_scanout(struct igpu_device *idev, dma_addr_t fb, u32 w,
			       u32 h, u32 pitch, u32 fmt)
{
	struct igpu_descriptor *d = idev->ring;
	struct igpu_submit_cmd *s = idev->ring + sizeof(*d);
	struct igpu_scanout_present *p = idev->ring + sizeof(*d) + sizeof(*s);
	u32 retired;
	u64 seq;

	mutex_lock(&idev->ring_lock);
	seq = ++idev->seqno;

	d->msg_type = MSG_SUBMIT_CMD;
	d->flags = DESC_FLAG_FENCED;
	d->len = sizeof(*p);
	d->data_offset = sizeof(*d) + sizeof(*s);
	d->seqno = seq;
	d->reserved = 0;

	s->ctx_id = 0;
	s->encoding = ENC_DISPLAY_SCANOUT;
	s->payload_len = sizeof(*p);
	s->flags = 0;
	s->seqno = seq;
	s->in_fence = 0;
	s->out_fence = seq;

	p->width = w;
	p->height = h;
	p->pitch = pitch;
	p->format = fmt;
	p->scanout_addr = fb;

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

	igpu_flush(to_igpu(crtc->dev), pipe->plane.state);

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
	.fb_create = drm_gem_fb_create,
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

	ret = drm_simple_display_pipe_init(drm, &idev->pipe, &igpu_pipe_funcs,
					   igpu_formats, ARRAY_SIZE(igpu_formats),
					   NULL, &idev->connector);
	if (ret)
		return ret;

	drm_mode_config_reset(drm);

	ret = drm_dev_register(drm, 0);
	if (ret)
		return ret;

	/* Prove the KMS ring path deterministically before fbcon starts flushing. */
	igpu_kms_selftest(idev);

	/* Bring up fbdev emulation → fbcon renders the console on our framebuffer. */
	drm_client_setup(drm, NULL);

	dev_info(&pdev->dev, "INFINIGPU-KMS: registered /dev/dri/card%d\n", drm->primary->index);
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

MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("infinigpu guest DRM/KMS display driver (Phase-0)");
MODULE_AUTHOR("Infinibay");
