/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * Phase-1 own-remoting Vulkan ICD for the "infinigpu" remote GPU.
 * Based in part on Mesa's lavapipe and venus drivers.
 *
 * The driver never compiles shaders. It captures the app's SPIR-V + draw state
 * and forwards it, over the DRM render node (DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED),
 * to the host, which replays it on a real GPU and DMA-writes the result into a
 * DRM dumb buffer that backs the color image's VkDeviceMemory.
 */

#ifndef INFINIGPU_PRIVATE_H
#define INFINIGPU_PRIVATE_H

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

/* Set INFINIGPU_DEBUG=1 to trace ICD bring-up (host smoke + real guest VM boot).
 * Off by default; never fires in production. Shared by every TU. */
#define IGPU_TRACE(...) do { \
   if (getenv("INFINIGPU_DEBUG")) { \
      fprintf(stderr, "[infinigpu] " __VA_ARGS__); fputc('\n', stderr); \
   } } while (0)

#include "c11/threads.h"
#include "util/list.h"

#include "vk_buffer.h"
#include "vk_command_buffer.h"
#include "vk_command_pool.h"
#include "vk_descriptor_set_layout.h"
#include "vk_device.h"
#include "vk_device_memory.h"
#include "vk_image.h"
#include "vk_instance.h"
#include "vk_physical_device.h"
#include "vk_pipeline_layout.h"
#include "vk_queue.h"
#include "vk_sampler.h"
#include "vk_shader_module.h"
#include "vk_sync.h"
#include "vk_sync_timeline.h"

/* Mesa's common WSI layer (VK_KHR_surface/swapchain + headless/display present).
 * Provides `struct wsi_device` and the three wsi_*_entrypoints tables we merge
 * into our dispatch tables. Header comes in via idep_vulkan_wsi (meson). */
#include "wsi_common.h"

/* Generated (vk_entrypoints_gen.py --prefix infinigpu --proto --weak):
 * declares infinigpu_{instance,physical_device,device}_entrypoints tables and
 * VKAPI_ATTR prototypes for every infinigpu_* entrypoint. */
#include "infinigpu_entrypoints.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Advertised device apiVersion (physical device). */
#define INFINIGPU_API_VERSION VK_API_VERSION_1_3

/* A graphics pipeline forwards at most a vertex + fragment stage for now. */
#define INFINIGPU_MAX_STAGES 2

/* Phase-2b recording limits. The wire is single-binding (interleaved binding 0), so we capture
 * at most this many vertex-input attributes and this many draws per command buffer, and cap
 * push constants at the Vulkan hardware maximum (256 B). */
#define INFINIGPU_MAX_ATTRS 16
#define INFINIGPU_MAX_DRAWS 512
#define INFINIGPU_MAX_PUSH_CONST 256

struct infinigpu_instance {
   struct vk_instance vk;
};

struct infinigpu_physical_device {
   struct vk_physical_device vk;

   /* Open fd of /dev/dri/renderD128 whose drm name == "infinigpu". */
   int drm_fd;

   /* Mesa common-WSI state. Embedded (not a pointer): wsi_device_init fills it,
    * pdev->vk.wsi_device points at it, wsi_device_finish tears it down. Software
    * present path (sw + wants_linear) — see infinigpu_wsi.c. */
   struct wsi_device wsi_device;

   /* CPU binary sync (+ an emulated timeline built on it) registered as the
    * device's supported sync types — see infinigpu_sync.c. */
   const struct vk_sync_type *sync_types[3];
   struct vk_sync_timeline_type sync_timeline_type;
};

/* WSI bring-up/teardown (infinigpu_wsi.c). Called from physical-device
 * init/destroy. init_wsi advertises nothing itself — the surface/swapchain
 * extension entries live in the instance/device extension tables. */
VkResult infinigpu_init_wsi(struct infinigpu_physical_device *pdev);
void infinigpu_finish_wsi(struct infinigpu_physical_device *pdev);

struct infinigpu_queue {
   struct vk_queue vk;
   struct infinigpu_device *device;
};

struct infinigpu_device {
   struct vk_device vk;
   struct infinigpu_physical_device *physical_device;
   struct infinigpu_queue queue;
};

/* ---- VkDeviceMemory: a DRM dumb buffer + its persistent host mmap ---- */
struct infinigpu_device_memory {
   struct vk_device_memory vk;   /* runtime base; auto-parses the alloc pNext */
   uint32_t gem_handle;          /* DRM_IOCTL_MODE_CREATE_DUMB result */
   void *map;                    /* mmap of the dumb buffer, kept for its lifetime */
   uint64_t map_size;            /* actual (page/stride-rounded) mapped bytes */
};

/* ---- VkImage: a single-plane LINEAR R8G8B8A8-style image ---- */
struct infinigpu_image {
   struct vk_image vk;           /* base, filled by vk_image_init */
   uint64_t size;                /* total linear allocation in bytes */
   uint32_t row_pitch;           /* linear row stride = width * blocksize (packed) */
   uint32_t alignment;           /* memory-requirements alignment */
   struct infinigpu_device_memory *mem;  /* bound backing (NULL until BindImageMemory2) */
   uint64_t mem_offset;          /* VkBindImageMemoryInfo::memoryOffset */
};

struct infinigpu_image_view {
   struct vk_image_view vk;      /* base, filled by vk_image_view_init */
   struct infinigpu_image *image;/* the image this view targets (path to bound memory) */
};

struct infinigpu_buffer {
   struct vk_buffer vk;          /* base, filled by vk_buffer_init */
   struct infinigpu_device_memory *mem;
   uint64_t offset;
   void *map;                    /* mem->map + offset, set in BindBufferMemory2 */
   uint64_t total_size;
};

/* ---- VkPipeline: captured forwarding state (never compiled) ---- */
struct infinigpu_pipeline_stage {
   VkShaderStageFlagBits stage;
   char *entrypoint;             /* strdup of VkPipelineShaderStageCreateInfo::pName */
   uint32_t *spirv;              /* memdup of the SPIR-V words */
   uint32_t spirv_size;          /* bytes */
};

/* One captured vertex-input attribute (binding 0), pre-mapped to a wire `vk_vformat`. */
struct infinigpu_vertex_attr {
   uint32_t location;
   uint32_t format;   /* infinigpu_abi vk_vformat (INFINIGPU_VFORMAT_*) */
   uint32_t offset;
};

struct infinigpu_pipeline {
   struct vk_object_base base;
   VkPipelineBindPoint bind_point;
   VkPrimitiveTopology topology;
   uint32_t stage_count;
   struct infinigpu_pipeline_stage stages[INFINIGPU_MAX_STAGES];

   /* Phase-2b vertex-input capture (binding 0 only — the wire is single-binding). `vertex_stride`
    * 0 ⇒ the pipeline reads no vertex buffer, so submit uses the bufferless FORWARDED path. */
   uint32_t vertex_stride;
   uint32_t attr_count;
   struct infinigpu_vertex_attr attrs[INFINIGPU_MAX_ATTRS];
   /* Phase-2d depth-test state, pre-packed as a ForwardedCmdListTail.depth_flags bitfield (0 ⇒ none). */
   uint32_t depth_flags;
   /* Phase-2d-A5 static rasterization+blend state, pre-packed as a ForwardedCmdListTail.raster_flags
    * bitfield (cull mode | front-face-CW? | blend?); 0 ⇒ cull NONE / CCW / blend off (the default). */
   uint32_t raster_flags;
   /* Extended-dynamic-state (EDS1, core Vulkan 1.3) mask: which of the states this driver forwards the
    * pipeline declared DYNAMIC via pDynamicState (INFINIGPU_DYN_*). For each set bit the static field
    * above is IGNORED per spec and the app supplies the value with a vkCmdSet* — resolved at submit from
    * the command buffer's dynamic values (infinigpu_sync.c). 0 ⇒ everything is static (the common path). */
   uint32_t dynamic_mask;
};

/* A dummy pipeline cache (we never cache — SPIR-V is forwarded, not compiled). */
struct infinigpu_pipeline_cache {
   struct vk_object_base base;
};

/* ---- Descriptors (Phase-2c textures/UBO) — the guest half of forwarded textures ----
 * The ICD doesn't run shaders, so a descriptor set is just a capture of the resources bound to it
 * (a sampled image + sampler for now). At submit, the bound set's image RGBA8 pixels are read from
 * its host-mapped memory and forwarded in the command list; the host binds them through its own
 * descriptor set. Descriptor-set LAYOUTs (runtime `vk_descriptor_set_layout`) and samplers carry no
 * driver state beyond the sampler's filter/address mode. */
struct infinigpu_sampler {
   struct vk_sampler vk;   /* MUST be first (runtime base) */
   bool linear;            /* magFilter == VK_FILTER_LINEAR */
   bool repeat;            /* addressModeU == VK_SAMPLER_ADDRESS_MODE_REPEAT */
};

struct infinigpu_descriptor_set_layout {
   struct vk_descriptor_set_layout vk;  /* MUST be first (runtime base, ref-counted) */
};

struct infinigpu_descriptor_pool {
   struct vk_object_base base;
   struct list_head sets;   /* infinigpu_descriptor_set::link — freed on reset/destroy */
};

/* One sampled texture bound in a descriptor set: its image + sampler + the dstBinding the sampled image
 * was written at (image@binding, sampler@binding+1). A real material shader binds several. */
#define INFINIGPU_MAX_SET_TEXTURES 8
struct infinigpu_desc_texture {
   struct infinigpu_image_view *image;
   struct infinigpu_sampler *sampler;
   uint32_t binding;
};

struct infinigpu_descriptor_set {
   struct vk_object_base base;
   struct list_head link;                 /* in its pool's `sets` list */
   struct infinigpu_descriptor_pool *pool;
   /* Phase-2c multi-texture: the sampled images bound here (empty ⇒ untextured). Each carries its own
    * dstBinding; at submit they are sorted by binding and forwarded as texture i at tex_binding + 2i. */
   struct infinigpu_desc_texture textures[INFINIGPU_MAX_SET_TEXTURES];
   uint32_t texture_count;
   /* Phase-2c uniform buffer bound here (NULL ⇒ none). Composes with the textures in the same set at a
    * distinct binding. `ubo_range == VK_WHOLE_SIZE` ⇒ resolve to total_size - ubo_offset at submit. */
   struct infinigpu_buffer *ubo_buffer;
   uint64_t ubo_offset;
   uint64_t ubo_range;
   uint32_t ubo_binding;                  /* dstBinding the UBO was written at */
   /* Phase-2c read-only storage buffer bound here (NULL ⇒ none). Composes with the UBO + textures in the
    * same set at a distinct binding. Same clamp/resolve rules as the UBO; forwarded as bytes (no writeback). */
   struct infinigpu_buffer *ssbo_buffer;
   uint64_t ssbo_offset;
   uint64_t ssbo_range;
   uint32_t ssbo_binding;                 /* dstBinding the SSBO was written at */
};

/* A deferred image->buffer copy, executed at submit AFTER the draw so the host
 * writeback is already in the image's memory. Supports the common readback case. */
#define INFINIGPU_MAX_COPIES 4
struct infinigpu_pending_copy {
   struct infinigpu_image *src;
   struct infinigpu_buffer *dst;
   uint64_t buffer_offset;
   uint32_t buffer_row_length;  /* in texels; 0 => tightly packed (== image width) */
   VkOffset3D image_offset;
   VkExtent3D image_extent;
};

/* A deferred buffer->image copy (texture UPLOAD), executed at submit BEFORE the draw so a sampled
 * texture staged in a buffer is present in the image's (LINEAR-packed) memory when the forwarded
 * draw reads it. The mirror of infinigpu_pending_copy. */
#define INFINIGPU_MAX_UPLOADS 8
struct infinigpu_pending_upload {
   struct infinigpu_buffer *src;
   struct infinigpu_image *dst;
   uint64_t buffer_offset;
   uint32_t buffer_row_length;  /* in texels; 0 => tightly packed (== image width) */
   VkOffset3D image_offset;
   VkExtent3D image_extent;
};

/* One recorded draw (CmdDraw / CmdDrawIndexed) with the dynamic viewport in effect. */
struct infinigpu_draw {
   uint32_t count;            /* vertexCount (non-indexed) or indexCount (indexed) */
   uint32_t instance_count;
   uint32_t first;           /* firstVertex or firstIndex */
   int32_t vertex_offset;    /* CmdDrawIndexed vertexOffset (0 for non-indexed) */
   bool indexed;
   float viewport[4];        /* (x,y,w,h); w == 0 ⇒ host uses the full render target */
};

/* ---- VkCommandBuffer: direct-record model (no enqueue/replay) ---- */
struct infinigpu_cmd_buffer {
   struct vk_command_buffer vk;  /* MUST be first */
   struct infinigpu_device *device;

   /* accumulated recording state */
   struct infinigpu_pipeline *bound_pipeline;   /* CmdBindPipeline */
   struct infinigpu_image_view *color_att;      /* CmdBeginRendering color attachment 0 */
   VkRect2D render_area;
   VkClearColorValue clear_value;               /* pColorAttachments[0].clearValue (LOAD_OP_CLEAR) */
   bool has_clear;
   uint32_t draw_vertex_count;                  /* last CmdDraw vertexCount (bufferless fallback path) */
   uint32_t draw_count;                         /* number of draws recorded (entries in draws[]) */

   /* Phase-2b bound geometry (binding 0 + index buffer) + the multi-draw list. */
   struct infinigpu_buffer *vbuf;               /* CmdBindVertexBuffers2 binding 0 */
   uint64_t vbuf_offset;
   struct infinigpu_buffer *ibuf;               /* CmdBindIndexBuffer2 */
   uint64_t ibuf_offset;
   uint32_t index_type;                         /* INFINIGPU_INDEX_TYPE_U16 / _U32 */
   bool has_dyn_viewport;
   float dyn_viewport[4];                       /* last CmdSetViewport viewport 0 (x,y,w,h) */

   /* Extended-dynamic-state values the app set via vkCmdSet* (EDS1, core 1.3). `dyn_set_mask` is which
    * (INFINIGPU_DYN_*) have been set on this recording; the values are consulted at submit ONLY for the
    * states the bound pipeline declared dynamic (pipeline->dynamic_mask). Reset in CmdBufferBegin. */
   uint32_t dyn_set_mask;
   uint32_t dyn_cull_mode;                      /* VkCullModeFlags from vkCmdSetCullMode */
   VkFrontFace dyn_front_face;                  /* vkCmdSetFrontFace */
   VkBool32 dyn_depth_test;                     /* vkCmdSetDepthTestEnable */
   VkBool32 dyn_depth_write;                    /* vkCmdSetDepthWriteEnable */
   VkCompareOp dyn_depth_compare;               /* vkCmdSetDepthCompareOp */
   VkPrimitiveTopology dyn_topology;            /* vkCmdSetPrimitiveTopology */

   struct infinigpu_draw draws[INFINIGPU_MAX_DRAWS];
   uint32_t push_const_len;                      /* highest push-constant byte written */
   uint8_t push_const[INFINIGPU_MAX_PUSH_CONST]; /* CmdPushConstants payload (offset-placed) */

   /* Phase-2c: the bound descriptor set carrying a sampled texture (first one with an image). */
   struct infinigpu_descriptor_set *bound_desc_set;

   /* Texture uploads (buffer->image), run BEFORE the forwarded draw. */
   struct infinigpu_pending_upload uploads[INFINIGPU_MAX_UPLOADS];
   uint32_t upload_count;

   struct infinigpu_pending_copy copies[INFINIGPU_MAX_COPIES];
   uint32_t copy_count;
};

/* ---- The driver's CPU binary sync (infinigpu_sync.c) ---- */
struct infinigpu_sync {
   struct vk_sync base;
   mtx_t lock;
   cnd_t changed;
   bool signaled;
};

/* struct vk_*::base is the vk_object_base used by the casts. */
VK_DEFINE_HANDLE_CASTS(infinigpu_instance, vk.base, VkInstance,
                       VK_OBJECT_TYPE_INSTANCE)
VK_DEFINE_HANDLE_CASTS(infinigpu_physical_device, vk.base, VkPhysicalDevice,
                       VK_OBJECT_TYPE_PHYSICAL_DEVICE)
VK_DEFINE_HANDLE_CASTS(infinigpu_device, vk.base, VkDevice,
                       VK_OBJECT_TYPE_DEVICE)
VK_DEFINE_HANDLE_CASTS(infinigpu_queue, vk.base, VkQueue,
                       VK_OBJECT_TYPE_QUEUE)
VK_DEFINE_HANDLE_CASTS(infinigpu_cmd_buffer, vk.base, VkCommandBuffer,
                       VK_OBJECT_TYPE_COMMAND_BUFFER)

VK_DEFINE_NONDISP_HANDLE_CASTS(infinigpu_device_memory, vk.base, VkDeviceMemory,
                               VK_OBJECT_TYPE_DEVICE_MEMORY)
VK_DEFINE_NONDISP_HANDLE_CASTS(infinigpu_image, vk.base, VkImage,
                               VK_OBJECT_TYPE_IMAGE)
VK_DEFINE_NONDISP_HANDLE_CASTS(infinigpu_image_view, vk.base, VkImageView,
                               VK_OBJECT_TYPE_IMAGE_VIEW)
VK_DEFINE_NONDISP_HANDLE_CASTS(infinigpu_buffer, vk.base, VkBuffer,
                               VK_OBJECT_TYPE_BUFFER)
VK_DEFINE_NONDISP_HANDLE_CASTS(infinigpu_pipeline, base, VkPipeline,
                               VK_OBJECT_TYPE_PIPELINE)
VK_DEFINE_NONDISP_HANDLE_CASTS(infinigpu_pipeline_cache, base, VkPipelineCache,
                               VK_OBJECT_TYPE_PIPELINE_CACHE)
VK_DEFINE_NONDISP_HANDLE_CASTS(infinigpu_sampler, vk.base, VkSampler,
                               VK_OBJECT_TYPE_SAMPLER)
VK_DEFINE_NONDISP_HANDLE_CASTS(infinigpu_descriptor_pool, base, VkDescriptorPool,
                               VK_OBJECT_TYPE_DESCRIPTOR_POOL)
VK_DEFINE_NONDISP_HANDLE_CASTS(infinigpu_descriptor_set, base, VkDescriptorSet,
                               VK_OBJECT_TYPE_DESCRIPTOR_SET)

static inline struct infinigpu_sync *
infinigpu_sync_as(struct vk_sync *sync)
{
   return (struct infinigpu_sync *)sync;
}

/* Minimal supported-extension tables. */
extern const struct vk_instance_extension_table infinigpu_instance_extensions;
extern const struct vk_device_extension_table infinigpu_device_extensions;

/* The device's command-buffer vtable (infinigpu_cmd_buffer.c). */
extern const struct vk_command_buffer_ops infinigpu_cmd_buffer_ops;

/* The driver's CPU binary sync type (infinigpu_sync.c). */
extern const struct vk_sync_type infinigpu_sync_type;

/* infinigpu_physical_device.c */
VkResult infinigpu_enumerate_physical_devices(struct vk_instance *vk_instance);
void infinigpu_physical_device_destroy(struct vk_physical_device *vk_pdev);

/* infinigpu_sync.c — the vk_queue.driver_submit hook. */
VkResult infinigpu_queue_submit(struct vk_queue *vk_queue,
                                struct vk_queue_submit *submit);

#ifdef __cplusplus
}
#endif

#endif /* INFINIGPU_PRIVATE_H */
