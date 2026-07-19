/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * DRM/KMD helpers (see infinigpu_kmd.h). Pure ioctl over the infinigpu render
 * node — no Mesa dependency, so this file compiles standalone against libdrm.
 */

#include "infinigpu_kmd.h"

#include <errno.h>
#include <string.h>
#include <sys/mman.h>

#include <xf86drm.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

#include "infinigpu_drm.h"   /* DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED + the args struct */

int
infinigpu_dumb_alloc(int fd, uint64_t size, uint32_t *handle, uint64_t *actual_size)
{
   struct drm_mode_create_dumb create = {
      /* A flat byte region: 32bpp keeps every DRM helper happy; width rounds the
       * size up to the next 4 bytes (pitch = width*4 ≥ size), height 1. */
      .width = (uint32_t)((size + 3) / 4),
      .height = 1,
      .bpp = 32,
   };

   if (size == 0 || create.width == 0)
      return -EINVAL;

   int ret = drmIoctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &create);
   if (ret != 0)
      return -errno;

   *handle = create.handle;
   *actual_size = create.size;   /* pitch*height, ≥ the requested size */
   return 0;
}

void *
infinigpu_dumb_map(int fd, uint32_t handle, uint64_t size)
{
   struct drm_mode_map_dumb map = { .handle = handle };

   if (drmIoctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map) != 0)
      return NULL;

   void *ptr = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
                    (off_t)map.offset);
   if (ptr == MAP_FAILED)
      return NULL;
   return ptr;
}

void
infinigpu_dumb_unmap(void *ptr, uint64_t size)
{
   if (ptr)
      munmap(ptr, size);
}

void
infinigpu_gem_close(int fd, uint32_t handle)
{
   struct drm_gem_close close = { .handle = handle };
   drmIoctl(fd, DRM_IOCTL_GEM_CLOSE, &close);
}

int
infinigpu_submit_forwarded(int fd, uint32_t bo_handle, uint32_t width, uint32_t height,
                           const void *payload, uint32_t payload_len)
{
   struct drm_infinigpu_submit_forwarded args;

   memset(&args, 0, sizeof(args));
   args.bo_handle = bo_handle;
   args.width = width;
   args.height = height;
   args.payload_len = payload_len;
   args.payload_ptr = (uint64_t)(uintptr_t)payload;

   if (drmIoctl(fd, DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED, &args) != 0)
      return -errno;
   return 0;
}
