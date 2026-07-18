/* SPDX-License-Identifier: (GPL-2.0-only OR MIT) */
/*
 * infinigpu render-node uAPI — the 3D command-submission ioctl.
 *
 * Shared by the guest kernel driver (guest/linux/infinigpu.c) and userspace (the thin
 * ICD / test submitter). A submit names a hand-rolled Vulkan workload (`op`) and a GEM
 * buffer to receive the R8G8B8A8 result; the driver wraps it as a SUBMIT_CMD
 * {VULKAN_VENUSLIKE} on the command ring and the host replays it on the physical GPU
 * (see crates/infinigpu-device submit_vulkan + wire::VulkanWorkload).
 */
#ifndef _UAPI_INFINIGPU_DRM_H
#define _UAPI_INFINIGPU_DRM_H

#if defined(__KERNEL__)
#include <uapi/drm/drm.h>
#else
#include <drm/drm.h>
#endif

/* `op` values — mirror infinigpu-abi wire::vk_op. */
#define INFINIGPU_VK_OP_CLEAR    0
#define INFINIGPU_VK_OP_TRIANGLE 1

/*
 * DRM_IOCTL_INFINIGPU_SUBMIT3D argument. `bg` is 4×f32 clear/background bits (kept as
 * __u32 so the kernel passes them through without touching the FPU). The rendered
 * `width`×`height` R8G8B8A8 image is written into `bo_handle` (a GEM/dumb buffer whose
 * size must be ≥ width*height*4). Blocks until the host has replayed the workload.
 */
struct drm_infinigpu_submit3d {
	__u32 bo_handle;
	__u32 op;
	__u32 width;
	__u32 height;
	__u32 bg[4];
};

#define DRM_INFINIGPU_SUBMIT3D 0x00
#define DRM_IOCTL_INFINIGPU_SUBMIT3D \
	DRM_IOWR(DRM_COMMAND_BASE + DRM_INFINIGPU_SUBMIT3D, struct drm_infinigpu_submit3d)

#endif /* _UAPI_INFINIGPU_DRM_H */
