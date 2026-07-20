/*
 * Copyright 2026 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * Descriptors + samplers (Phase-2c textures/UBO, guest half). The ICD never runs
 * shaders, so these objects only CAPTURE the resources bound to a descriptor set
 * (a sampled image + sampler for now). At submit, infinigpu_sync.c reads the bound
 * set's image pixels from its host-mapped memory and forwards them in the command
 * list; the host binds them through its own real descriptor set.
 *
 * Descriptor-set LAYOUTs use the runtime `vk_descriptor_set_layout` object (ref-
 * counted); we store no per-binding state because vkUpdateDescriptorSets carries
 * the descriptorType directly. Samplers subclass the runtime `vk_sampler`, keeping
 * only the filter/address mode the wire needs. Pools + sets are driver-owned
 * (no vk_common backfill); a pool tracks its sets so reset/destroy frees them.
 */

#include "infinigpu_private.h"

#include "vk_alloc.h"
#include "vk_log.h"
#include "vk_object.h"

/* ------------------------------------------------------------ descriptor set layout */

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreateDescriptorSetLayout(VkDevice _device,
                                    const VkDescriptorSetLayoutCreateInfo *pCreateInfo,
                                    const VkAllocationCallbacks *pAllocator,
                                    VkDescriptorSetLayout *pSetLayout)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   /* Ref-counted runtime object; no driver state to fill (types come from the writes). */
   struct infinigpu_descriptor_set_layout *l =
      vk_descriptor_set_layout_zalloc(&dev->vk, sizeof(*l));
   if (!l)
      return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);

   *pSetLayout = vk_descriptor_set_layout_to_handle(&l->vk);
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_DestroyDescriptorSetLayout(VkDevice _device, VkDescriptorSetLayout _layout,
                                     const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   VK_FROM_HANDLE(vk_descriptor_set_layout, layout, _layout);
   if (!layout)
      return;
   vk_descriptor_set_layout_unref(&dev->vk, layout);
}

/* ------------------------------------------------------------ sampler */

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreateSampler(VkDevice _device, const VkSamplerCreateInfo *pCreateInfo,
                        const VkAllocationCallbacks *pAllocator, VkSampler *pSampler)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   struct infinigpu_sampler *s =
      vk_sampler_create(&dev->vk, pCreateInfo, pAllocator, sizeof(*s));
   if (!s)
      return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);

   /* The wire carries only linear-vs-nearest + repeat-vs-clamp (see sampler_flags). */
   s->linear = (pCreateInfo->magFilter == VK_FILTER_LINEAR);
   s->repeat = (pCreateInfo->addressModeU == VK_SAMPLER_ADDRESS_MODE_REPEAT);

   *pSampler = infinigpu_sampler_to_handle(s);
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_DestroySampler(VkDevice _device, VkSampler _sampler,
                         const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   VK_FROM_HANDLE(infinigpu_sampler, s, _sampler);
   if (!s)
      return;
   vk_sampler_destroy(&dev->vk, pAllocator, &s->vk);
}

/* ------------------------------------------------------------ descriptor pool + sets */

static void
infinigpu_free_set(struct infinigpu_device *dev, struct infinigpu_descriptor_set *set)
{
   list_del(&set->link);
   vk_object_free(&dev->vk, NULL, set);
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreateDescriptorPool(VkDevice _device,
                               const VkDescriptorPoolCreateInfo *pCreateInfo,
                               const VkAllocationCallbacks *pAllocator,
                               VkDescriptorPool *pDescriptorPool)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   struct infinigpu_descriptor_pool *pool =
      vk_object_zalloc(&dev->vk, pAllocator, sizeof(*pool), VK_OBJECT_TYPE_DESCRIPTOR_POOL);
   if (!pool)
      return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);

   list_inithead(&pool->sets);
   *pDescriptorPool = infinigpu_descriptor_pool_to_handle(pool);
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_DestroyDescriptorPool(VkDevice _device, VkDescriptorPool _pool,
                                const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   VK_FROM_HANDLE(infinigpu_descriptor_pool, pool, _pool);
   if (!pool)
      return;

   list_for_each_entry_safe(struct infinigpu_descriptor_set, set, &pool->sets, link)
      infinigpu_free_set(dev, set);
   vk_object_free(&dev->vk, pAllocator, pool);
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_ResetDescriptorPool(VkDevice _device, VkDescriptorPool _pool,
                              VkDescriptorPoolResetFlags flags)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   VK_FROM_HANDLE(infinigpu_descriptor_pool, pool, _pool);

   list_for_each_entry_safe(struct infinigpu_descriptor_set, set, &pool->sets, link)
      infinigpu_free_set(dev, set);
   list_inithead(&pool->sets);
   return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_AllocateDescriptorSets(VkDevice _device,
                                 const VkDescriptorSetAllocateInfo *pAllocateInfo,
                                 VkDescriptorSet *pDescriptorSets)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   VK_FROM_HANDLE(infinigpu_descriptor_pool, pool, pAllocateInfo->descriptorPool);

   uint32_t i;
   for (i = 0; i < pAllocateInfo->descriptorSetCount; i++) {
      struct infinigpu_descriptor_set *set =
         vk_object_zalloc(&dev->vk, NULL, sizeof(*set), VK_OBJECT_TYPE_DESCRIPTOR_SET);
      if (!set) {
         /* Roll back the sets allocated so far, per the spec's all-or-nothing contract. */
         for (uint32_t j = 0; j < i; j++) {
            VK_FROM_HANDLE(infinigpu_descriptor_set, s, pDescriptorSets[j]);
            infinigpu_free_set(dev, s);
            pDescriptorSets[j] = VK_NULL_HANDLE;
         }
         return vk_error(dev, VK_ERROR_OUT_OF_POOL_MEMORY);
      }
      set->pool = pool;
      set->image = NULL;
      set->sampler = NULL;
      set->tex_binding = 0;
      set->ubo_buffer = NULL;
      set->ubo_offset = 0;
      set->ubo_range = 0;
      set->ubo_binding = 0;
      list_addtail(&set->link, &pool->sets);
      pDescriptorSets[i] = infinigpu_descriptor_set_to_handle(set);
   }
   return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_FreeDescriptorSets(VkDevice _device, VkDescriptorPool _pool,
                             uint32_t descriptorSetCount,
                             const VkDescriptorSet *pDescriptorSets)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   for (uint32_t i = 0; i < descriptorSetCount; i++) {
      VK_FROM_HANDLE(infinigpu_descriptor_set, set, pDescriptorSets[i]);
      if (set)
         infinigpu_free_set(dev, set);
   }
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_UpdateDescriptorSets(VkDevice _device, uint32_t descriptorWriteCount,
                               const VkWriteDescriptorSet *pDescriptorWrites,
                               uint32_t descriptorCopyCount,
                               const VkCopyDescriptorSet *pDescriptorCopies)
{
   /* Capture the resources each write binds, and the binding NUMBER (dstBinding) so the host can build
    * a descriptor-set layout that places them where the shader declares them. A sampled image + sampler
    * and a uniform buffer can be written into the SAME set at distinct bindings (Phase-2c composition).
    * The single-resource case takes element 0. */
   for (uint32_t w = 0; w < descriptorWriteCount; w++) {
      const VkWriteDescriptorSet *wr = &pDescriptorWrites[w];
      VK_FROM_HANDLE(infinigpu_descriptor_set, set, wr->dstSet);
      if (!set || wr->descriptorCount == 0)
         continue;

      switch (wr->descriptorType) {
      case VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER:
         if (!wr->pImageInfo)
            break;
         if (wr->pImageInfo[0].imageView) {
            set->image = infinigpu_image_view_from_handle(wr->pImageInfo[0].imageView);
            set->tex_binding = wr->dstBinding;
         }
         if (wr->pImageInfo[0].sampler)
            set->sampler = infinigpu_sampler_from_handle(wr->pImageInfo[0].sampler);
         break;
      case VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE:
         if (wr->pImageInfo && wr->pImageInfo[0].imageView) {
            set->image = infinigpu_image_view_from_handle(wr->pImageInfo[0].imageView);
            set->tex_binding = wr->dstBinding;
         }
         break;
      case VK_DESCRIPTOR_TYPE_SAMPLER:
         if (wr->pImageInfo && wr->pImageInfo[0].sampler)
            set->sampler = infinigpu_sampler_from_handle(wr->pImageInfo[0].sampler);
         break;
      case VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER:
         /* A UBO (per-frame matrices etc.). Non-dynamic only — dynamic offsets from
          * CmdBindDescriptorSets are unsupported this iteration. */
         if (wr->pBufferInfo && wr->pBufferInfo[0].buffer) {
            set->ubo_buffer = infinigpu_buffer_from_handle(wr->pBufferInfo[0].buffer);
            set->ubo_offset = wr->pBufferInfo[0].offset;
            set->ubo_range = wr->pBufferInfo[0].range;
            set->ubo_binding = wr->dstBinding;
         }
         break;
      default:
         break; /* SSBO/dynamic/etc. — not forwarded yet */
      }
   }

   /* Descriptor copies would move bindings between sets; unused by the apps we forward today. */
   (void)descriptorCopyCount;
   (void)pDescriptorCopies;
}
