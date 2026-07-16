// SPDX-License-Identifier: GPL-2.0
//
// infinigpu — Linux guest PCI driver (Phase-0 bring-up).
//
// Binds the infinigpu vfio-user device (1b36:0110) and, in probe(), runs a
// self-test that exercises the whole guest→host→GPU path from inside a real
// kernel: map BAR0, verify DEV_MAGIC, build a one-entry command ring in coherent
// DMA memory, submit a DISPLAY_CLEAR, ring the doorbell, and confirm the host
// rendered on the physical GPU and DMA-wrote the frame back into our buffer.
//
// This is intentionally a plain PCI driver, not a DRM/KMS display driver — the
// modeset/framebuffer layer (ADR-0005 M1) is a later refinement. The register and
// wire layout mirror crates/infinigpu-abi (kept in sync manually for now; a
// cbindgen header will make it automatic).

#include <linux/module.h>
#include <linux/pci.h>
#include <linux/dma-mapping.h>
#include <linux/delay.h>
#include <linux/io.h>

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
#define ENC_DISPLAY_CLEAR    0x0100u
#define DESC_FLAG_FENCED     0x1u

/* IEEE-754 bit patterns for the clear colour {0.0, 0.6, 0.8, 1.0}
 * (kernel code avoids the FPU) → 8-bit {0, 153, 204, 255}. */
#define F_0_0 0x00000000u
#define F_0_6 0x3f19999au
#define F_0_8 0x3f4ccccdu
#define F_1_0 0x3f800000u

#define W 256
#define H 256
#define SCANOUT_OFF 0x40000            /* 256 KiB */
#define BUFSZ (SCANOUT_OFF + W * H * 4) /* ring area + scanout */

struct igpu_descriptor {
	u32 msg_type, flags, len, data_offset;
	u64 seqno, reserved;
};
struct igpu_submit_cmd {
	u32 ctx_id, encoding, payload_len, flags;
	u64 seqno, in_fence, out_fence;
};
struct igpu_clear_present {
	u32 width, height;
	u32 rgba[4]; /* raw IEEE-754 bits */
	u64 scanout_addr;
};

struct igpu {
	void __iomem *bar0;
	void *buf;
	dma_addr_t dma;
};

static int igpu_selftest(struct pci_dev *pdev, struct igpu *g)
{
	struct igpu_descriptor *d = g->buf;
	struct igpu_submit_cmd *s = g->buf + sizeof(*d);
	struct igpu_clear_present *c = g->buf + sizeof(*d) + sizeof(*s);
	u8 *px = (u8 *)g->buf + SCANOUT_OFF;
	u32 retired = 0;
	int i;

	d->msg_type = MSG_SUBMIT_CMD;
	d->flags = DESC_FLAG_FENCED;
	d->len = sizeof(*c);
	d->data_offset = sizeof(*d) + sizeof(*s);
	d->seqno = 1;
	d->reserved = 0;

	s->ctx_id = 0;
	s->encoding = ENC_DISPLAY_CLEAR;
	s->payload_len = sizeof(*c);
	s->flags = 0;
	s->seqno = 1;
	s->in_fence = 0;
	s->out_fence = 1;

	c->width = W;
	c->height = H;
	c->rgba[0] = F_0_0;
	c->rgba[1] = F_0_6;
	c->rgba[2] = F_0_8;
	c->rgba[3] = F_1_0;
	c->scanout_addr = g->dma + SCANOUT_OFF;

	wmb(); /* ring visible before the doorbell */

	iowrite32(lower_32_bits(g->dma), g->bar0 + REG_CMD_RING_BASE_LO);
	iowrite32(upper_32_bits(g->dma), g->bar0 + REG_CMD_RING_BASE_HI);
	iowrite32(1, g->bar0 + REG_DOORBELL_CMD0);

	/* the device processes the doorbell synchronously; poll defensively */
	for (i = 0; i < 1000; i++) {
		retired = ioread32(g->bar0 + REG_RETIRED_LO);
		if (retired >= 1)
			break;
		udelay(100);
	}

	if (retired >= 1 && px[0] == 0 && px[1] == 153 && px[2] == 204 &&
	    px[3] == 255) {
		dev_info(&pdev->dev,
			 "INFINIGPU-SELFTEST: PASS retired=%u scanout[0]=[%u,%u,%u,%u]\n",
			 retired, px[0], px[1], px[2], px[3]);
		return 0;
	}

	dev_err(&pdev->dev,
		"INFINIGPU-SELFTEST: FAIL retired=%u scanout[0]=[%u,%u,%u,%u]\n",
		retired, px[0], px[1], px[2], px[3]);
	return -EIO;
}

static int igpu_probe(struct pci_dev *pdev, const struct pci_device_id *id)
{
	struct igpu *g;
	u32 magic, abi, caps;
	int ret;

	BUILD_BUG_ON(sizeof(struct igpu_descriptor) != 32);
	BUILD_BUG_ON(sizeof(struct igpu_submit_cmd) != 40);
	BUILD_BUG_ON(sizeof(struct igpu_clear_present) != 32);

	g = devm_kzalloc(&pdev->dev, sizeof(*g), GFP_KERNEL);
	if (!g)
		return -ENOMEM;

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
	g->bar0 = pcim_iomap_table(pdev)[0];

	magic = ioread32(g->bar0 + REG_DEV_MAGIC);
	abi = ioread32(g->bar0 + REG_ABI_VERSION);
	caps = ioread32(g->bar0 + REG_DEV_CAPS);
	dev_info(&pdev->dev, "infinigpu magic=%#x abi=%#x caps=%#x\n", magic,
		 abi, caps);
	if (magic != DEV_MAGIC) {
		dev_err(&pdev->dev, "bad magic (not an infinigpu device)\n");
		return -ENODEV;
	}

	iowrite32(GLOBAL_CTRL_ENABLE, g->bar0 + REG_GLOBAL_CTRL);

	g->buf = dma_alloc_coherent(&pdev->dev, BUFSZ, &g->dma, GFP_KERNEL);
	if (!g->buf)
		return -ENOMEM;

	pci_set_drvdata(pdev, g);
	return igpu_selftest(pdev, g);
}

static void igpu_remove(struct pci_dev *pdev)
{
	struct igpu *g = pci_get_drvdata(pdev);

	if (g && g->buf)
		dma_free_coherent(&pdev->dev, BUFSZ, g->buf, g->dma);
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
};
module_pci_driver(igpu_driver);

MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("infinigpu guest PCI driver (Phase-0 bring-up)");
MODULE_AUTHOR("Infinibay");
