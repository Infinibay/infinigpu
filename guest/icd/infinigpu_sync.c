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
#include "util/format_srgb.h"
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
               /* GPU_MULTI_WAIT is required of the point type by vk_sync_timeline
                * (vk_sync_timeline_type_validate) — the emulated timeline the ICD
                * registers for OpenGL/Zink is built on this binary type. Honest here:
                * queue submit honours all waits via vk_sync_wait_many (any count), and
                * IMMEDIATE synchronous submit means they are already signalled. */
               VK_SYNC_FEATURE_GPU_MULTI_WAIT |
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

/* Pack a float clear colour to a 4-byte RGBA/BGRA UNORM/SRGB texel (the formats zink uses for FBO
 * and swapchain colour). Vulkan specifies a clear value in linear space, so apply the sRGB transfer
 * for _SRGB formats (alpha stays linear). Returns false for any format we don't pack. */
static bool
infinigpu_pack_clear_rgba8(VkFormat fmt, const float f[4], uint8_t out[4])
{
   bool bgra, srgb;
   switch (fmt) {
   case VK_FORMAT_R8G8B8A8_UNORM: bgra = false; srgb = false; break;
   case VK_FORMAT_R8G8B8A8_SRGB:  bgra = false; srgb = true;  break;
   case VK_FORMAT_B8G8R8A8_UNORM: bgra = true;  srgb = false; break;
   case VK_FORMAT_B8G8R8A8_SRGB:  bgra = true;  srgb = true;  break;
   default: return false;
   }
   uint8_t c[4];
   for (int i = 0; i < 4; i++) {
      float v = f[i] < 0.0f ? 0.0f : (f[i] > 1.0f ? 1.0f : f[i]);
      if (srgb && i < 3)
         c[i] = util_format_linear_float_to_srgb_8unorm(v);
      else
         c[i] = (uint8_t)(v * 255.0f + 0.5f);
   }
   if (bgra) { out[0] = c[2]; out[1] = c[1]; out[2] = c[0]; out[3] = c[3]; }
   else      { out[0] = c[0]; out[1] = c[1]; out[2] = c[2]; out[3] = c[3]; }
   return true;
}

/* Apply a render-pass LOAD_OP_CLEAR to the colour attachment when a command buffer clears but records
 * NO draw (e.g. glClear followed by glReadPixels). The forwarded-draw path folds the clear into the
 * host render (bg[]), but a draw-less command buffer skips the ioctl entirely (see the comment in
 * infinigpu_replay_cmd_buffer) — so without this the image keeps its stale/zero contents and the
 * deferred image->buffer readback returns zeros. The attachment is a host-mapped, host-coherent dumb
 * buffer, so realise the clear as a CPU fill of the packed clear colour over the render area — no GPU
 * round-trip, and equivalent to a hardware clear for a synchronous single-submit driver. */
static void
infinigpu_run_clear(struct infinigpu_image *img, const VkClearColorValue *col,
                    const VkRect2D *area)
{
   if (!img || !img->mem || !img->mem->map)
      return;
   if (vk_format_get_blocksize(img->vk.format) != 4)
      return;   /* colour clear only — depth/stencil + non-32bpp are out of the readback path's scope */
   uint8_t px[4];
   if (!infinigpu_pack_clear_rgba8(img->vk.format, col->float32, px))
      return;

   /* Clamp the render area to the image. A zero-sized area (never set) means the whole image. */
   uint32_t x0 = area ? (uint32_t)area->offset.x : 0;
   uint32_t y0 = area ? (uint32_t)area->offset.y : 0;
   uint32_t w = (area && area->extent.width)  ? area->extent.width  : img->vk.extent.width;
   uint32_t h = (area && area->extent.height) ? area->extent.height : img->vk.extent.height;
   if (x0 >= img->vk.extent.width || y0 >= img->vk.extent.height)
      return;
   if (x0 + w > img->vk.extent.width)  w = img->vk.extent.width - x0;
   if (y0 + h > img->vk.extent.height) h = img->vk.extent.height - y0;

   char *base = (char *)img->mem->map + img->mem_offset;
   for (uint32_t y = 0; y < h; y++) {
      char *row = base + (uint64_t)(y0 + y) * img->row_pitch + (uint64_t)x0 * 4;
      for (uint32_t x = 0; x < w; x++)
         memcpy(row + (uint64_t)x * 4, px, 4);
   }
}

/* Forward an encoded draw payload to the host so the A5000 renders it into `img`'s pixels.
 *
 * The SUBMIT_FORWARDED uAPI DMA-writes the rendered RGBA8 to the target BO's BASE (the KMD patches
 * scanout_addr = the gem's dma_addr; there is NO sub-BO offset field). But zink sub-allocates: it
 * binds a colour image at img->mem_offset INSIDE a shared VkDeviceMemory whose base holds OTHER
 * resources. Rendering to the base would (a) land the pixels where the image ISN'T — the readback
 * then reads base+mem_offset and sees all zeros (the observed "black render"), and (b) clobber the
 * neighbour resource sitting at offset 0. So when the image is sub-allocated, render into a PRIVATE
 * scratch BO (offset 0) and CPU-blit the tightly-packed result into the image's real, row_pitch-
 * strided rows. A dedicated allocation (mem_offset == 0) keeps the direct, zero-copy path — and is
 * exactly the case the coherency regression test exercises, so that path is unchanged.
 *
 * TODO(perf): add a bo_offset field to drm_infinigpu_submit_forwarded so the KMD can patch
 * scanout_addr = dma_addr + mem_offset and the host writes straight to base+mem_offset — dropping the
 * scratch BO + per-draw blit. That is a uAPI+KMD change (needs a guest module reload/reboot). */
static VkResult
infinigpu_forward_to_image(struct infinigpu_device *dev, struct infinigpu_image *img,
                           uint32_t width, uint32_t height, const uint8_t *payload, uint32_t n)
{
   const int drm_fd = dev->physical_device->drm_fd;

   if (img->mem_offset == 0) {
      const int ret = infinigpu_submit_forwarded(drm_fd, img->mem->gem_handle, width, height,
                                                 payload, n);
      if (ret != 0)
         return vk_errorf(dev, VK_ERROR_DEVICE_LOST, "SUBMIT_FORWARDED failed (%d)", ret);
      return VK_SUCCESS;
   }

   const uint64_t fb_bytes = (uint64_t)width * height * 4u;
   uint32_t sh = 0;
   uint64_t ssz = 0;
   if (infinigpu_dumb_alloc(drm_fd, fb_bytes, &sh, &ssz) != 0)
      return vk_errorf(dev, VK_ERROR_OUT_OF_DEVICE_MEMORY, "scratch render BO alloc failed");
   void *smap = infinigpu_dumb_map(drm_fd, sh, ssz);

   IGPU_TRACE("forward: scratch gem=%u (image gem=%u off=%llu) %ux%u", sh, img->mem->gem_handle,
              (unsigned long long)img->mem_offset, width, height);
   const int ret = infinigpu_submit_forwarded(drm_fd, sh, width, height, payload, n);
   if (ret == 0 && smap) {
      const char *src = (const char *)smap;                 /* tightly packed: width*4 per row */
      char *dst = (char *)img->mem->map + img->mem_offset;  /* image: row_pitch per row */
      const size_t rb = (size_t)width * 4u;
      for (uint32_t y = 0; y < height; y++)
         memcpy(dst + (uint64_t)y * img->row_pitch, src + (uint64_t)y * rb, rb);
   }
   if (smap)
      infinigpu_dumb_unmap(smap, ssz);
   infinigpu_gem_close(drm_fd, sh);

   if (ret != 0)
      return vk_errorf(dev, VK_ERROR_DEVICE_LOST, "SUBMIT_FORWARDED failed (%d)", ret);
   return VK_SUCCESS;
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

   /* Vertex source: a MESH binds a vertex buffer (stride>0 → whole vertices from the
    * CmdBindVertexBuffers offset to the buffer end); a BUFFERLESS draw (stride==0, e.g. vkcube pulling
    * vertices from a UBO by gl_VertexIndex) binds none — vdata=NULL, vlen=0, and the shader reads the
    * forwarded UBO/SSBO captured below. The host's replay builds an empty vertex-input for stride==0. */
   struct infinigpu_buffer *vb = cmd->vbuf;
   IGPU_TRACE("cmdlist: stride=%u vbuf=%p map=%p total=%llu voff=%llu attrs=%u vs=%uB fs=%uB draws=%u",
              p->vertex_stride, (void *)vb, vb ? vb->map : NULL,
              vb ? (unsigned long long)vb->total_size : 0ull,
              (unsigned long long)cmd->vbuf_offset, p->attr_count,
              vs->spirv_size, fs->spirv_size, cmd->draw_count);
   const uint8_t *vdata = NULL;
   uint32_t vlen = 0;
   if (p->vertex_stride > 0) {
      if (!vb || !vb->map || cmd->vbuf_offset >= vb->total_size)
         return vk_errorf(dev, VK_ERROR_UNKNOWN, "cmdlist draw without a valid vertex buffer");
      vdata = (const uint8_t *)vb->map + cmd->vbuf_offset;
      uint64_t vavail = vb->total_size - cmd->vbuf_offset;
      vlen = (uint32_t)((vavail / p->vertex_stride) * p->vertex_stride);
      if (vlen == 0)
         return vk_errorf(dev, VK_ERROR_UNKNOWN, "vertex buffer smaller than one vertex");
   }

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

   /* Phase-2c multi-texture: forward the sampled images bound in the set as texture i at tex_binding + 2i
    * (the host's image@b / sampler@b+1 layout — how a material shader declares consecutive texture/sampler
    * pairs). CRITICAL for correctness: the host derives each texture's binding purely from its ARRAY INDEX
    * (base + 2i), so the forwarded array MUST cover EVERY binding the shader declares, with no gaps — else
    * a later texture would land on a binding the shader reads as a different one (silent colour scramble).
    * So we forward ALL image slots (not just 4-bpp ones): a non-4-bpp / unmapped image becomes a 1×1
    * placeholder at ITS binding, keeping the base+2i sequence dense and the host layout complete — the
    * shader samples a benign default there, never the wrong texture. If the slots are NOT a consecutive
    * even sequence (base, base+2, …) — a gap or a sampler-only slot — the base+2i model can't represent
    * them faithfully, so we forward NOTHING (fail-safe untextured, never scrambled). Arbitrary/
    * non-consecutive bindings need a per-texture-binding wire (follow-up). `texpix` is malloc'd + freed. */
   struct infinigpu_descriptor_set *ds = cmd->bound_desc_set;
   struct TextureDescWire texs[INFINIGPU_MAX_SET_TEXTURES];
   uint8_t *texpix = NULL;
   uint32_t texpix_len = 0;
   uint32_t tex_count = 0;
   uint32_t tex_binding = 0;
   if (ds && ds->texture_count > 0) {
      /* Sort ALL image slots by binding (insertion sort; N ≤ 8) — including non-4-bpp, so the sequence
       * stays complete and every shader-declared binding is covered. */
      struct infinigpu_desc_texture *sorted[INFINIGPU_MAX_SET_TEXTURES];
      uint32_t n = ds->texture_count;
      for (uint32_t i = 0; i < n; i++)
         sorted[i] = &ds->textures[i];
      for (uint32_t i = 1; i < n; i++) {
         struct infinigpu_desc_texture *key = sorted[i];
         uint32_t j = i;
         while (j > 0 && sorted[j - 1]->binding > key->binding) {
            sorted[j] = sorted[j - 1];
            j--;
         }
         sorted[j] = key;
      }
      /* Valid only if every slot has a bound image AND the bindings are consecutive-even (base + 2i). */
      uint32_t base = sorted[0]->binding;
      bool ok = true;
      for (uint32_t i = 0; i < n; i++) {
         if (!sorted[i]->image || !sorted[i]->image->image ||
             sorted[i]->binding != base + 2u * i) {
            ok = false;
            break;
         }
      }
      if (ok) {
         /* Total pixel bytes: real w*h*4 for a 4-bpp mapped image, else 4 (a 1×1 placeholder). */
         size_t total = 0;
         for (uint32_t i = 0; i < n; i++) {
            struct infinigpu_image *ti = sorted[i]->image->image;
            bool fwd = ti->mem && ti->mem->map && ti->row_pitch == ti->vk.extent.width * 4u;
            total += fwd ? (size_t)ti->vk.extent.width * ti->vk.extent.height * 4u : 4u;
         }
         texpix = malloc(total ? total : 1);
         if (texpix) {
            size_t off = 0;
            for (uint32_t i = 0; i < n; i++) {
               struct infinigpu_image *ti = sorted[i]->image->image;
               bool fwd = ti->mem && ti->mem->map && ti->row_pitch == ti->vk.extent.width * 4u;
               if (fwd) {
                  uint32_t tw = ti->vk.extent.width;
                  uint32_t th = ti->vk.extent.height;
                  uint32_t bytes = tw * th * 4u;
                  memcpy(texpix + off, (const uint8_t *)ti->mem->map + ti->mem_offset, bytes);
                  off += bytes;
                  texs[i].width = tw;
                  texs[i].height = th;
                  texs[i].data_len = bytes;
               } else {
                  /* 1×1 opaque-white placeholder — keeps this binding present in the host layout so a
                   * non-4-bpp map degrades to a default rather than mis-binding a later texture. */
                  texpix[off] = texpix[off + 1] = texpix[off + 2] = texpix[off + 3] = 255;
                  off += 4;
                  texs[i].width = 1;
                  texs[i].height = 1;
                  texs[i].data_len = 4;
               }
               texs[i].sampler_flags = 0;
               struct infinigpu_sampler *smp = sorted[i]->sampler;
               if (smp) {
                  if (smp->linear)
                     texs[i].sampler_flags |= INFINIGPU_SAMPLER_LINEAR;
                  if (smp->repeat)
                     texs[i].sampler_flags |= INFINIGPU_SAMPLER_REPEAT;
               }
            }
            texpix_len = (uint32_t)total;
            tex_count = n;
            tex_binding = base;
         }
      }
   }

   /* Phase-2c uniform buffer: if a UBO is bound in the (single) descriptor set, forward its bytes so
    * the host binds them for the shader's var<uniform> block. buf->map already includes the
    * BindBufferMemory bind offset, so add ONLY the write offset (never double-count). */
   const uint8_t *ubo = NULL;
   uint32_t ubo_len = 0;
   uint32_t ubo_binding = 0;
   if (ds) {
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

   /* Phase-2c storage buffer (SSBO): identical to the UBO capture but STORAGE_BUFFER and a far larger
    * cap (matches host MAX_SSBO_BYTES = 16 MiB; maxStorageBufferRange is >= 128 MiB). Read-only — the
    * host never writes it back. Same OOB-read clamp as the UBO (an app's explicit range may overrun). */
   const uint8_t *ssbo = NULL;
   uint32_t ssbo_len = 0;
   uint32_t ssbo_binding = 0;
   if (ds) {
      if (ds->ssbo_buffer && ds->ssbo_buffer->map && ds->ssbo_offset < ds->ssbo_buffer->total_size) {
         uint64_t savail = ds->ssbo_buffer->total_size - ds->ssbo_offset;
         uint64_t range = (ds->ssbo_range == VK_WHOLE_SIZE) ? savail : ds->ssbo_range;
         if (range > savail)
            range = savail;
         if (range > 0 && range <= (16u * 1024u * 1024u)) {
            ssbo = (const uint8_t *)ds->ssbo_buffer->map + ds->ssbo_offset;
            ssbo_len = (uint32_t)range;
            ssbo_binding = ds->ssbo_binding;
         }
      }
   }

   const size_t cap = 256 + vs->spirv_size + fs->spirv_size +
                      strlen(vs->entrypoint) + 1 + strlen(fs->entrypoint) + 1 +
                      (size_t)p->attr_count * sizeof(struct VertexAttrWire) +
                      (size_t)cmd->draw_count * sizeof(struct DrawCmdWire) +
                      (size_t)tex_count * sizeof(struct TextureDescWire) +
                      vlen + ilen + cmd->push_const_len + ubo_len + ssbo_len + texpix_len;
   uint8_t *payload = malloc(cap);
   if (!payload) {
      free(draws);
      free(texpix);
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
      ssbo, ssbo_len, ssbo_binding,
      draws, cmd->draw_count,
      tex_count ? texs : NULL, tex_count, tex_binding, texpix, texpix_len,
      raster_flags);
   free(draws);
   free(texpix);
   if (n == 0) {
      free(payload);
      return vk_errorf(dev, VK_ERROR_UNKNOWN, "cmdlist payload did not fit");
   }

   IGPU_TRACE("cmdlist: encoded payload=%zuB vlen=%u ilen=%u -> forward gem=%u off=%llu %ux%u",
              n, vlen, ilen, img->mem->gem_handle, (unsigned long long)img->mem_offset, width, height);
   VkResult fr = infinigpu_forward_to_image(dev, img, width, height, payload, (uint32_t)n);
   free(payload);
   IGPU_TRACE("cmdlist: forward ret=%d", (int)fr);
   return fr;
}

/* Forward one command buffer's recorded draw to the host, then run its deferred
 * copies. A command buffer with no draw (e.g. a pure clear/copy) skips the ioctl. */
static VkResult
infinigpu_replay_cmd_buffer(struct infinigpu_device *dev,
                            struct infinigpu_cmd_buffer *cmd)
{
   IGPU_TRACE("submit cmdbuf: draws=%u clear=%d color_att=%p pipeline=%p uploads=%u copies=%u",
              cmd->draw_count, cmd->has_clear, (void *)cmd->color_att,
              (void *)cmd->bound_pipeline, cmd->upload_count, cmd->copy_count);

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
      IGPU_TRACE("draw branch: vs=%p fs=%p stages=%u vertex_stride=%u vbuf=%p -> %s path",
                 (void *)vs, (void *)fs, p->stage_count, p->vertex_stride, (void *)cmd->vbuf,
                 (p->vertex_stride > 0 && cmd->vbuf) ? "cmdlist" : "bufferless");
      if (!vs || !fs)
         return vk_errorf(dev, VK_ERROR_UNKNOWN,
                          "forwarded draw needs a vertex + fragment stage");

      struct infinigpu_image *img = cmd->color_att->image;
      if (!img || !img->mem)
         return vk_errorf(dev, VK_ERROR_UNKNOWN,
                          "color attachment has no bound memory");

      /* The command-list path carries the full draw state (vertex buffer, UBO/SSBO, textures, dynamic
       * state). Take it for a real MESH (non-zero stride + a bound vertex buffer) OR for a BUFFERLESS
       * draw that still needs a UBO — a shader that pulls its vertices from a uniform block by
       * gl_VertexIndex (vkcube), which the Phase-1 bufferless path can't carry (it forwards no UBO).
       * Only a pipeline with neither a vertex buffer nor a UBO falls to the Phase-1 SM-generated path. */
      const bool has_vbuf = (p->vertex_stride > 0 && cmd->vbuf);
      const bool has_ubo = cmd->bound_desc_set && cmd->bound_desc_set->ubo_buffer;
      if (has_vbuf || has_ubo) {
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

         VkResult fr = infinigpu_forward_to_image(dev, img, width, height, payload, (uint32_t)n);
         free(payload);
         if (fr != VK_SUCCESS)
            return fr;
      }
   } else if (cmd->has_clear && cmd->color_att && cmd->color_att->image) {
      /* Draw-less clear (glClear + readback): no forwarded submit runs, so realise the
       * LOAD_OP_CLEAR on the CPU into the host-mapped attachment before the readback copy. */
      IGPU_TRACE("submit cmdbuf: draw-less clear -> CPU fill of color attachment");
      infinigpu_run_clear(cmd->color_att->image, &cmd->clear_value, &cmd->render_area);
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
