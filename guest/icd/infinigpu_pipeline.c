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
#include "infinigpu_forwarded.h"

#include <string.h>

#include "vk_alloc.h"
#include "vk_log.h"
#include "vk_object.h"
#include "vk_util.h"

/* Map a VkFormat used for a vertex attribute to the wire `vk_vformat`. The host's `map_vformat`
 * has a safe fallback, but we only forward the formats the encoder documents; an unrecognized
 * attribute format forwards as RGBA32F (won't crash, may misread — rare in practice). */
static uint32_t
infinigpu_map_vformat(VkFormat f)
{
   switch (f) {
   case VK_FORMAT_R32_SFLOAT:          return INFINIGPU_VFORMAT_R32_SFLOAT;
   case VK_FORMAT_R32G32_SFLOAT:       return INFINIGPU_VFORMAT_R32G32_SFLOAT;
   case VK_FORMAT_R32G32B32_SFLOAT:    return INFINIGPU_VFORMAT_R32G32B32_SFLOAT;
   case VK_FORMAT_R32G32B32A32_SFLOAT: return INFINIGPU_VFORMAT_R32G32B32A32_SFLOAT;
   case VK_FORMAT_R8G8B8A8_UNORM:      return INFINIGPU_VFORMAT_R8G8B8A8_UNORM;
   case VK_FORMAT_R32_UINT:            return INFINIGPU_VFORMAT_R32_UINT;
   default:                            return INFINIGPU_VFORMAT_R32G32B32A32_SFLOAT;
   }
}

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

   /* Phase-2b: capture the vertex-input layout (binding 0 only — the wire is single-binding).
    * `vertex_stride` stays 0 when the pipeline reads no vertex buffer, which routes submit to the
    * bufferless FORWARDED path (SM-generated vertices, e.g. a fullscreen triangle). */
   const VkPipelineVertexInputStateCreateInfo *vi = ci->pVertexInputState;
   if (vi) {
      for (uint32_t b = 0; b < vi->vertexBindingDescriptionCount; b++) {
         if (vi->pVertexBindingDescriptions[b].binding == 0) {
            p->vertex_stride = vi->pVertexBindingDescriptions[b].stride;
            break;
         }
      }
      for (uint32_t a = 0; a < vi->vertexAttributeDescriptionCount; a++) {
         const VkVertexInputAttributeDescription *ad = &vi->pVertexAttributeDescriptions[a];
         if (ad->binding != 0)
            continue; /* multi-binding not on the wire yet — binding-0 attrs only */
         if (p->attr_count >= INFINIGPU_MAX_ATTRS)
            return vk_errorf(dev, VK_ERROR_UNKNOWN, "too many vertex attributes");
         p->attrs[p->attr_count].location = ad->location;
         p->attrs[p->attr_count].format = infinigpu_map_vformat(ad->format);
         p->attrs[p->attr_count].offset = ad->offset;
         p->attr_count++;
      }
   }

   /* Phase-2d: pack the depth-test state into a ForwardedCmdListTail.depth_flags bitfield. VkCompareOp
    * values (0..7) match INFINIGPU_DEPTH_CMP_* exactly. Ignored on the bufferless path (stride 0). */
   const VkPipelineDepthStencilStateCreateInfo *ds = ci->pDepthStencilState;
   if (ds && (ds->depthTestEnable || ds->depthWriteEnable)) {
      uint32_t df = 0;
      if (ds->depthTestEnable)
         df |= INFINIGPU_DEPTH_TEST;
      if (ds->depthWriteEnable)
         df |= INFINIGPU_DEPTH_WRITE;
      df |= ((uint32_t)ds->depthCompareOp) << INFINIGPU_DEPTH_COMPARE_SHIFT;
      p->depth_flags = df;
   }

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
