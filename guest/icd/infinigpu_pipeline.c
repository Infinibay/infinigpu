/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * VkShaderModule + VkPipeline (+ a dummy VkPipelineCache). The driver NEVER
 * compiles SPIR-V — it captures each stage's SPIR-V, entry point and the draw
 * topology and forwards them at submit. These entrypoints are hand-written
 * because the lite runtime does not backfill them (their vk_common versions live
 * in the full runtime, which would drag in the whole nir/vtn toolchain).
 * Cribbed from lavapipe (lvp_pipeline.c) + vk_pipeline.c's SPIR-V extraction.
 */

#include "infinigpu_private.h"

#include <string.h>

#include "vk_alloc.h"
#include "vk_log.h"
#include "vk_object.h"
#include "vk_util.h"

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreateShaderModule(VkDevice _device,
                             const VkShaderModuleCreateInfo *pCreateInfo,
                             const VkAllocationCallbacks *pAllocator,
                             VkShaderModule *pShaderModule)
{
   VK_FROM_HANDLE(vk_device, device, _device);
   /* Reuse the runtime's struct vk_shader_module; store the SPIR-V, never a nir. */
   struct vk_shader_module *m =
      vk_object_alloc(device, pAllocator, sizeof(*m) + pCreateInfo->codeSize,
                      VK_OBJECT_TYPE_SHADER_MODULE);
   if (!m)
      return VK_ERROR_OUT_OF_HOST_MEMORY;

   m->nir = NULL;
   m->size = pCreateInfo->codeSize;
   memcpy(m->data, pCreateInfo->pCode, pCreateInfo->codeSize);

   *pShaderModule = vk_shader_module_to_handle(m);
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_DestroyShaderModule(VkDevice _device, VkShaderModule _module,
                              const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(vk_device, device, _device);
   VK_FROM_HANDLE(vk_shader_module, m, _module);

   if (!m)
      return;
   vk_object_free(device, pAllocator, m);
}

static VkResult
infinigpu_pipeline_add_stage(struct infinigpu_device *dev,
                             struct infinigpu_pipeline *p,
                             const VkPipelineShaderStageCreateInfo *s)
{
   if (p->stage_count >= INFINIGPU_MAX_STAGES)
      return vk_errorf(dev, VK_ERROR_UNKNOWN, "too many pipeline stages");

   /* SPIR-V source: either a VkShaderModule handle, or an inline
    * VkShaderModuleCreateInfo in pNext (VK_KHR_maintenance5 / 1.3). */
   VK_FROM_HANDLE(vk_shader_module, module, s->module);
   const uint32_t *spirv_data;
   uint32_t spirv_size;
   if (module != NULL) {
      spirv_data = (const uint32_t *)module->data;
      spirv_size = module->size;
   } else {
      const VkShaderModuleCreateInfo *minfo =
         vk_find_struct_const(s->pNext, SHADER_MODULE_CREATE_INFO);
      if (minfo == NULL)
         return vk_errorf(dev, VK_ERROR_UNKNOWN, "no shader module provided");
      spirv_data = minfo->pCode;
      spirv_size = minfo->codeSize;
   }

   struct infinigpu_pipeline_stage *dst = &p->stages[p->stage_count];
   dst->spirv = vk_alloc(&dev->vk.alloc, spirv_size, 8,
                         VK_SYSTEM_ALLOCATION_SCOPE_OBJECT);
   if (!dst->spirv)
      return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);
   memcpy(dst->spirv, spirv_data, spirv_size);
   dst->spirv_size = spirv_size;

   dst->entrypoint = vk_strdup(&dev->vk.alloc, s->pName,
                               VK_SYSTEM_ALLOCATION_SCOPE_OBJECT);
   if (!dst->entrypoint) {
      vk_free(&dev->vk.alloc, dst->spirv);
      dst->spirv = NULL;
      return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);
   }
   dst->stage = s->stage;
   p->stage_count++;
   return VK_SUCCESS;
}

static void
infinigpu_pipeline_free_stages(struct infinigpu_device *dev,
                               struct infinigpu_pipeline *p)
{
   for (uint32_t i = 0; i < p->stage_count; i++) {
      vk_free(&dev->vk.alloc, p->stages[i].spirv);
      vk_free(&dev->vk.alloc, p->stages[i].entrypoint);
   }
   p->stage_count = 0;
}

static VkResult
infinigpu_graphics_pipeline_init(struct infinigpu_device *dev,
                                 const VkGraphicsPipelineCreateInfo *ci,
                                 struct infinigpu_pipeline *p)
{
   p->bind_point = VK_PIPELINE_BIND_POINT_GRAPHICS;
   /* Minimal path: a normal graphics pipeline has non-NULL input assembly and a
    * fixed (non-dynamic) topology. */
   p->topology = ci->pInputAssemblyState
                    ? ci->pInputAssemblyState->topology
                    : VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST;

   for (uint32_t i = 0; i < ci->stageCount; i++) {
      VkResult r = infinigpu_pipeline_add_stage(dev, p, &ci->pStages[i]);
      if (r != VK_SUCCESS)
         return r;
   }
   return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreateGraphicsPipelines(VkDevice _device, VkPipelineCache pipelineCache,
                                  uint32_t createInfoCount,
                                  const VkGraphicsPipelineCreateInfo *pCreateInfos,
                                  const VkAllocationCallbacks *pAllocator,
                                  VkPipeline *pPipelines)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   VkResult result = VK_SUCCESS;
   uint32_t i = 0;

   for (; i < createInfoCount; i++) {
      struct infinigpu_pipeline *p =
         vk_object_zalloc(&dev->vk, pAllocator, sizeof(*p), VK_OBJECT_TYPE_PIPELINE);
      VkResult r = p ? infinigpu_graphics_pipeline_init(dev, &pCreateInfos[i], p)
                     : VK_ERROR_OUT_OF_HOST_MEMORY;
      if (r != VK_SUCCESS) {
         if (p) {
            infinigpu_pipeline_free_stages(dev, p);
            vk_object_free(&dev->vk, pAllocator, p);
         }
         pPipelines[i] = VK_NULL_HANDLE;
         result = r;
         if (pCreateInfos[i].flags & VK_PIPELINE_CREATE_EARLY_RETURN_ON_FAILURE_BIT)
            break;
         continue;
      }
      pPipelines[i] = infinigpu_pipeline_to_handle(p);
   }

   for (; i < createInfoCount; i++)
      pPipelines[i] = VK_NULL_HANDLE;
   return result;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_DestroyPipeline(VkDevice _device, VkPipeline _pipeline,
                          const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   VK_FROM_HANDLE(infinigpu_pipeline, p, _pipeline);

   if (!p)
      return;
   infinigpu_pipeline_free_stages(dev, p);
   vk_object_free(&dev->vk, pAllocator, p);
}

/* We forward SPIR-V and never compile, so a pipeline cache is a no-op object —
 * present only so a well-behaved app's vkCreatePipelineCache succeeds. */
VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreatePipelineCache(VkDevice _device,
                              const VkPipelineCacheCreateInfo *pCreateInfo,
                              const VkAllocationCallbacks *pAllocator,
                              VkPipelineCache *pPipelineCache)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   struct infinigpu_pipeline_cache *cache =
      vk_object_zalloc(&dev->vk, pAllocator, sizeof(*cache),
                       VK_OBJECT_TYPE_PIPELINE_CACHE);
   if (!cache)
      return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);

   *pPipelineCache = infinigpu_pipeline_cache_to_handle(cache);
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_DestroyPipelineCache(VkDevice _device, VkPipelineCache _cache,
                               const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(infinigpu_device, dev, _device);
   VK_FROM_HANDLE(infinigpu_pipeline_cache, cache, _cache);

   if (!cache)
      return;
   vk_object_free(&dev->vk, pAllocator, cache);
}
