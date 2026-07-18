/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * VkDevice + VkQueue.  Cribbed from lavapipe (lvp_CreateDevice /
 * lvp_queue_init / lvp_queue_submit).  Phase 0: immediate submit mode, the
 * driver_submit hook is a no-op that reports success.
 */

#include "infinigpu_private.h"

#include "vk_alloc.h"
#include "vk_log.h"

VkResult
infinigpu_queue_submit(struct vk_queue *vk_queue,
                       struct vk_queue_submit *submit)
{
   /* Phase 0: nothing is executed on the (remote) GPU yet. */
   return VK_SUCCESS;
}

static VkResult
infinigpu_queue_init(struct infinigpu_device *device,
                     struct infinigpu_queue *queue,
                     const VkDeviceQueueCreateInfo *create_info,
                     uint32_t index_in_family)
{
   /* vk_queue_init: src/vulkan/runtime/vk_queue.h
    *   (queue, device, pCreateInfo, index_in_family) */
   VkResult result = vk_queue_init(&queue->vk, &device->vk, create_info,
                                   index_in_family);
   if (result != VK_SUCCESS)
      return result;

   queue->device = device;
   queue->vk.driver_submit = infinigpu_queue_submit;
   return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreateDevice(VkPhysicalDevice physicalDevice,
                       const VkDeviceCreateInfo *pCreateInfo,
                       const VkAllocationCallbacks *pAllocator,
                       VkDevice *pDevice)
{
   VK_FROM_HANDLE(infinigpu_physical_device, physical_device, physicalDevice);
   struct infinigpu_device *device;

   assert(pCreateInfo->sType == VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO);

   device = vk_zalloc2(&physical_device->vk.instance->alloc, pAllocator,
                       sizeof(*device), 8,
                       VK_SYSTEM_ALLOCATION_SCOPE_DEVICE);
   if (!device)
      return vk_error(physical_device, VK_ERROR_OUT_OF_HOST_MEMORY);

   struct vk_device_dispatch_table dispatch_table;
   vk_device_dispatch_table_from_entrypoints(
      &dispatch_table, &infinigpu_device_entrypoints, true);

   /* vk_device_init: src/vulkan/runtime/vk_device.h
    *   (device, physical_device, dispatch_table, pCreateInfo, alloc).
    * Checks enabled extensions/features against the physical device. */
   VkResult result = vk_device_init(&device->vk, &physical_device->vk,
                                    &dispatch_table, pCreateInfo, pAllocator);
   if (result != VK_SUCCESS) {
      vk_free2(&physical_device->vk.instance->alloc, pAllocator, device);
      return result;
   }

   device->physical_device = physical_device;

   /* Phase 0 expects a single queue in family 0 (as vulkaninfo requests). */
   assert(pCreateInfo->queueCreateInfoCount >= 1);
   result = infinigpu_queue_init(device, &device->queue,
                                 &pCreateInfo->pQueueCreateInfos[0], 0);
   if (result != VK_SUCCESS) {
      vk_device_finish(&device->vk);
      vk_free2(&physical_device->vk.instance->alloc, pAllocator, device);
      return result;
   }

   *pDevice = infinigpu_device_to_handle(device);
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_DestroyDevice(VkDevice _device,
                        const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(infinigpu_device, device, _device);

   if (!device)
      return;

   vk_queue_finish(&device->queue.vk);
   vk_device_finish(&device->vk);
   vk_free2(&device->vk.alloc, pAllocator, device);
}
