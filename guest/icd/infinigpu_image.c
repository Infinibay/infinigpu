/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * VkImage / VkImageView / VkBuffer + their memory requirements and binds.
 * Images are single-plane LINEAR: the row pitch is PACKED (width * blocksize) so
 * it matches exactly what the host DMA-writes back (tightly packed rows) into the
 * dumb buffer bound to the image. Cribbed from lavapipe (lvp_image.c/lvp_device.c).
 */

#include "infinigpu_private.h"

#include "util/u_math.h"
#include "vk_format.h"
#include "vk_log.h"
#include "vk_util.h"

/* Memory-requirements alignment (the offset a VkDeviceMemory bind must satisfy).
 * Distinct from the row pitch, which stays packed for the host writeback. */
#define INFINIGPU_MEM_ALIGN 256

static VkResult
infinigpu_image_create(VkDevice _device, const VkImageCreateInfo *pCreateInfo,
                       const VkAllocationCallbacks *pAllocator, VkImage *pImage)
{
   VK_FROM_HANDLE(infinigpu_device, device, _device);
   struct infinigpu_image *image =
      vk_image_create(&device->vk, pCreateInfo, pAllocator, sizeof(*image));
   if (!image)
      return vk_error(device, VK_ERROR_OUT_OF_HOST_MEMORY);

   const uint32_t bpp = vk_format_get_blocksize(pCreateInfo->format);
   /* Packed rows: pitch == width*bpp so the host's tightly-packed DMA writeback
    * lands exactly, and GetImageSubresourceLayout reports the same stride. */
   image->row_pitch = pCreateInfo->extent.width * bpp;
   image->alignment = INFINIGPU_MEM_ALIGN;
   image->size = align64((uint64_t)image->row_pitch * pCreateInfo->extent.height *
                            pCreateInfo->arrayLayers,
                         INFINIGPU_MEM_ALIGN);

   IGPU_TRACE("CreateImage: format=%d %ux%u usage=0x%x tiling=%d -> size=%llu",
              pCreateInfo->format, pCreateInfo->extent.width, pCreateInfo->extent.height,
              pCreateInfo->usage, pCreateInfo->tiling, (unsigned long long)image->size);
   *pImage = infinigpu_image_to_handle(image);
   return VK_SUCCESS;
}

/* Resolve a swapchain + index to the driver's real (WSI-created) VkImage. */
static struct infinigpu_image *
infinigpu_swapchain_get_image(VkSwapchainKHR swapchain, uint32_t index)
{
   return infinigpu_image_from_handle(wsi_common_get_image(swapchain, index));
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreateImage(VkDevice _device, const VkImageCreateInfo *pCreateInfo,
                      const VkAllocationCallbacks *pAllocator, VkImage *pImage)
{
   /* WSI deferred-alloc path: an app may create an image tagged with
    * VkImageSwapchainCreateInfoKHR and later bind it onto a swapchain image
    * (VkBindImageMemorySwapchainInfoKHR, handled in BindImageMemory2). Build it
    * with the layout the WSI implicitly chose: LINEAR (infinigpu is always
    * packed-linear anyway), single-sample, color-attachment-usable. Matches
    * lavapipe's lvp_image_from_swapchain, adapted to our linear image model. */
   const VkImageSwapchainCreateInfoKHR *swapchain_info =
      vk_find_struct_const(pCreateInfo->pNext, IMAGE_SWAPCHAIN_CREATE_INFO_KHR);
   if (swapchain_info && swapchain_info->swapchain != VK_NULL_HANDLE) {
      VkImageCreateInfo local = *pCreateInfo;
      local.pNext = NULL;
      local.tiling = VK_IMAGE_TILING_LINEAR;
      local.samples = VK_SAMPLE_COUNT_1_BIT;
      local.usage |= VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
      return infinigpu_image_create(_device, &local, pAllocator, pImage);
   }

   return infinigpu_image_create(_device, pCreateInfo, pAllocator, pImage);
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_DestroyImage(VkDevice _device, VkImage _image,
                       const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(infinigpu_device, device, _device);
   VK_FROM_HANDLE(infinigpu_image, image, _image);

   if (!image)
      return;
   vk_image_destroy(&device->vk, pAllocator, &image->vk);
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_GetImageMemoryRequirements2(VkDevice _device,
                                      const VkImageMemoryRequirementsInfo2 *pInfo,
                                      VkMemoryRequirements2 *pMemoryRequirements)
{
   VK_FROM_HANDLE(infinigpu_image, image, pInfo->image);

   pMemoryRequirements->memoryRequirements.memoryTypeBits = 1;
   pMemoryRequirements->memoryRequirements.size = image->size;
   pMemoryRequirements->memoryRequirements.alignment = image->alignment;

   vk_foreach_struct(ext, pMemoryRequirements->pNext) {
      if (ext->sType == VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS) {
         VkMemoryDedicatedRequirements *ded = (void *)ext;
         /* Require a dedicated allocation so every image is bound at offset 0 of its own BO.
          * Interim fix: the host writeback (DMA to the scanout / render target) uses the BO base
          * address and ignores VkBindImageMemoryInfo::memoryOffset, while the read paths add it —
          * so a sub-allocated image (VMA binds many images into one VkDeviceMemory at nonzero
          * offsets by default) would get the GPU result written at offset 0, clobbering a neighbour
          * and reading black. Forcing dedicated allocation makes the offset-0 invariant hold. The
          * real fix threads memoryOffset through the SUBMIT_FORWARDED ioctl (ABI bump); until then
          * this trades a little VRAM for correctness. */
         ded->prefersDedicatedAllocation = true;
         ded->requiresDedicatedAllocation = true;
      }
   }
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_BindImageMemory2(VkDevice _device, uint32_t bindInfoCount,
                           const VkBindImageMemoryInfo *pBindInfos)
{
   for (uint32_t i = 0; i < bindInfoCount; i++) {
      VK_FROM_HANDLE(infinigpu_image, image, pBindInfos[i].image);
      VK_FROM_HANDLE(infinigpu_device_memory, mem, pBindInfos[i].memory);

      /* WSI deferred-alloc bind: alias this image onto a swapchain image's
       * backing memory instead of `mem` (which is VK_NULL_HANDLE here). The
       * WSI already allocated + bound that swapchain image via our normal path,
       * so it has a live ->mem; we just point at the same BO/offset. */
      const VkBindImageMemorySwapchainInfoKHR *sw =
         vk_find_struct_const(pBindInfos[i].pNext,
                              BIND_IMAGE_MEMORY_SWAPCHAIN_INFO_KHR);
      if (sw && sw->swapchain != VK_NULL_HANDLE) {
         struct infinigpu_image *backing =
            infinigpu_swapchain_get_image(sw->swapchain, sw->imageIndex);
         image->mem = backing->mem;
         image->mem_offset = backing->mem_offset;
      } else {
         image->mem = mem;                           /* the only backing step */
         image->mem_offset = pBindInfos[i].memoryOffset;
      }

      const VkBindMemoryStatusKHR *status =
         vk_find_struct_const(pBindInfos[i].pNext, BIND_MEMORY_STATUS_KHR);
      if (status && status->pResult)
         *status->pResult = VK_SUCCESS;
   }
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_GetImageSubresourceLayout(VkDevice _device, VkImage _image,
                                    const VkImageSubresource *pSubresource,
                                    VkSubresourceLayout *pLayout)
{
   VK_FROM_HANDLE(infinigpu_image, image, _image);

   pLayout->offset = 0;
   pLayout->rowPitch = image->row_pitch;
   pLayout->size = image->size;
   pLayout->depthPitch = 0;
   pLayout->arrayPitch = image->vk.array_layers > 1
                            ? (uint64_t)image->row_pitch * image->vk.extent.height
                            : 0;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreateImageView(VkDevice _device, const VkImageViewCreateInfo *pCreateInfo,
                          const VkAllocationCallbacks *pAllocator, VkImageView *pView)
{
   VK_FROM_HANDLE(infinigpu_device, device, _device);
   struct infinigpu_image_view *view =
      vk_image_view_create(&device->vk, false, pCreateInfo, pAllocator, sizeof(*view));
   if (!view)
      return vk_error(device, VK_ERROR_OUT_OF_HOST_MEMORY);

   view->image = infinigpu_image_from_handle(pCreateInfo->image);
   *pView = infinigpu_image_view_to_handle(view);
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_DestroyImageView(VkDevice _device, VkImageView _iview,
                           const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(infinigpu_device, device, _device);
   VK_FROM_HANDLE(infinigpu_image_view, iview, _iview);

   if (!iview)
      return;
   vk_image_view_destroy(&device->vk, pAllocator, &iview->vk);
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreateBuffer(VkDevice _device, const VkBufferCreateInfo *pCreateInfo,
                       const VkAllocationCallbacks *pAllocator, VkBuffer *pBuffer)
{
   VK_FROM_HANDLE(infinigpu_device, device, _device);
   struct infinigpu_buffer *buffer =
      vk_buffer_create(&device->vk, pCreateInfo, pAllocator, sizeof(*buffer));
   if (!buffer)
      return vk_error(device, VK_ERROR_OUT_OF_HOST_MEMORY);

   buffer->total_size = pCreateInfo->size;
   *pBuffer = infinigpu_buffer_to_handle(buffer);
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_DestroyBuffer(VkDevice _device, VkBuffer _buffer,
                        const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(infinigpu_device, device, _device);
   VK_FROM_HANDLE(infinigpu_buffer, buffer, _buffer);

   if (!buffer)
      return;
   vk_buffer_destroy(&device->vk, pAllocator, &buffer->vk);
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_BindBufferMemory2(VkDevice _device, uint32_t bindInfoCount,
                            const VkBindBufferMemoryInfo *pBindInfos)
{
   for (uint32_t i = 0; i < bindInfoCount; i++) {
      VK_FROM_HANDLE(infinigpu_buffer, buffer, pBindInfos[i].buffer);
      VK_FROM_HANDLE(infinigpu_device_memory, mem, pBindInfos[i].memory);

      buffer->mem = mem;
      buffer->offset = pBindInfos[i].memoryOffset;
      /* `mem` may be VK_NULL_HANDLE (→ NULL here); guard the deref so a null-memory bind
       * can't segfault the ICD (Phase-1 audit). */
      buffer->map = (mem && mem->map) ? (char *)mem->map + pBindInfos[i].memoryOffset : NULL;

      const VkBindMemoryStatusKHR *status =
         vk_find_struct_const(pBindInfos[i].pNext, BIND_MEMORY_STATUS_KHR);
      if (status && status->pResult)
         *status->pResult = VK_SUCCESS;
   }
   return VK_SUCCESS;
}

/* Implementing this backfills BOTH GetBufferMemoryRequirements2 and the non-2
 * form (vk_common delegates to it). */
VKAPI_ATTR void VKAPI_CALL
infinigpu_GetDeviceBufferMemoryRequirements(VkDevice _device,
                                            const VkDeviceBufferMemoryRequirements *pInfo,
                                            VkMemoryRequirements2 *pMemoryRequirements)
{
   pMemoryRequirements->memoryRequirements.memoryTypeBits = 1;
   pMemoryRequirements->memoryRequirements.alignment = INFINIGPU_MEM_ALIGN;
   pMemoryRequirements->memoryRequirements.size =
      align64(pInfo->pCreateInfo->size, INFINIGPU_MEM_ALIGN);
}
