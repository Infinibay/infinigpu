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
#define INFINIGPU_VK_OP_FORWARDED 2   /* draw with the guest app's own forwarded SPIR-V */

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

/*
 * DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED argument — the thin Mesa ICD's submit path.
 *
 * Unlike SUBMIT3D (which names a built-in `op`), the caller supplies a pre-serialized
 * forwarded-draw body at `payload_ptr` (`payload_len` bytes): a wire::VulkanWorkload (40B)
 * followed by a ForwardedDrawTail (24B) and the guest app's own vertex+fragment SPIR-V and
 * entry-point names — exactly what guest/icd/infinigpu_forwarded.c emits. The driver wraps it
 * as a SUBMIT_CMD{VULKAN_VENUSLIKE} on the command ring, patching the workload's scanout_addr to
 * `bo_handle`'s DMA address (the host DMA-writes the rendered R8G8B8A8 there — so bo must be
 * ≥ width*height*4). The body is too large for the ring's inline payload slot, so it travels
 * out-of-line (PAYLOAD_ABS). Blocks until the host has replayed the draw. Write-only: the result
 * lands in `bo_handle`, not the args.
 */
struct drm_infinigpu_submit_forwarded {
	__u32 bo_handle;    /* target color image (GEM/dumb); host DMA-writes the render here */
	__u32 width;
	__u32 height;
	__u32 payload_len;  /* bytes at payload_ptr (VulkanWorkload + ForwardedDrawTail + SPIR-V blobs) */
	__u64 payload_ptr;  /* userspace pointer to the forwarded body (guest/icd/infinigpu_forwarded.c) */
};

#define DRM_INFINIGPU_SUBMIT_FORWARDED 0x01
#define DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED \
	DRM_IOW(DRM_COMMAND_BASE + DRM_INFINIGPU_SUBMIT_FORWARDED, struct drm_infinigpu_submit_forwarded)

#endif /* _UAPI_INFINIGPU_DRM_H */
