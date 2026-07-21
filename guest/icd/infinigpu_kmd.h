/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * Thin DRM/KMD helpers the ICD uses to talk to the infinigpu render node
 * (guest/linux/infinigpu.c) — all pure ioctl, no Mesa dependency:
 *
 *   - a DRM "dumb" buffer as the contiguous, DMA-backed, mmappable storage for a
 *     VkDeviceMemory (the color image lands here; the host DMA-writes the render
 *     into its dma_addr, which the KMD resolves from the GEM handle at submit),
 *   - the INFINIGPU_SUBMIT_FORWARDED ioctl that replays a forwarded-draw body on
 *     the host GPU (guest/include/infinigpu_drm.h).
 */

#ifndef INFINIGPU_KMD_H
#define INFINIGPU_KMD_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Allocate a DRM dumb buffer of at least `size` bytes as a flat 1-row region
 * (width = ceil(size/4), height = 1, bpp = 32). Returns 0 and fills *handle /
 * *actual_size on success, or a negative errno. The buffer is a drm_gem_dma
 * object, so the KMD resolves its single dma_addr for the host writeback. */
int infinigpu_dumb_alloc(int fd, uint64_t size, uint32_t *handle, uint64_t *actual_size);

/* mmap a dumb buffer (via DRM_IOCTL_MODE_MAP_DUMB + mmap). Returns the mapping or
 * NULL. `size` must be the buffer's actual size (from infinigpu_dumb_alloc). */
void *infinigpu_dumb_map(int fd, uint32_t handle, uint64_t size);

/* munmap a previously mapped dumb buffer. */
void infinigpu_dumb_unmap(void *ptr, uint64_t size);

/* Close a GEM handle (frees the dumb buffer). */
void infinigpu_gem_close(int fd, uint32_t handle);

/* PRIME-export a GEM handle to a dma-buf fd (DRM_CLOEXEC|DRM_RDWR). The KMD is
 * drm_gem_dma, so PRIME export is supported. Returns 0 and fills *out_fd (caller
 * owns it — close()) or a negative errno. Backs vkGetMemoryFdKHR, which WSI's
 * DRM/display present path uses to page-flip a swapchain image onto the scanout. */
int infinigpu_prime_handle_to_fd(int fd, uint32_t handle, int *out_fd);

/* Issue DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED: replay `payload` (a forwarded-draw
 * body from infinigpu_encode_forwarded — VulkanWorkload + ForwardedDrawTail +
 * SPIR-V) on the host GPU, DMA-writing the width*height R8G8B8A8 result into
 * `bo_handle`. Blocks until the host retires. Returns 0 or a negative errno. */
int infinigpu_submit_forwarded(int fd, uint32_t bo_handle, uint32_t width, uint32_t height,
                               const void *payload, uint32_t payload_len);

#ifdef __cplusplus
}
#endif

#endif /* INFINIGPU_KMD_H */
