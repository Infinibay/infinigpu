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
   cmd->copy_count = 0;
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

VKAPI_ATTR void VKAPI_CALL
infinigpu_CmdDraw(VkCommandBuffer commandBuffer, uint32_t vertexCount,
                  uint32_t instanceCount, uint32_t firstVertex, uint32_t firstInstance)
{
   VK_FROM_HANDLE(infinigpu_cmd_buffer, cmd, commandBuffer);
   /* Record the draw; a single forwarded submit replays the bound pipeline's
    * shaders over vertexCount vertices at submit time. */
   cmd->draw_vertex_count = vertexCount;
   cmd->draw_count++;
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
