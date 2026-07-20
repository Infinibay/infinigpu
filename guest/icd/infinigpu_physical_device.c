/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * VkPhysicalDevice: enumerate one device backed by the infinigpu PRIMARY node
 * (/dev/dri/card*, selected by drm name — NOT the render node, which forbids the
 * dumb-buffer ioctls our allocations need), fabricate properties, and answer the *2 property
 * queries.  Cribbed from lavapipe (lvp_physical_device_init /
 * lvp_enumerate_physical_devices / lvp_get_properties / the *2 getters).
 *
 * NOTE ON Properties2 / Features2: they are intentionally NOT hand-written.
 * The lite runtime ships GENERATED vk_common_GetPhysicalDeviceProperties2 /
 * vk_common_GetPhysicalDeviceFeatures2 (src/vulkan/util/
 * vk_physical_device_properties_gen.py / _features_gen.py) that copy from
 * pdevice->properties / pdevice->supported_features and walk the whole pNext
 * chain (incl. VkPhysicalDeviceDriverProperties / VkPhysicalDeviceVulkan1[123]
 * Properties, which is what surfaces driverID/driverName in vulkaninfo).
 * vk_physical_device_init() adds those common entrypoints to our dispatch
 * table (with overwrite=false), so we only fill the structs.  This is exactly
 * how lavapipe does it -- hand-writing Properties2 would be strictly worse.
 * Only QueueFamilyProperties2 and MemoryProperties2 have no vk_common *2
 * implementation, so those two we DO write. */

#include "infinigpu_private.h"

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* Set INFINIGPU_DEBUG=1 to trace ICD bring-up (useful both for the host smoke
 * and for the real guest VM boot). Off by default; never fires in production. */
#define IGPU_TRACE(...) do { \
   if (getenv("INFINIGPU_DEBUG")) { \
      fprintf(stderr, "[infinigpu] " __VA_ARGS__); fputc('\n', stderr); \
   } } while (0)

#include <xf86drm.h>

#include "vk_alloc.h"
#include "vk_log.h"
#include "vk_util.h"

#define INFINIGPU_RENDER_NODE "/dev/dri/renderD128"
#define INFINIGPU_DRM_NAME    "infinigpu"

/* Phase 0: advertise no device extensions. */
const struct vk_device_extension_table infinigpu_device_extensions = {
   0,
};

static void
infinigpu_get_properties(const struct infinigpu_physical_device *pdev,
                         struct vk_properties *p)
{
   /* struct vk_properties is the flat Mesa properties struct (generated
    * src/vulkan/util/vk_physical_device_properties.h); designated init zeroes
    * every field we omit (UUIDs, limits, sparseProperties, ...). */
   *p = (struct vk_properties) {
      /* Vulkan 1.0 core */
      .apiVersion    = INFINIGPU_API_VERSION,
      .driverVersion = VK_MAKE_VERSION(0, 1, 0),
      .vendorID      = 0x0000F00F,        /* made-up */
      .deviceID      = 0x000A5000,        /* made-up */
      .deviceType    = VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU,

      /* A few nonzero limits so tools don't divide by zero; the rest are 0. */
      .maxImageDimension1D              = 16384,
      .maxImageDimension2D              = 16384,
      .maxImageDimension3D              = 2048,
      .maxImageDimensionCube            = 16384,
      .maxImageArrayLayers              = 2048,
      .maxMemoryAllocationCount         = 4096,
      .maxSamplerAllocationCount        = 4000,
      .bufferImageGranularity           = 64,
      .maxBoundDescriptorSets           = 8,
      .maxPushConstantsSize             = 256,
      .minMemoryMapAlignment            = 4096,
      .maxComputeSharedMemorySize       = 49152,
      .maxComputeWorkGroupCount         = { 65535, 65535, 65535 },
      .maxComputeWorkGroupInvocations   = 1024,
      .maxComputeWorkGroupSize          = { 1024, 1024, 64 },
      .maxViewports                     = 16,
      .maxViewportDimensions            = { 16384, 16384 },
      .viewportBoundsRange              = { -32768.0f, 32768.0f },

      /* Vulkan 1.1 */
      .deviceLUIDValid = false,
      .deviceNodeMask  = 0,

      /* Vulkan 1.2 -- VkPhysicalDeviceDriverProperties.  driverName is what
       * makes vulkaninfo say "infinigpu" (NOT lavapipe).  NOTE: there is no
       * "infinigpu" VkDriverId enum value; leave the numeric driverID as a
       * placeholder until one is registered with Khronos. */
      .driverID = (VkDriverId)0,
      .conformanceVersion = (VkConformanceVersion){ 1, 3, 0, 0 },
   };

   snprintf(p->deviceName, VK_MAX_PHYSICAL_DEVICE_NAME_SIZE,
            "infinigpu (A5000 remote)");
   snprintf(p->driverName, VK_MAX_DRIVER_NAME_SIZE, "infinigpu");
   snprintf(p->driverInfo, VK_MAX_DRIVER_INFO_SIZE, "infinigpu phase-0");
   /* pipelineCacheUUID / deviceUUID / driverUUID left all-zero. */
}

/* Phase 1 advertises exactly the features the forwarded-triangle path uses: a
 * 1.3 device with dynamic rendering (CmdBeginRendering) and synchronization2
 * (CmdPipelineBarrier2). We do NOT advertise timelineSemaphore — the driver runs
 * a binary CPU sync in IMMEDIATE submit mode (see infinigpu_sync.c). */
static void
infinigpu_get_features(struct vk_features *f)
{
   *f = (struct vk_features){
      .dynamicRendering = true,
      .synchronization2 = true,
   };
}

static VkResult
infinigpu_physical_device_init(struct infinigpu_physical_device *pdev,
                               struct infinigpu_instance *instance,
                               int drm_fd)
{
   struct vk_physical_device_dispatch_table dispatch_table;
   vk_physical_device_dispatch_table_from_entrypoints(
      &dispatch_table, &infinigpu_physical_device_entrypoints, true);

   struct vk_properties properties;
   infinigpu_get_properties(pdev, &properties);
   struct vk_features features;
   infinigpu_get_features(&features);

   /* vk_physical_device_init: src/vulkan/runtime/vk_physical_device.h
    *   (pdev, instance, supported_extensions, supported_features,
    *    properties, dispatch_table). */
   IGPU_TRACE("pdev_init: calling vk_physical_device_init");
   VkResult result = vk_physical_device_init(&pdev->vk, &instance->vk,
                                             &infinigpu_device_extensions,
                                             &features,
                                             &properties,
                                             &dispatch_table);
   IGPU_TRACE("pdev_init: vk_physical_device_init -> %d", result);
   if (result != VK_SUCCESS)
      return result;

   /* Register the driver's binary CPU sync. Binary-only (no timeline) keeps the
    * device in IMMEDIATE submit mode, where driver_submit runs synchronously on
    * the caller's thread and blocks on the forwarded-draw ioctl. */
   pdev->sync_types[0] = &infinigpu_sync_type;
   pdev->sync_types[1] = NULL;
   pdev->vk.supported_sync_types = pdev->sync_types;

   pdev->drm_fd = drm_fd;
   return VK_SUCCESS;
}

void
infinigpu_physical_device_destroy(struct vk_physical_device *vk_pdev)
{
   struct infinigpu_physical_device *pdev =
      container_of(vk_pdev, struct infinigpu_physical_device, vk);

   if (pdev->drm_fd >= 0)
      close(pdev->drm_fd);
   vk_physical_device_finish(&pdev->vk);
   vk_free(&pdev->vk.instance->alloc, pdev);
}

/* Open the infinigpu DRM node that backs allocations + submits.
 *
 * IMPORTANT: buffer storage is DRM "dumb" buffers (CREATE_DUMB/MAP_DUMB), and the
 * DRM core forbids those ioctls on RENDER nodes — they lack DRM_RENDER_ALLOW, so a
 * render-node fd gets EACCES. They ARE permitted on the PRIMARY node (card*) with no
 * DRM master, and our render-allowed ioctl (SUBMIT_FORWARDED) works there too. So we
 * bind the PRIMARY node, selected by DRM driver name — never the render node.
 *
 * Returns an fd (caller owns) or -1 if no infinigpu primary node is present. In smoke
 * mode, falls back to the render node so the bring-up path is still exercised. */
static int
infinigpu_open_node(bool smoke)
{
   for (int i = 0; i < 16; i++) {
      char path[32];
      snprintf(path, sizeof(path), "/dev/dri/card%d", i);
      int fd = open(path, O_RDWR | O_CLOEXEC);
      if (fd < 0)
         continue;
      /* drmGetVersion / drmFreeVersion: xf86drm.h (libdrm). ->name is NUL-term. */
      drmVersionPtr v = drmGetVersion(fd);
      const bool match = v && v->name && strcmp(v->name, INFINIGPU_DRM_NAME) == 0;
      if (v)
         drmFreeVersion(v);
      if (match) {
         IGPU_TRACE("open node: %s is the infinigpu primary node -> fd %d", path, fd);
         return fd;
      }
      close(fd);
   }
   if (smoke) {
      int fd = open(INFINIGPU_RENDER_NODE, O_RDWR | O_CLOEXEC);
      IGPU_TRACE("open node: smoke fallback open(%s) -> %d", INFINIGPU_RENDER_NODE, fd);
      return fd;
   }
   return -1;
}

VkResult
infinigpu_enumerate_physical_devices(struct vk_instance *vk_instance)
{
   struct infinigpu_instance *instance =
      container_of(vk_instance, struct infinigpu_instance, vk);

   /* INFINIGPU_SMOKE_ANY_NODE: dev/test escape hatch. The real infinigpu DRM
    * device only exists inside a guest VM; on a bare host card0 is the physical
    * GPU (e.g. nvidia, often not even group-openable). In smoke mode we (a) skip
    * the drm name check (accept the render node) and, (b) if no node can be opened
    * at all, fabricate a device with NO backing fd (drm_fd = -1) — Phase 0 renders
    * nothing, so the whole load -> enumerate -> CreateDevice -> property-query
    * path is exercised without a real device. Never set it in production. */
   const bool smoke = getenv("INFINIGPU_SMOKE_ANY_NODE") != NULL;
   IGPU_TRACE("enumerate: called (smoke=%d)", smoke);

   int fd = infinigpu_open_node(smoke);
   IGPU_TRACE("enumerate: node fd -> %d", fd);
   if (fd < 0 && !smoke)
      return VK_SUCCESS; /* no compatible device -> empty list */

   struct infinigpu_physical_device *pdev =
      vk_zalloc2(&instance->vk.alloc, NULL, sizeof(*pdev), 8,
                 VK_SYSTEM_ALLOCATION_SCOPE_INSTANCE);
   if (!pdev) {
      close(fd);
      return vk_error(instance, VK_ERROR_OUT_OF_HOST_MEMORY);
   }

   VkResult result = infinigpu_physical_device_init(pdev, instance, fd);
   if (result != VK_SUCCESS) {
      vk_free(&instance->vk.alloc, pdev);
      if (fd >= 0)
         close(fd);
      return result;
   }

   list_addtail(&pdev->vk.link, &instance->vk.physical_devices.list);
   IGPU_TRACE("enumerate: device added, returning VK_SUCCESS");
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_GetPhysicalDeviceQueueFamilyProperties2(
   VkPhysicalDevice physicalDevice,
   uint32_t *pCount,
   VkQueueFamilyProperties2 *pQueueFamilyProperties)
{
   IGPU_TRACE("GetQueueFamilyProperties2: count=%u props=%p", *pCount,
              (void *)pQueueFamilyProperties);
   VK_OUTARRAY_MAKE_TYPED(VkQueueFamilyProperties2, out,
                          pQueueFamilyProperties, pCount);

   vk_outarray_append_typed(VkQueueFamilyProperties2, &out, p) {
      p->queueFamilyProperties = (VkQueueFamilyProperties) {
         .queueFlags = VK_QUEUE_GRAPHICS_BIT |
                       VK_QUEUE_COMPUTE_BIT |
                       VK_QUEUE_TRANSFER_BIT,
         .queueCount = 1,
         .timestampValidBits = 64,
         .minImageTransferGranularity = (VkExtent3D){ 1, 1, 1 },
      };
   }
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_GetPhysicalDeviceMemoryProperties2(
   VkPhysicalDevice physicalDevice,
   VkPhysicalDeviceMemoryProperties2 *pMemoryProperties)
{
   IGPU_TRACE("GetMemoryProperties2");
   VkPhysicalDeviceMemoryProperties *m = &pMemoryProperties->memoryProperties;

   m->memoryHeapCount = 1;
   m->memoryHeaps[0] = (VkMemoryHeap) {
      .size = 16ULL * 1024 * 1024 * 1024, /* fabricated 16 GiB */
      .flags = VK_MEMORY_HEAP_DEVICE_LOCAL_BIT,
   };

   m->memoryTypeCount = 1;
   m->memoryTypes[0] = (VkMemoryType) {
      .propertyFlags = VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT |
                       VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                       VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
      .heapIndex = 0,
   };
}

/* The Vulkan-1.0 vk_common_GetPhysicalDevice{Format,ImageFormat,SparseImageFormat}Properties
 * wrappers (vk_physical_device.c) dispatch straight to these *2 entrypoints, and — unlike
 * Features2/Properties2 — there is NO generated vk_common *2 fallback, so a NULL here is a
 * hard crash under vulkaninfo. Phase 1 advertises the R8G8B8A8/B8G8R8A8 color formats the
 * forwarded-triangle path renders into (LINEAR, dumb-buffer-backed). */

/* The formats a forwarded draw can target: 32-bit RGBA/BGRA, LINEAR tiling. */
static bool
infinigpu_format_supported(VkFormat format)
{
   switch (format) {
   case VK_FORMAT_R8G8B8A8_UNORM:
   case VK_FORMAT_R8G8B8A8_SRGB:
   case VK_FORMAT_B8G8R8A8_UNORM:
   case VK_FORMAT_B8G8R8A8_SRGB:
      return true;
   default:
      return false;
   }
}

/* Formats usable as a vertex-buffer attribute (the wire's `vk_vformat` set). An app / validation
 * layer checks bufferFeatures & VERTEX_BUFFER_BIT before building a vertex-input state, so these
 * must be advertised or a real mesh pipeline is rejected. */
static bool
infinigpu_vertex_format_supported(VkFormat format)
{
   switch (format) {
   case VK_FORMAT_R32_SFLOAT:
   case VK_FORMAT_R32G32_SFLOAT:
   case VK_FORMAT_R32G32B32_SFLOAT:
   case VK_FORMAT_R32G32B32A32_SFLOAT:
   case VK_FORMAT_R8G8B8A8_UNORM:
   case VK_FORMAT_R32_UINT:
      return true;
   default:
      return false;
   }
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_GetPhysicalDeviceFormatProperties2(
   VkPhysicalDevice physicalDevice,
   VkFormat format,
   VkFormatProperties2 *pFormatProperties)
{
   VkFormatProperties *p = &pFormatProperties->formatProperties;
   *p = (VkFormatProperties){ 0 };

   if (infinigpu_format_supported(format)) {
      /* A linear image is both the render target (host DMA-writes it) and the
       * copy/readback/sampled source. */
      const VkFormatFeatureFlags feats =
         VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT |
         VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BLEND_BIT |
         VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT |
         VK_FORMAT_FEATURE_BLIT_SRC_BIT |
         VK_FORMAT_FEATURE_TRANSFER_SRC_BIT |
         VK_FORMAT_FEATURE_TRANSFER_DST_BIT;
      p->linearTilingFeatures = feats;
      p->optimalTilingFeatures = feats;
   }
   if (infinigpu_vertex_format_supported(format))
      p->bufferFeatures |= VK_FORMAT_FEATURE_VERTEX_BUFFER_BIT;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_GetPhysicalDeviceImageFormatProperties2(
   VkPhysicalDevice physicalDevice,
   const VkPhysicalDeviceImageFormatInfo2 *pImageFormatInfo,
   VkImageFormatProperties2 *pImageFormatProperties)
{
   if (!infinigpu_format_supported(pImageFormatInfo->format) ||
       pImageFormatInfo->type != VK_IMAGE_TYPE_2D)
      return VK_ERROR_FORMAT_NOT_SUPPORTED;

   pImageFormatProperties->imageFormatProperties = (VkImageFormatProperties){
      .maxExtent = { 16384, 16384, 1 },
      .maxMipLevels = 1,
      .maxArrayLayers = 2048,
      .sampleCounts = VK_SAMPLE_COUNT_1_BIT,
      .maxResourceSize = 1ULL << 31,
   };
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_GetPhysicalDeviceSparseImageFormatProperties2(
   VkPhysicalDevice physicalDevice,
   const VkPhysicalDeviceSparseImageFormatInfo2 *pFormatInfo,
   uint32_t *pPropertyCount,
   VkSparseImageFormatProperties2 *pProperties)
{
   *pPropertyCount = 0;
}
