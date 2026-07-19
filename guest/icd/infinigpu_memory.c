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

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_AllocateMemory(VkDevice _device,
                         const VkMemoryAllocateInfo *pAllocateInfo,
                         const VkAllocationCallbacks *pAllocator,
                         VkDeviceMemory *pMemory)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   struct infinigpu_device_memory *mem;
   int drm_fd = dev->physical_device->drm_fd;

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
      /* Smoke/dev host without a real infinigpu node: no backing possible. */
      vk_device_memory_destroy(&dev->vk, pAllocator, &mem->vk);
      return vk_error(dev, VK_ERROR_OUT_OF_DEVICE_MEMORY);
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

   if (mem->map)
      infinigpu_dumb_unmap(mem->map, mem->map_size);
   if (dev->physical_device->drm_fd >= 0)
      infinigpu_gem_close(dev->physical_device->drm_fd, mem->gem_handle);
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
