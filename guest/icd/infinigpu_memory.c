/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * VkDeviceMemory: a DRM "dumb" buffer (contiguous, DMA-backed, mmappable) is the
 * storage for every allocation. The color image lands in one of these; at submit
 * the KMD resolves its dma_addr from the GEM handle and the host DMA-writes the
 * render into it. Signatures cribbed from lavapipe (lvp_device.c) and nvk.
 */

#include "infinigpu_private.h"
#include "infinigpu_kmd.h"

#include "vk_log.h"

#include <stdlib.h>

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_AllocateMemory(VkDevice _device,
                         const VkMemoryAllocateInfo *pAllocateInfo,
                         const VkAllocationCallbacks *pAllocator,
                         VkDeviceMemory *pMemory)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   struct infinigpu_device_memory *mem;
   int drm_fd = dev->physical_device->drm_fd;

   IGPU_TRACE("AllocateMemory: size=%llu typeIndex=%u",
              (unsigned long long)pAllocateInfo->allocationSize,
              pAllocateInfo->memoryTypeIndex);

   /* allocationSize==0 is legal — vk_device_memory_create would assert. */
   if (pAllocateInfo->allocationSize == 0) {
      *pMemory = VK_NULL_HANDLE;
      return VK_SUCCESS;
   }

   /* vk_device_memory_create zallocs the whole struct and auto-parses the alloc
    * pNext (size/type_index/flags/import+export handle types). Do not re-parse. */
   mem = vk_device_memory_create(&dev->vk, pAllocateInfo, pAllocator, sizeof(*mem));
   if (!mem)
      return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);

   if (drm_fd < 0) {
      /* Smoke/dev host without a real infinigpu DRM node (drm_fd < 0 only ever
       * happens under INFINIGPU_SMOKE_ANY_NODE — a production guest either has the
       * node or enumerates no device). Back the allocation with plain host memory
       * so every CPU-mapped path still works for bring-up: WSI software present
       * (memcpy from the map), buffer reads, readback. A forwarded DRAW still needs
       * a real node (no dma_addr), but the whole swapchain/present path is testable.
       * gem_handle stays 0; FreeMemory frees() rather than munmap+gem_close. */
      mem->map = malloc(pAllocateInfo->allocationSize);
      if (!mem->map) {
         vk_device_memory_destroy(&dev->vk, pAllocator, &mem->vk);
         return vk_error(dev, VK_ERROR_OUT_OF_DEVICE_MEMORY);
      }
      mem->map_size = pAllocateInfo->allocationSize;
      mem->gem_handle = 0;
      *pMemory = infinigpu_device_memory_to_handle(mem);
      return VK_SUCCESS;
   }

   if (infinigpu_dumb_alloc(drm_fd, pAllocateInfo->allocationSize,
                            &mem->gem_handle, &mem->map_size) != 0) {
      vk_device_memory_destroy(&dev->vk, pAllocator, &mem->vk);
      return vk_error(dev, VK_ERROR_OUT_OF_DEVICE_MEMORY);
   }

   mem->map = infinigpu_dumb_map(drm_fd, mem->gem_handle, mem->map_size);
   if (!mem->map) {
      infinigpu_gem_close(drm_fd, mem->gem_handle);
      vk_device_memory_destroy(&dev->vk, pAllocator, &mem->vk);
      return vk_error(dev, VK_ERROR_MEMORY_MAP_FAILED);
   }

   *pMemory = infinigpu_device_memory_to_handle(mem);
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_FreeMemory(VkDevice _device, VkDeviceMemory _mem,
                     const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   VK_FROM_HANDLE(infinigpu_device_memory, mem, _mem);

   if (!mem)
      return;

   if (dev->physical_device->drm_fd < 0) {
      /* Smoke host-malloc fallback (see AllocateMemory): plain free, no DRM. */
      free(mem->map);
   } else {
      if (mem->map)
         infinigpu_dumb_unmap(mem->map, mem->map_size);
      infinigpu_gem_close(dev->physical_device->drm_fd, mem->gem_handle);
   }
   vk_device_memory_destroy(&dev->vk, pAllocator, &mem->vk);
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_MapMemory2KHR(VkDevice _device,
                        const VkMemoryMapInfoKHR *pMemoryMapInfo,
                        void **ppData)
{
   VK_FROM_HANDLE(infinigpu_device_memory, mem, pMemoryMapInfo->memory);

   if (mem == NULL) {
      *ppData = NULL;
      return VK_SUCCESS;
   }
   /* The whole BO is mmapped for its lifetime; just offset into it. */
   *ppData = (char *)mem->map + pMemoryMapInfo->offset;
   return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_UnmapMemory2KHR(VkDevice _device,
                          const VkMemoryUnmapInfoKHR *pMemoryUnmapInfo)
{
   /* Persistent mapping — unmapped only at FreeMemory. */
   return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_FlushMappedMemoryRanges(VkDevice _device, uint32_t memoryRangeCount,
                                  const VkMappedMemoryRange *pMemoryRanges)
{
   /* Dumb buffers are host-coherent here. */
   return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_InvalidateMappedMemoryRanges(VkDevice _device, uint32_t memoryRangeCount,
                                       const VkMappedMemoryRange *pMemoryRanges)
{
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_GetDeviceMemoryCommitment(VkDevice _device, VkDeviceMemory _mem,
                                    VkDeviceSize *pCommittedMemoryInBytes)
{
   /* No lazily-allocated memory. */
   *pCommittedMemoryInBytes = 0;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_GetMemoryFdKHR(VkDevice _device,
                         const VkMemoryGetFdInfoKHR *pGetFdInfo,
                         int *pFd)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   VK_FROM_HANDLE(infinigpu_device_memory, mem, pGetFdInfo->memory);

   /* We only export dma-buf (the WSI DRM/display present path asks for exactly this
    * to page-flip a swapchain image onto the infinigpu scanout). The backing is a
    * drm_gem_dma dumb buffer; PRIME-export its GEM handle. */
   if (pGetFdInfo->handleType != VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT &&
       pGetFdInfo->handleType != VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT)
      return vk_error(dev, VK_ERROR_FEATURE_NOT_PRESENT);

   if (mem == NULL || mem->gem_handle == 0 || dev->physical_device->drm_fd < 0)
      return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);

   int fd = -1;
   int ret = infinigpu_prime_handle_to_fd(dev->physical_device->drm_fd,
                                          mem->gem_handle, &fd);
   if (ret != 0)
      return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);

   *pFd = fd;
   return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_GetMemoryFdPropertiesKHR(VkDevice _device,
                                   VkExternalMemoryHandleTypeFlagBits handleType,
                                   int fd,
                                   VkMemoryFdPropertiesKHR *pMemoryFdProperties)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);

   if (handleType != VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT)
      return vk_error(dev, VK_ERROR_INVALID_EXTERNAL_HANDLE);

   /* Single memory type (index 0) backs every allocation on this driver. */
   pMemoryFdProperties->memoryTypeBits = 1u;
   return VK_SUCCESS;
}
