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
#include "vk_descriptor_update_template.h"
#include "vk_log.h"
#include "vk_object.h"
#include "vk_util.h"

/* ------------------------------------------------------------ descriptor set layout */

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreateDescriptorSetLayout(VkDevice _device,
                                    const VkDescriptorSetLayoutCreateInfo *pCreateInfo,
                                    const VkAllocationCallbacks *pAllocator,
                                    VkDescriptorSetLayout *pSetLayout)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   IGPU_TRACE("CreateDescriptorSetLayout: bindings=%u flags=0x%x",
              pCreateInfo->bindingCount, pCreateInfo->flags);
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

/* Core Vulkan 1.1.  zink calls this UNCHECKED when it builds a set layout for a
 * shader's resources at first draw — leaving it NULL is a jump-to-0 SIGSEGV on
 * the zink thread.  We back layouts with runtime bookkeeping only (types come
 * from the writes, our per-stage limits are all 1024), so every layout zink can
 * describe is supported; just report that and echo any variable-count request. */
VKAPI_ATTR void VKAPI_CALL
infinigpu_GetDescriptorSetLayoutSupport(VkDevice _device,
                                        const VkDescriptorSetLayoutCreateInfo *pCreateInfo,
                                        VkDescriptorSetLayoutSupport *pSupport)
{
   const VkDescriptorSetLayoutBindingFlagsCreateInfo *variable_flags =
      vk_find_struct_const(pCreateInfo->pNext,
                           DESCRIPTOR_SET_LAYOUT_BINDING_FLAGS_CREATE_INFO);
   VkDescriptorSetVariableDescriptorCountLayoutSupport *variable_count =
      vk_find_struct(pSupport->pNext,
                     DESCRIPTOR_SET_VARIABLE_DESCRIPTOR_COUNT_LAYOUT_SUPPORT);
   if (variable_count) {
      variable_count->maxVariableDescriptorCount = 0;
      if (variable_flags) {
         for (unsigned i = 0; i < variable_flags->bindingCount; i++) {
            if (variable_flags->pBindingFlags[i] &
                VK_DESCRIPTOR_BINDING_VARIABLE_DESCRIPTOR_COUNT_BIT)
               variable_count->maxVariableDescriptorCount = 1024;
         }
      }
   }
   pSupport->supported = VK_TRUE;
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
   IGPU_TRACE("CreateDescriptorPool: maxSets=%u flags=0x%x", pCreateInfo->maxSets,
              pCreateInfo->flags);
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
      set->texture_count = 0;
      set->ubo_count = 0;
      set->ssbo_buffer = NULL;
      set->ssbo_offset = 0;
      set->ssbo_range = 0;
      set->ssbo_binding = 0;
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

/* Find the texture slot for image-binding `binding`, creating it (image/sampler NULL) if absent.
 * Returns NULL only when the set is already full (INFINIGPU_MAX_SET_TEXTURES) — a fail-safe drop. The
 * slots are keyed by the sampled-image dstBinding; a separate SAMPLER write at `binding+1` pairs with
 * the image slot at `binding` (the host's image@b / sampler@b+1 layout). */
static struct infinigpu_desc_texture *
infinigpu_tex_slot(struct infinigpu_descriptor_set *set, uint32_t binding)
{
   for (uint32_t i = 0; i < set->texture_count; i++)
      if (set->textures[i].binding == binding)
         return &set->textures[i];
   if (set->texture_count >= INFINIGPU_MAX_SET_TEXTURES)
      return NULL;
   struct infinigpu_desc_texture *t = &set->textures[set->texture_count++];
   t->image = NULL;
   t->sampler = NULL;
   t->binding = binding;
   return t;
}

/* Find the UBO slot for `binding`, creating it if absent. Returns NULL only when the set already holds
 * INFINIGPU_MAX_SET_UBOS distinct UBO bindings (fail-safe drop). Keyed by binding so re-writing the same
 * binding updates in place and distinct bindings (MVP@0, colour@4) each keep their own slot. */
static struct infinigpu_desc_ubo *
infinigpu_ubo_slot(struct infinigpu_descriptor_set *set, uint32_t binding)
{
   for (uint32_t i = 0; i < set->ubo_count; i++)
      if (set->ubos[i].binding == binding)
         return &set->ubos[i];
   if (set->ubo_count >= INFINIGPU_MAX_SET_UBOS)
      return NULL;
   struct infinigpu_desc_ubo *u = &set->ubos[set->ubo_count++];
   u->buffer = NULL;
   u->offset = 0;
   u->range = 0;
   u->binding = binding;
   return u;
}

/* Capture ONE descriptor's resource into the set, keyed by binding number so the host can build a
 * layout that places it where the shader declares it. Shared by the plain-write path
 * (vkUpdateDescriptorSets) and the template path (vkUpdateDescriptorSetWithTemplate): `img` and
 * `buf` point at the source VkDescriptorImageInfo / VkDescriptorBufferInfo — only the one matching
 * `type` is read. A separate SAMPLER at `b+1` pairs with the image at `b` (host layout image@b,
 * sampler@b+1). Single-resource capture; array elements collapse onto the binding (last wins). */
static void
infinigpu_apply_descriptor(struct infinigpu_descriptor_set *set, VkDescriptorType type,
                           uint32_t dstBinding, const VkDescriptorImageInfo *img,
                           const VkDescriptorBufferInfo *buf)
{
   switch (type) {
   case VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER: {
      if (!img)
         break;
      struct infinigpu_desc_texture *t = infinigpu_tex_slot(set, dstBinding);
      if (!t)
         break;
      if (img->imageView)
         t->image = infinigpu_image_view_from_handle(img->imageView);
      if (img->sampler)
         t->sampler = infinigpu_sampler_from_handle(img->sampler);
      break;
   }
   case VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE: {
      if (!img || !img->imageView)
         break;
      struct infinigpu_desc_texture *t = infinigpu_tex_slot(set, dstBinding);
      if (t)
         t->image = infinigpu_image_view_from_handle(img->imageView);
      break;
   }
   case VK_DESCRIPTOR_TYPE_SAMPLER: {
      /* A separate sampler pairs with the image one binding lower (host layout: image@b, sampler@b+1). */
      if (!img || !img->sampler || dstBinding == 0)
         break;
      struct infinigpu_desc_texture *t = infinigpu_tex_slot(set, dstBinding - 1);
      if (t)
         t->sampler = infinigpu_sampler_from_handle(img->sampler);
      break;
   }
   case VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER:
      /* A UBO (per-frame matrices, a material/colour block, etc.). Keyed by binding so a set with
       * several (zink binds an MVP@0 for the VS + a colour@4 for the FS) keeps them all. Non-dynamic
       * only — dynamic offsets from CmdBindDescriptorSets are unsupported this iteration. */
      if (buf && buf->buffer) {
         struct infinigpu_desc_ubo *u = infinigpu_ubo_slot(set, dstBinding);
         if (u) {
            u->buffer = infinigpu_buffer_from_handle(buf->buffer);
            u->offset = buf->offset;
            u->range = buf->range;
         }
      }
      break;
   case VK_DESCRIPTOR_TYPE_STORAGE_BUFFER:
      /* A read-only SSBO (a DXVK structured/raw SRV, a skinning palette, per-instance data).
       * Non-dynamic only (STORAGE_BUFFER_DYNAMIC stays in `default:` — dynamic offsets are a
       * follow-up). Mirrors the UBO capture exactly; forwarded as bytes, never written back. */
      if (buf && buf->buffer) {
         set->ssbo_buffer = infinigpu_buffer_from_handle(buf->buffer);
         set->ssbo_offset = buf->offset;
         set->ssbo_range = buf->range;
         set->ssbo_binding = dstBinding;
      }
      break;
   default:
      break; /* STORAGE_BUFFER_DYNAMIC/UNIFORM_BUFFER_DYNAMIC/etc. — not forwarded yet */
   }
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_UpdateDescriptorSets(VkDevice _device, uint32_t descriptorWriteCount,
                               const VkWriteDescriptorSet *pDescriptorWrites,
                               uint32_t descriptorCopyCount,
                               const VkCopyDescriptorSet *pDescriptorCopies)
{
   for (uint32_t w = 0; w < descriptorWriteCount; w++) {
      const VkWriteDescriptorSet *wr = &pDescriptorWrites[w];
      VK_FROM_HANDLE(infinigpu_descriptor_set, set, wr->dstSet);
      if (!set || wr->descriptorCount == 0)
         continue;
      infinigpu_apply_descriptor(set, wr->descriptorType, wr->dstBinding,
                                 wr->pImageInfo, wr->pBufferInfo);
   }

   /* Descriptor copies would move bindings between sets; unused by the apps we forward today. */
   (void)descriptorCopyCount;
   (void)pDescriptorCopies;
}

/* Core Vulkan 1.1.  zink's DEFAULT descriptor path: it bakes a set's writes into a template once and
 * replays them per draw via this call (leaving it NULL is the jump-to-0 SIGSEGV zink hits after the
 * layout-support check).  The common runtime already parsed the template into vk_descriptor_template_
 * entry records; we walk them and reuse the exact same per-descriptor capture as the plain-write path.
 * `pData` is the caller's blob — each entry element lives at pData + offset + i*stride and is a
 * VkDescriptorImageInfo, VkDescriptorBufferInfo, or VkBufferView per the entry's descriptor type. */
VKAPI_ATTR void VKAPI_CALL
infinigpu_UpdateDescriptorSetWithTemplate(VkDevice _device, VkDescriptorSet descriptorSet,
                                          VkDescriptorUpdateTemplate descriptorUpdateTemplate,
                                          const void *pData)
{
   VK_FROM_HANDLE(infinigpu_descriptor_set, set, descriptorSet);
   VK_FROM_HANDLE(vk_descriptor_update_template, templ, descriptorUpdateTemplate);
   if (!set || !templ)
      return;

   for (uint32_t e = 0; e < templ->entry_count; e++) {
      const struct vk_descriptor_template_entry *entry = &templ->entries[e];
      for (uint32_t j = 0; j < entry->array_count; j++) {
         const char *p = (const char *)pData + entry->offset + (size_t)j * entry->stride;
         infinigpu_apply_descriptor(set, entry->type, entry->binding,
                                    (const VkDescriptorImageInfo *)p,
                                    (const VkDescriptorBufferInfo *)p);
      }
   }
}
