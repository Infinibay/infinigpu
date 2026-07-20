/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * VkCommandBuffer — DIRECT-record model (not lavapipe's enqueue/replay). Cmd*
 * calls accumulate state (bound pipeline, color attachment, draw, deferred
 * copies) into infinigpu_cmd_buffer; the real GPU work happens synchronously in
 * infinigpu_queue_submit (infinigpu_sync.c) via DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED.
 * Cribbed from lavapipe (lvp_cmd_buffer.c). Cmd{SetViewport,SetScissor,
 * BeginRenderPass2,...} are left to vk_common backfills (lite runtime).
 */

#include "infinigpu_private.h"
#include "infinigpu_forwarded.h"

#include <string.h>

#include "vk_alloc.h"
#include "vk_command_buffer.h"
#include "vk_command_pool.h"

static void
infinigpu_cmd_reset_state(struct infinigpu_cmd_buffer *cmd)
{
   cmd->bound_pipeline = NULL;
   cmd->color_att = NULL;
   cmd->has_clear = false;
   cmd->draw_vertex_count = 0;
   cmd->draw_count = 0;
   cmd->vbuf = NULL;
   cmd->vbuf_offset = 0;
   cmd->ibuf = NULL;
   cmd->ibuf_offset = 0;
   cmd->index_type = INFINIGPU_INDEX_TYPE_U16;
   cmd->has_dyn_viewport = false;
   cmd->dyn_set_mask = 0; /* EDS1 dynamic-state values are per-recording; drop them on reset */
   cmd->push_const_len = 0;
   cmd->bound_desc_set = NULL;
   cmd->upload_count = 0;
   cmd->copy_count = 0;
}

/* The viewport in effect for a draw: the last CmdSetViewport, else all-zero so the host falls back
 * to the full render target (its `viewport[2] == 0` convention). */
static void
infinigpu_current_viewport(const struct infinigpu_cmd_buffer *cmd, float out[4])
{
   if (cmd->has_dyn_viewport) {
      memcpy(out, cmd->dyn_viewport, sizeof(float) * 4);
   } else {
      out[0] = out[1] = out[2] = out[3] = 0.0f;
   }
}

static void
infinigpu_cmd_buffer_destroy(struct vk_command_buffer *vk_cmd)
{
   struct infinigpu_cmd_buffer *cmd =
      container_of(vk_cmd, struct infinigpu_cmd_buffer, vk);
   vk_command_buffer_finish(&cmd->vk);
   vk_free(&cmd->vk.pool->alloc, cmd);
}

static VkResult
infinigpu_create_cmd_buffer(struct vk_command_pool *pool,
                            VkCommandBufferLevel level,
                            struct vk_command_buffer **cmd_buffer_out)
{
   struct infinigpu_device *device =
      container_of(pool->base.device, struct infinigpu_device, vk);
   struct infinigpu_cmd_buffer *cmd =
      vk_alloc(&pool->alloc, sizeof(*cmd), 8, VK_SYSTEM_ALLOCATION_SCOPE_OBJECT);
   if (cmd == NULL)
      return vk_error(device, VK_ERROR_OUT_OF_HOST_MEMORY);

   VkResult result =
      vk_command_buffer_init(pool, &cmd->vk, &infinigpu_cmd_buffer_ops, level);
   if (result != VK_SUCCESS) {
      vk_free(&pool->alloc, cmd);
      return result;
   }
   cmd->device = device;
   infinigpu_cmd_reset_state(cmd);

   *cmd_buffer_out = &cmd->vk;
   return VK_SUCCESS;
}

static void
infinigpu_reset_cmd_buffer(struct vk_command_buffer *vk_cmd,
                           UNUSED VkCommandBufferResetFlags flags)
{
   struct infinigpu_cmd_buffer *cmd =
      container_of(vk_cmd, struct infinigpu_cmd_buffer, vk);
   infinigpu_cmd_reset_state(cmd);
   vk_command_buffer_reset(&cmd->vk);
}

const struct vk_command_buffer_ops infinigpu_cmd_buffer_ops = {
   .create = infinigpu_create_cmd_buffer,
   .reset = infinigpu_reset_cmd_buffer,
   .destroy = infinigpu_cmd_buffer_destroy,
};

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_BeginCommandBuffer(VkCommandBuffer commandBuffer,
                             const VkCommandBufferBeginInfo *pBeginInfo)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   vk_command_buffer_begin(&cmd->vk, pBeginInfo);
   infinigpu_cmd_reset_state(cmd);
   return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_EndCommandBuffer(VkCommandBuffer commandBuffer)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   return vk_command_buffer_end(&cmd->vk);
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdBindPipeline(VkCommandBuffer commandBuffer,
                          VkPipelineBindPoint pipelineBindPoint, VkPipeline pipeline)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   if (pipelineBindPoint == VK_PIPELINE_BIND_POINT_GRAPHICS)
      cmd->bound_pipeline = infinigpu_pipeline_from_handle(pipeline);
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdBeginRendering(VkCommandBuffer commandBuffer,
                            const VkRenderingInfo *pRenderingInfo)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);

   cmd->render_area = pRenderingInfo->renderArea;
   cmd->color_att = NULL;
   cmd->has_clear = false;

   if (pRenderingInfo->colorAttachmentCount >= 1) {
      const VkRenderingAttachmentInfo *att = &pRenderingInfo->pColorAttachments[0];
      cmd->color_att = infinigpu_image_view_from_handle(att->imageView);
      if (att->loadOp == VK_ATTACHMENT_LOAD_OP_CLEAR) {
         cmd->clear_value = att->clearValue.color;
         cmd->has_clear = true;
      }
   }
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdEndRendering(VkCommandBuffer commandBuffer)
{
   /* Nothing to flush at record time — the draw is forwarded at submit. */
}

/* Append a recorded draw to the multi-draw list (shared by CmdDraw + CmdDrawIndexed). Silently caps
 * at INFINIGPU_MAX_DRAWS — a command buffer with more draws than that overflows the static list; we
 * flag it so the submit fails closed rather than dropping geometry. */
static void
infinigpu_record_draw(struct infinigpu_cmd_buffer *cmd, uint32_t count,
                      uint32_t instance_count, uint32_t first, int32_t vertex_offset,
                      bool indexed)
{
   if (cmd->draw_count >= INFINIGPU_MAX_DRAWS) {
      vk_command_buffer_set_error(&cmd->vk, VK_ERROR_OUT_OF_HOST_MEMORY);
      return;
   }
   struct infinigpu_draw *d = &cmd->draws[cmd->draw_count++];
   d->count = count;
   d->instance_count = instance_count;
   d->first = first;
   d->vertex_offset = vertex_offset;
   d->indexed = indexed;
   infinigpu_current_viewport(cmd, d->viewport);
   /* Kept for the bufferless fallback (no vertex buffer bound): the last non-indexed vertexCount. */
   if (!indexed)
      cmd->draw_vertex_count = count;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdDraw(VkCommandBuffer commandBuffer, uint32_t vertexCount,
                  uint32_t instanceCount, uint32_t firstVertex, uint32_t firstInstance)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   /* Record the draw; a single forwarded submit replays the bound pipeline's shaders over the
    * bound vertex buffer (or SM-generated vertices if none) at submit time. */
   infinigpu_record_draw(cmd, vertexCount, instanceCount, firstVertex, 0, false);
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdDrawIndexed(VkCommandBuffer commandBuffer, uint32_t indexCount,
                         uint32_t instanceCount, uint32_t firstIndex,
                         int32_t vertexOffset, uint32_t firstInstance)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   infinigpu_record_draw(cmd, indexCount, instanceCount, firstIndex, vertexOffset, true);
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdBindVertexBuffers2(VkCommandBuffer commandBuffer, uint32_t firstBinding,
                                uint32_t bindingCount, const VkBuffer *pBuffers,
                                const VkDeviceSize *pOffsets, const VkDeviceSize *pSizes,
                                const VkDeviceSize *pStrides)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   /* The wire carries a single interleaved binding 0; capture it, ignore higher bindings. */
   for (uint32_t i = 0; i < bindingCount; i++) {
      if (firstBinding + i != 0)
         continue;
      cmd->vbuf = pBuffers ? infinigpu_buffer_from_handle(pBuffers[i]) : NULL;
      cmd->vbuf_offset = pOffsets ? pOffsets[i] : 0;
   }
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdBindIndexBuffer2(VkCommandBuffer commandBuffer, VkBuffer buffer,
                              VkDeviceSize offset, VkDeviceSize size, VkIndexType indexType)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   cmd->ibuf = infinigpu_buffer_from_handle(buffer);
   cmd->ibuf_offset = offset;
   cmd->index_type = (indexType == VK_INDEX_TYPE_UINT32) ? INFINIGPU_INDEX_TYPE_U32
                                                         : INFINIGPU_INDEX_TYPE_U16;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdSetViewport(VkCommandBuffer commandBuffer, uint32_t firstViewport,
                         uint32_t viewportCount, const VkViewport *pViewports)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   /* We forward a single viewport per draw; capture viewport 0. */
   if (firstViewport == 0 && viewportCount >= 1) {
      cmd->dyn_viewport[0] = pViewports[0].x;
      cmd->dyn_viewport[1] = pViewports[0].y;
      cmd->dyn_viewport[2] = pViewports[0].width;
      cmd->dyn_viewport[3] = pViewports[0].height;
      cmd->has_dyn_viewport = true;
   }
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdSetViewportWithCount(VkCommandBuffer commandBuffer, uint32_t viewportCount,
                                  const VkViewport *pViewports)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   if (viewportCount >= 1) {
      cmd->dyn_viewport[0] = pViewports[0].x;
      cmd->dyn_viewport[1] = pViewports[0].y;
      cmd->dyn_viewport[2] = pViewports[0].width;
      cmd->dyn_viewport[3] = pViewports[0].height;
      cmd->has_dyn_viewport = true;
   }
}

/* Extended-dynamic-state (EDS1, core Vulkan 1.3) setters. Each records the value + marks it set in
 * dyn_set_mask; at submit the resolver (infinigpu_sync.c) consults these ONLY for the states the bound
 * pipeline declared dynamic. DXVK/VKD3D drive real cull/front-face/depth/topology through these — the
 * pipelines leave the static fields at defaults, so without these the forwarded state would be wrong. */
VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdSetCullMode(VkCommandBuffer commandBuffer, VkCullModeFlags cullMode)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   cmd->dyn_cull_mode = cullMode;
   cmd->dyn_set_mask |= INFINIGPU_DYN_CULL_MODE;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdSetFrontFace(VkCommandBuffer commandBuffer, VkFrontFace frontFace)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   cmd->dyn_front_face = frontFace;
   cmd->dyn_set_mask |= INFINIGPU_DYN_FRONT_FACE;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdSetDepthTestEnable(VkCommandBuffer commandBuffer, VkBool32 depthTestEnable)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   cmd->dyn_depth_test = depthTestEnable;
   cmd->dyn_set_mask |= INFINIGPU_DYN_DEPTH_TEST;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdSetDepthWriteEnable(VkCommandBuffer commandBuffer, VkBool32 depthWriteEnable)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   cmd->dyn_depth_write = depthWriteEnable;
   cmd->dyn_set_mask |= INFINIGPU_DYN_DEPTH_WRITE;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdSetDepthCompareOp(VkCommandBuffer commandBuffer, VkCompareOp depthCompareOp)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   cmd->dyn_depth_compare = depthCompareOp;
   cmd->dyn_set_mask |= INFINIGPU_DYN_DEPTH_COMPARE;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdSetPrimitiveTopology(VkCommandBuffer commandBuffer, VkPrimitiveTopology primitiveTopology)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   cmd->dyn_topology = primitiveTopology;
   cmd->dyn_set_mask |= INFINIGPU_DYN_TOPOLOGY;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdBindDescriptorSets(VkCommandBuffer commandBuffer,
                               VkPipelineBindPoint pipelineBindPoint, VkPipelineLayout layout,
                               uint32_t firstSet, uint32_t descriptorSetCount,
                               const VkDescriptorSet *pDescriptorSets,
                               uint32_t dynamicOffsetCount, const uint32_t *pDynamicOffsets)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   if (pipelineBindPoint != VK_PIPELINE_BIND_POINT_GRAPHICS)
      return;
   /* Record the first bound set that carries a forwarded resource — a sampled image and/or a uniform
    * buffer, composed in one set (Phase-2c). Later sets with a resource override earlier ones
    * (last-bound wins, like a real driver). Single-set composition: the host binds exactly set 0. */
   for (uint32_t i = 0; i < descriptorSetCount; i++) {
      struct infinigpu_descriptor_set *set =
         infinigpu_descriptor_set_from_handle(pDescriptorSets[i]);
      if (set && (set->texture_count > 0 || set->ubo_buffer || set->ssbo_buffer))
         cmd->bound_desc_set = set;
   }
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdPushConstants(VkCommandBuffer commandBuffer, VkPipelineLayout layout,
                           VkShaderStageFlags stageFlags, uint32_t offset, uint32_t size,
                           const void *pValues)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   /* Place the bytes at their declared offset; the host applies the whole block at offset 0 to
    * VERTEX|FRAGMENT. Bound to the 256 B hardware max (the host rejects anything larger). */
   uint64_t end = (uint64_t)offset + size;
   if (end > INFINIGPU_MAX_PUSH_CONST) {
      vk_command_buffer_set_error(&cmd->vk, VK_ERROR_OUT_OF_HOST_MEMORY);
      return;
   }
   memcpy(cmd->push_const + offset, pValues, size);
   if (end > cmd->push_const_len)
      cmd->push_const_len = (uint32_t)end;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdPipelineBarrier2(VkCommandBuffer commandBuffer,
                              const VkDependencyInfo *pDependencyInfo)
{
   /* Synchronous single-submit driver: the blocking ioctl serializes everything,
    * so there is nothing to order here. (Required non-NULL so render-pass
    * emulation's implicit layout transitions don't hit a NULL dispatch slot.) */
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdCopyImageToBuffer2(VkCommandBuffer commandBuffer,
                                const VkCopyImageToBufferInfo2 *pInfo)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   struct infinigpu_image *src = infinigpu_image_from_handle(pInfo->srcImage);
   struct infinigpu_buffer *dst = infinigpu_buffer_from_handle(pInfo->dstBuffer);

   /* Defer each region to submit — the host must render into the image first. */
   for (uint32_t i = 0; i < pInfo->regionCount; i++) {
      if (cmd->copy_count >= INFINIGPU_MAX_COPIES) {
         vk_command_buffer_set_error(&cmd->vk, VK_ERROR_OUT_OF_HOST_MEMORY);
         return;
      }
      const VkBufferImageCopy2 *r = &pInfo->pRegions[i];
      struct infinigpu_pending_copy *pc = &cmd->copies[cmd->copy_count++];
      pc->src = src;
      pc->dst = dst;
      pc->buffer_offset = r->bufferOffset;
      pc->buffer_row_length = r->bufferRowLength;
      pc->image_offset = r->imageOffset;
      pc->image_extent = r->imageExtent;
   }
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdCopyBufferToImage2(VkCommandBuffer commandBuffer,
                               const VkCopyBufferToImageInfo2 *pInfo)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   struct infinigpu_buffer *src = infinigpu_buffer_from_handle(pInfo->srcBuffer);
   struct infinigpu_image *dst = infinigpu_image_from_handle(pInfo->dstImage);

   /* Defer each region to submit; uploads run BEFORE the forwarded draw so a staged texture is in
    * the image's LINEAR-packed memory when the draw samples it. */
   for (uint32_t i = 0; i < pInfo->regionCount; i++) {
      if (cmd->upload_count >= INFINIGPU_MAX_UPLOADS) {
         vk_command_buffer_set_error(&cmd->vk, VK_ERROR_OUT_OF_HOST_MEMORY);
         return;
      }
      const VkBufferImageCopy2 *r = &pInfo->pRegions[i];
      struct infinigpu_pending_upload *pu = &cmd->uploads[cmd->upload_count++];
      pu->src = src;
      pu->dst = dst;
      pu->buffer_offset = r->bufferOffset;
      pu->buffer_row_length = r->bufferRowLength;
      pu->image_offset = r->imageOffset;
      pu->image_extent = r->imageExtent;
   }
}
