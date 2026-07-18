// SPDX-License-Identifier: (GPL-2.0-only OR MIT)
//
// submit3d_test — a minimal userspace submitter for the infinigpu render node, standing in for
// the (future) thin Vulkan ICD. It drives the own-remoting 3D datapath end-to-end from guest
// userspace: open the DRM node, allocate a result buffer, ask the host to replay a TRIANGLE
// workload on the physical GPU (DRM_IOCTL_INFINIGPU_SUBMIT3D), then map the buffer and confirm the
// GPU-rendered pixels came back. Static-linked so it runs in the harness's busybox initramfs.
//
// Prints exactly one result line the harness greps:  "SUBMIT3D: PASS lit=N ..."  or  "SUBMIT3D: FAIL ...".

#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>
#include "infinigpu_drm.h"

static uint32_t f32_bits(float f)
{
	uint32_t u;
	memcpy(&u, &f, sizeof(u));
	return u;
}

int main(int argc, char **argv)
{
	const char *node = argc > 1 ? argv[1] : "/dev/dri/card0";
	const uint32_t w = 64, h = 64;
	int fd, r;

	fd = open(node, O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		printf("SUBMIT3D: FAIL open(%s): %s\n", node, strerror(errno));
		return 1;
	}

	/* Allocate a w*h*4 result buffer (dumb buffer = a drm_gem_dma object → a single dma_addr). */
	struct drm_mode_create_dumb cd = { .width = w, .height = h, .bpp = 32 };
	if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &cd)) {
		printf("SUBMIT3D: FAIL create_dumb: %s\n", strerror(errno));
		return 1;
	}

	/* Ask the host to replay a shader-executed triangle on the physical GPU into that buffer. */
	struct drm_infinigpu_submit3d req = {
		.bo_handle = cd.handle,
		.op = INFINIGPU_VK_OP_TRIANGLE,
		.width = w,
		.height = h,
		.bg = { f32_bits(0.02f), f32_bits(0.02f), f32_bits(0.05f), f32_bits(1.0f) },
	};
	r = ioctl(fd, DRM_IOCTL_INFINIGPU_SUBMIT3D, &req);
	if (r) {
		printf("SUBMIT3D: FAIL ioctl SUBMIT3D: %s\n", strerror(errno));
		return 1;
	}

	/* Map the buffer and confirm GPU-rendered pixels came back over the device's DMA path. */
	struct drm_mode_map_dumb md = { .handle = cd.handle };
	if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &md)) {
		printf("SUBMIT3D: FAIL map_dumb: %s\n", strerror(errno));
		return 1;
	}
	uint8_t *px = mmap(NULL, cd.size, PROT_READ, MAP_SHARED, fd, md.offset);
	if (px == MAP_FAILED) {
		printf("SUBMIT3D: FAIL mmap: %s\n", strerror(errno));
		return 1;
	}

	unsigned lit = 0;
	for (uint32_t i = 0; i < w * h; i++) {
		const uint8_t *p = px + (size_t)i * 4;
		if ((unsigned)p[0] + p[1] + p[2] > 96)
			lit++;
	}
	munmap(px, cd.size);

	if (lit == 0) {
		printf("SUBMIT3D: FAIL no lit pixels (host render did not reach the guest buffer)\n");
		return 1;
	}
	printf("SUBMIT3D: PASS lit=%u/%u (host replayed a Vulkan TRIANGLE on the GPU → guest buffer)\n",
	       lit, w * h);
	return 0;
}
