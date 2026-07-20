/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * Synchronization + queue submit. The device runs in IMMEDIATE submit mode: a
 * binary CPU sync (infinigpu_sync, adapted from lavapipe's lvp_pipe_sync but
 * WITHOUT any async fence — DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED blocks until the
 * host GPU DMA-write completes, so a submit is fully done the instant the ioctl
 * returns). driver_submit honors waits, forwards each command buffer's recorded
 * draw + deferred copies, then signals every signal sync so WaitForFences returns.
 */

#include "infinigpu_private.h"
#include "infinigpu_forwarded.h"
#include "infinigpu_abi.h"    /* VertexAttrWire / DrawCmdWire — the cmdlist encoder's arrays */
#include "infinigpu_kmd.h"

#include <stdlib.h>
#include <string.h>

#include "util/os_time.h"
#include "util/timespec.h"
#include "vk_format.h"
#include "vk_log.h"

/* ------------------------------------------------------------------ sync type */

static VkResult
infinigpu_sync_init(UNUSED struct vk_device *vk_device, struct vk_sync *vk_sync,
                    uint64_t initial_value)
{
   struct infinigpu_sync *sync = infinigpu_sync_as(vk_sync);
   mtx_init(&sync->lock, mtx_plain);
   cnd_init(&sync->changed);
   sync->signaled = (initial_value != 0);
   return VK_SUCCESS;
}

static void
infinigpu_sync_finish(UNUSED struct vk_device *vk_device, struct vk_sync *vk_sync)
{
   struct infinigpu_sync *sync = infinigpu_sync_as(vk_sync);
   cnd_destroy(&sync->changed);
   mtx_destroy(&sync->lock);
}

static VkResult
infinigpu_sync_signal(UNUSED struct vk_device *vk_device, struct vk_sync *vk_sync,
                      UNUSED uint64_t value)
{
   struct infinigpu_sync *sync = infinigpu_sync_as(vk_sync);
   mtx_lock(&sync->lock);
   sync->signaled = true;
   cnd_broadcast(&sync->changed);
   mtx_unlock(&sync->lock);
   return VK_SUCCESS;
}

static VkResult
infinigpu_sync_reset(UNUSED struct vk_device *vk_device, struct vk_sync *vk_sync)
{
   struct infinigpu_sync *sync = infinigpu_sync_as(vk_sync);
   mtx_lock(&sync->lock);
   sync->signaled = false;
   cnd_broadcast(&sync->changed);
   mtx_unlock(&sync->lock);
   return VK_SUCCESS;
}

static VkResult
infinigpu_sync_wait(struct vk_device *vk_device, struct vk_sync *vk_sync,
                    UNUSED uint64_t wait_value, enum vk_sync_wait_flags wait_flags,
                    uint64_t abs_timeout_ns)
{
   struct infinigpu_sync *sync = infinigpu_sync_as(vk_sync);

   /* WAIT_ANY is a multi-wait concept; the runtime never passes it to a single
    * sync's wait. */
   assert(!(wait_flags & VK_SYNC_WAIT_ANY));

   mtx_lock(&sync->lock);

   uint64_t now_ns = os_time_get_nano();
   while (!sync->signaled) {
      if (now_ns >= abs_timeout_ns) {
         mtx_unlock(&sync->lock);
         return VK_TIMEOUT;
      }

      int ret;
      if (abs_timeout_ns >= INT64_MAX) {
         ret = cnd_wait(&sync->changed, &sync->lock);
      } else {
         /* C11 threads use CLOCK_REALTIME while our timeouts are CLOCK_MONOTONIC;
          * convert to a relative deadline and re-check now_ns after each wake. */
         uint64_t rel_ns = abs_timeout_ns - now_ns;
         struct timespec now_ts, abs_ts;
         timespec_get(&now_ts, TIME_UTC);
         if (timespec_add_nsec(&abs_ts, &now_ts, rel_ns))
            ret = cnd_wait(&sync->changed, &sync->lock);
         else
            ret = cnd_timedwait(&sync->changed, &sync->lock, &abs_ts);
      }
      if (ret == thrd_error) {
         mtx_unlock(&sync->lock);
         return vk_errorf(vk_device, VK_ERROR_UNKNOWN, "cnd_timedwait failed");
      }
      now_ns = os_time_get_nano();
   }

   mtx_unlock(&sync->lock);
   return VK_SUCCESS;
}

const struct vk_sync_type infinigpu_sync_type = {
   .size = sizeof(struct infinigpu_sync),
   .features = VK_SYNC_FEATURE_BINARY |
               VK_SYNC_FEATURE_GPU_WAIT |
               VK_SYNC_FEATURE_CPU_WAIT |
               VK_SYNC_FEATURE_CPU_RESET |
               VK_SYNC_FEATURE_CPU_SIGNAL |
               VK_SYNC_FEATURE_WAIT_PENDING,
   .init = infinigpu_sync_init,
   .finish = infinigpu_sync_finish,
   .signal = infinigpu_sync_signal,
   .reset = infinigpu_sync_reset,
   .wait = infinigpu_sync_wait,
};

/* ------------------------------------------------------------ queue submit */

/* Execute one deferred image->buffer copy now that the host has DMA-written the
 * image's dumb buffer. Both memories are host-mapped, so this is a CPU blit. */
static void
infinigpu_run_copy(const struct infinigpu_pending_copy *pc)
{
   struct infinigpu_image *img = pc->src;
   struct infinigpu_buffer *buf = pc->dst;

   if (!img || !img->mem || !img->mem->map || !buf || !buf->mem || !buf->mem->map)
      return;

   const uint32_t bpp = vk_format_get_blocksize(img->vk.format);
   const uint32_t rows = pc->image_extent.height;
   const uint32_t w = pc->image_extent.width;
   const uint32_t dst_row_texels = pc->buffer_row_length ? pc->buffer_row_length : w;

   const char *src = (const char *)img->mem->map + img->mem_offset +
                     (uint64_t)pc->image_offset.y * img->row_pitch +
                     (uint64_t)pc->image_offset.x * bpp;
   char *dst = (char *)buf->mem->map + buf->offset + pc->buffer_offset;

   for (uint32_t y = 0; y < rows; y++)
      memcpy(dst + (uint64_t)y * dst_row_texels * bpp,
             src + (uint64_t)y * img->row_pitch, (size_t)w * bpp);
}

/* Execute one deferred buffer->image copy (texture upload) — the mirror of infinigpu_run_copy. Both
 * memories are host-mapped, so this is a CPU blit into the image's LINEAR-packed rows. */
static void
infinigpu_run_upload(const struct infinigpu_pending_upload *pu)
{
   struct infinigpu_buffer *buf = pu->src;
   struct infinigpu_image *img = pu->dst;

   if (!buf || !buf->mem || !buf->mem->map || !img || !img->mem || !img->mem->map)
      return;

   const uint32_t bpp = vk_format_get_blocksize(img->vk.format);
   const uint32_t rows = pu->image_extent.height;
   const uint32_t w = pu->image_extent.width;
   const uint32_t src_row_texels = pu->buffer_row_length ? pu->buffer_row_length : w;

   const char *src = (const char *)buf->mem->map + buf->offset + pu->buffer_offset;
   char *dst = (char *)img->mem->map + img->mem_offset +
               (uint64_t)pu->image_offset.y * img->row_pitch +
               (uint64_t)pu->image_offset.x * bpp;

   for (uint32_t y = 0; y < rows; y++)
      memcpy(dst + (uint64_t)y * img->row_pitch,
             src + (uint64_t)y * src_row_texels * bpp, (size_t)w * bpp);
}

/* Phase-2b/2c: forward a real mesh (bound vertex/index buffers + multi-draw) via the command-list
 * encoder. The bound pipeline captured a non-zero vertex stride, so the app reads a vertex buffer;
 * we read its host-mapped bytes (whole vertices from the bound offset to the buffer end), build the
 * attr/draw wire arrays, and encode — plus a bound descriptor set's sampled texture (Phase-2c). */
static VkResult
infinigpu_replay_cmdlist(struct infinigpu_device *dev, struct infinigpu_cmd_buffer *cmd,
                         struct infinigpu_pipeline *p,
                         const struct infinigpu_pipeline_stage *vs,
                         const struct infinigpu_pipeline_stage *fs,
                         struct infinigpu_image *img)
{
   int drm_fd = dev->physical_device->drm_fd;
   const uint32_t width = img->vk.extent.width;
   const uint32_t height = img->vk.extent.height;
   float bg[4] = { 0.0f, 0.0f, 0.0f, 1.0f };
   if (cmd->has_clear)
      for (int c = 0; c < 4; c++)
         bg[c] = cmd->clear_value.float32[c];

   const uint32_t topo =
      (p->topology == VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP)
         ? INFINIGPU_VK_TOPOLOGY_TRIANGLE_STRIP
         : INFINIGPU_VK_TOPOLOGY_TRIANGLE_LIST;

   /* Vertex buffer: whole vertices from the CmdBindVertexBuffers offset to the buffer end. */
   struct infinigpu_buffer *vb = cmd->vbuf;
   if (!vb || !vb->map || cmd->vbuf_offset >= vb->total_size)
      return vk_errorf(dev, VK_ERROR_UNKNOWN, "cmdlist draw without a valid vertex buffer");
   const uint8_t *vdata = (const uint8_t *)vb->map + cmd->vbuf_offset;
   uint64_t vavail = vb->total_size - cmd->vbuf_offset;
   uint32_t vlen = (uint32_t)((vavail / p->vertex_stride) * p->vertex_stride);
   if (vlen == 0)
      return vk_errorf(dev, VK_ERROR_UNKNOWN, "vertex buffer smaller than one vertex");

   /* Index buffer: forwarded only if a draw is indexed (the host treats index presence as global,
    * so a command buffer must not mix indexed and non-indexed draws — content rarely does). */
   bool any_indexed = false;
   for (uint32_t i = 0; i < cmd->draw_count; i++)
      if (cmd->draws[i].indexed) {
         any_indexed = true;
         break;
      }
   const uint8_t *idata = NULL;
   uint32_t ilen = 0;
   if (any_indexed) {
      struct infinigpu_buffer *ib = cmd->ibuf;
      if (!ib || !ib->map || cmd->ibuf_offset >= ib->total_size)
         return vk_errorf(dev, VK_ERROR_UNKNOWN, "indexed draw without a valid index buffer");
      uint32_t istride = (cmd->index_type == INFINIGPU_INDEX_TYPE_U32) ? 4u : 2u;
      idata = (const uint8_t *)ib->map + cmd->ibuf_offset;
      uint64_t iavail = ib->total_size - cmd->ibuf_offset;
      ilen = (uint32_t)((iavail / istride) * istride);
   }

   /* Wire arrays: attrs from the pipeline's captured vertex-input, draws from the recorded list. */
   struct VertexAttrWire attrs[INFINIGPU_MAX_ATTRS];
   for (uint32_t a = 0; a < p->attr_count; a++) {
      attrs[a].location = p->attrs[a].location;
      attrs[a].format = p->attrs[a].format;
      attrs[a].offset = p->attrs[a].offset;
   }
   struct DrawCmdWire *draws = malloc(sizeof(*draws) * cmd->draw_count);
   if (!draws)
      return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);
   for (uint32_t i = 0; i < cmd->draw_count; i++) {
      draws[i].count = cmd->draws[i].count;
      draws[i].instance_count = cmd->draws[i].instance_count;
      draws[i].first = cmd->draws[i].first;
      draws[i].vertex_offset = cmd->draws[i].vertex_offset;
      draws[i].vp_x = cmd->draws[i].viewport[0];
      draws[i].vp_y = cmd->draws[i].viewport[1];
      draws[i].vp_w = cmd->draws[i].viewport[2];
      draws[i].vp_h = cmd->draws[i].viewport[3];
   }

   /* Phase-2c texture: if a descriptor set with a sampled image is bound, forward its RGBA8 pixels.
    * The ICD's images are LINEAR + tightly packed (row_pitch == width*bpp), so the pixels read
    * contiguously from the image's host-mapped memory. Only 4-bpp (RGBA8-class) images are
    * forwarded — the host samples them as R8G8B8A8; other formats are skipped fail-safe (untextured
    * rather than colour-scrambled). */
   struct TextureDescWire texdesc;
   const uint8_t *texpix = NULL;
   uint32_t texpix_len = 0;
   uint32_t tex_count = 0;
   if (cmd->bound_desc_set && cmd->bound_desc_set->image) {
      struct infinigpu_image *ti = cmd->bound_desc_set->image->image;
      if (ti && ti->mem && ti->mem->map && ti->row_pitch == ti->vk.extent.width * 4u) {
         uint32_t tw = ti->vk.extent.width;
         uint32_t th = ti->vk.extent.height;
         texpix = (const uint8_t *)ti->mem->map + ti->mem_offset;
         texpix_len = tw * th * 4u;
         texdesc.width = tw;
         texdesc.height = th;
         texdesc.data_len = texpix_len;
         texdesc.sampler_flags = 0;
         struct infinigpu_sampler *smp = cmd->bound_desc_set->sampler;
         if (smp) {
            if (smp->linear)
               texdesc.sampler_flags |= INFINIGPU_SAMPLER_LINEAR;
            if (smp->repeat)
               texdesc.sampler_flags |= INFINIGPU_SAMPLER_REPEAT;
         }
         tex_count = 1;
      }
   }

   /* Phase-2c uniform buffer: if a UBO is bound in the (single) descriptor set, forward its bytes so
    * the host binds them for the shader's var<uniform> block. buf->map already includes the
    * BindBufferMemory bind offset, so add ONLY the write offset (never double-count). tex_binding is
    * forwarded so the host places the texture where the shader declares it (composes with the UBO). */
   const uint8_t *ubo = NULL;
   uint32_t ubo_len = 0;
   uint32_t ubo_binding = 0;
   uint32_t tex_binding = 0;
   struct infinigpu_descriptor_set *ds = cmd->bound_desc_set;
   if (ds) {
      tex_binding = ds->tex_binding;
      if (ds->ubo_buffer && ds->ubo_buffer->map && ds->ubo_offset < ds->ubo_buffer->total_size) {
         /* Clamp to the bytes actually in the buffer past the write offset, exactly like the vertex
          * (vavail) and index (iavail) paths above. A non-conformant app can bind an explicit
          * VkDescriptorBufferInfo.range that overruns the buffer (offset+range > total_size); without
          * this clamp the encoder would memcpy past the mapped region — an OOB read of the guest's own
          * memory. VK_WHOLE_SIZE is already exactly the available bytes. */
         uint64_t uavail = ds->ubo_buffer->total_size - ds->ubo_offset;
         uint64_t range = (ds->ubo_range == VK_WHOLE_SIZE) ? uavail : ds->ubo_range;
         if (range > uavail)
            range = uavail;
         if (range > 0 && range <= 65536) {
            ubo = (const uint8_t *)ds->ubo_buffer->map + ds->ubo_offset;
            ubo_len = (uint32_t)range;
            ubo_binding = ds->ubo_binding;
         }
      }
   }

   const size_t cap = 256 + vs->spirv_size + fs->spirv_size +
                      strlen(vs->entrypoint) + 1 + strlen(fs->entrypoint) + 1 +
                      (size_t)p->attr_count * sizeof(struct VertexAttrWire) +
                      (size_t)cmd->draw_count * sizeof(struct DrawCmdWire) +
                      (size_t)tex_count * sizeof(struct TextureDescWire) +
                      vlen + ilen + cmd->push_const_len + ubo_len + texpix_len;
   uint8_t *payload = malloc(cap);
   if (!payload) {
      free(draws);
      return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);
   }

   /* EDS1 (core Vulkan 1.3): for each state the bound pipeline declared DYNAMIC *and* the app set via a
    * vkCmdSet*, override the pipeline's static capture with the command buffer's dynamic value. DXVK/
    * VKD3D leave the static pipeline fields at defaults and drive cull/front-face/depth/topology this
    * way, so this resolve is what makes their real state reach the host. Normalize the Vk enums to wire
    * form here; the pure resolver (tested in the conformance crate) does the mask-select + repack. */
   uint32_t raster_flags = 0, depth_flags = 0, topo_final = 0;
   infinigpu_resolve_forwarded_state(
      p->raster_flags, p->depth_flags, topo,
      p->dynamic_mask, cmd->dyn_set_mask,
      cmd->dyn_cull_mode & INFINIGPU_CULL_MASK,
      cmd->dyn_front_face == VK_FRONT_FACE_CLOCKWISE ? 1u : 0u,
      cmd->dyn_depth_test != VK_FALSE ? 1u : 0u,
      cmd->dyn_depth_write != VK_FALSE ? 1u : 0u,
      (uint32_t)cmd->dyn_depth_compare & 0x7u,
      cmd->dyn_topology == VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP
         ? INFINIGPU_VK_TOPOLOGY_TRIANGLE_STRIP
         : INFINIGPU_VK_TOPOLOGY_TRIANGLE_LIST,
      &raster_flags, &depth_flags, &topo_final);

   /* scanout_addr=0: the KMD overwrites it with the target BO's dma_addr. */
   const size_t n = infinigpu_encode_forwarded_cmdlist(
      payload, cap, width, height, bg, 0,
      vs->spirv, vs->spirv_size / 4, fs->spirv, fs->spirv_size / 4,
      vs->entrypoint, fs->entrypoint,
      p->vertex_stride, attrs, p->attr_count,
      vdata, vlen, idata, ilen, cmd->index_type,
      topo_final, depth_flags,
      cmd->push_const, cmd->push_const_len,
      ubo, ubo_len, ubo_binding,
      draws, cmd->draw_count,
      tex_count ? &texdesc : NULL, tex_count, tex_binding, texpix, texpix_len,
      raster_flags);
   free(draws);
   if (n == 0) {
      free(payload);
      return vk_errorf(dev, VK_ERROR_UNKNOWN, "cmdlist payload did not fit");
   }

   const int ret = infinigpu_submit_forwarded(drm_fd, img->mem->gem_handle,
                                               width, height, payload, (uint32_t)n);
   free(payload);
   if (ret != 0)
      return vk_errorf(dev, VK_ERROR_DEVICE_LOST, "SUBMIT_FORWARDED failed (%d)", ret);
   return VK_SUCCESS;
}

/* Forward one command buffer's recorded draw to the host, then run its deferred
 * copies. A command buffer with no draw (e.g. a pure clear/copy) skips the ioctl. */
static VkResult
infinigpu_replay_cmd_buffer(struct infinigpu_device *dev,
                            struct infinigpu_cmd_buffer *cmd)
{
   int drm_fd = dev->physical_device->drm_fd;

   /* Texture uploads first, so a staged sampled texture is in the image's memory before any draw
    * (in this or a later command buffer) reads it. */
   for (uint32_t u = 0; u < cmd->upload_count; u++)
      infinigpu_run_upload(&cmd->uploads[u]);

   if (cmd->draw_count > 0 && cmd->color_att && cmd->bound_pipeline) {
      struct infinigpu_pipeline *p = cmd->bound_pipeline;
      const struct infinigpu_pipeline_stage *vs = NULL, *fs = NULL;
      for (uint32_t s = 0; s < p->stage_count; s++) {
         if (p->stages[s].stage == VK_SHADER_STAGE_VERTEX_BIT)
            vs = &p->stages[s];
         else if (p->stages[s].stage == VK_SHADER_STAGE_FRAGMENT_BIT)
            fs = &p->stages[s];
      }
      if (!vs || !fs)
         return vk_errorf(dev, VK_ERROR_UNKNOWN,
                          "forwarded draw needs a vertex + fragment stage");

      struct infinigpu_image *img = cmd->color_att->image;
      if (!img || !img->mem)
         return vk_errorf(dev, VK_ERROR_UNKNOWN,
                          "color attachment has no bound memory");

      /* Phase-2b: a pipeline that reads a vertex buffer (non-zero stride) takes the command-list
       * path (real mesh); otherwise the Phase-1 bufferless path (SM-generated vertices). */
      if (p->vertex_stride > 0 && cmd->vbuf) {
         VkResult r = infinigpu_replay_cmdlist(dev, cmd, p, vs, fs, img);
         if (r != VK_SUCCESS)
            return r;
      } else {
         const uint32_t width = img->vk.extent.width;
         const uint32_t height = img->vk.extent.height;
         float bg[4] = { 0.0f, 0.0f, 0.0f, 1.0f };
         if (cmd->has_clear)
            for (int c = 0; c < 4; c++)
               bg[c] = cmd->clear_value.float32[c];

         const uint32_t topo =
            (p->topology == VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP)
               ? INFINIGPU_VK_TOPOLOGY_TRIANGLE_STRIP
               : INFINIGPU_VK_TOPOLOGY_TRIANGLE_LIST;

         const size_t cap = 128 + vs->spirv_size + fs->spirv_size +
                            strlen(vs->entrypoint) + 1 + strlen(fs->entrypoint) + 1;
         uint8_t *payload = malloc(cap);
         if (!payload)
            return vk_error(dev, VK_ERROR_OUT_OF_HOST_MEMORY);

         /* scanout_addr=0: the KMD overwrites it with the target BO's dma_addr. */
         const size_t n = infinigpu_encode_forwarded(
            payload, cap, width, height, bg, 0, cmd->draw_vertex_count, topo,
            vs->spirv, vs->spirv_size / 4, fs->spirv, fs->spirv_size / 4,
            vs->entrypoint, fs->entrypoint);
         if (n == 0) {
            free(payload);
            return vk_errorf(dev, VK_ERROR_UNKNOWN, "forwarded payload did not fit");
         }

         const int ret = infinigpu_submit_forwarded(drm_fd, img->mem->gem_handle,
                                                     width, height, payload, (uint32_t)n);
         free(payload);
         if (ret != 0)
            return vk_errorf(dev, VK_ERROR_DEVICE_LOST,
                             "SUBMIT_FORWARDED failed (%d)", ret);
      }
   }

   for (uint32_t c = 0; c < cmd->copy_count; c++)
      infinigpu_run_copy(&cmd->copies[c]);

   return VK_SUCCESS;
}

VkResult
infinigpu_queue_submit(struct vk_queue *vk_queue, struct vk_queue_submit *submit)
{
   struct infinigpu_queue *queue =
      container_of(vk_queue, struct infinigpu_queue, vk);
   struct infinigpu_device *dev = queue->device;

   /* 1. Honor waits. Synchronous driver => already signaled, returns at once. */
   VkResult result = vk_sync_wait_many(&dev->vk, submit->wait_count, submit->waits,
                                       VK_SYNC_WAIT_COMPLETE, UINT64_MAX);
   if (result != VK_SUCCESS)
      return result;

   /* 2. Forward each command buffer's draw (blocking ioctl) + deferred copies. */
   for (uint32_t i = 0; i < submit->command_buffer_count; i++) {
      struct infinigpu_cmd_buffer *cmd =
         container_of(submit->command_buffers[i], struct infinigpu_cmd_buffer, vk);
      result = infinigpu_replay_cmd_buffer(dev, cmd);
      if (result != VK_SUCCESS)
         return result;
   }

   /* 3. Work is complete — signal all signal syncs (fences/semaphores). */
   for (uint32_t i = 0; i < submit->signal_count; i++) {
      result = vk_sync_signal(&dev->vk, submit->signals[i].sync,
                              submit->signals[i].signal_value);
      if (result != VK_SUCCESS)
         return result;
   }

   return VK_SUCCESS;
}
