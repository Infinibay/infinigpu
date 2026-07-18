/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * VkPhysicalDevice: enumerate one device backed by /dev/dri/renderD128 iff its
 * drm name is "infinigpu", fabricate properties, and answer the *2 property
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

   /* vk_physical_device_init: src/vulkan/runtime/vk_physical_device.h
    *   (pdev, instance, supported_extensions, supported_features,
    *    properties, dispatch_table) -- the 25.0.7 arity includes `properties`
    *    as the 5th arg (added vs older trees). NULL features => all false. */
   IGPU_TRACE("pdev_init: calling vk_physical_device_init");
   VkResult result = vk_physical_device_init(&pdev->vk, &instance->vk,
                                             &infinigpu_device_extensions,
                                             NULL,
                                             &properties,
                                             &dispatch_table);
   IGPU_TRACE("pdev_init: vk_physical_device_init -> %d", result);
   if (result != VK_SUCCESS)
      return result;

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

VkResult
infinigpu_enumerate_physical_devices(struct vk_instance *vk_instance)
{
   struct infinigpu_instance *instance =
      container_of(vk_instance, struct infinigpu_instance, vk);

   /* INFINIGPU_SMOKE_ANY_NODE: dev/test escape hatch. The real infinigpu DRM
    * device only exists inside a guest VM; on a bare host renderD128 is the
    * physical GPU (e.g. nvidia, often not even group-openable). In smoke mode we
    * (a) skip the drm name check and, (b) if the node can't be opened at all,
    * fabricate a device with NO backing fd (drm_fd = -1) — Phase 0 renders
    * nothing, so the whole load -> enumerate -> CreateDevice -> property-query
    * path is exercised without a real device. Never set it in production. */
   const bool smoke = getenv("INFINIGPU_SMOKE_ANY_NODE") != NULL;
   IGPU_TRACE("enumerate: called (smoke=%d)", smoke);

   int fd = open(INFINIGPU_RENDER_NODE, O_RDWR | O_CLOEXEC);
   IGPU_TRACE("enumerate: open(%s) -> %d", INFINIGPU_RENDER_NODE, fd);
   if (fd < 0 && !smoke)
      return VK_SUCCESS; /* no compatible device -> empty list */

   if (fd >= 0) {
      /* drmGetVersion / drmFreeVersion: xf86drm.h (libdrm). ->name is NUL-term. */
      drmVersionPtr version = drmGetVersion(fd);
      const bool is_infinigpu = version && version->name &&
                                strcmp(version->name, INFINIGPU_DRM_NAME) == 0;
      if (version)
         drmFreeVersion(version);
      if (!is_infinigpu && !smoke) {
         close(fd);
         return VK_SUCCESS;
      }
   }

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
 * hard crash under vulkaninfo. Phase 0 advertises no format support yet (real format
 * capabilities arrive with the render path in Phase 1). */

VKAPI_ATTR void VKAPI_CALL
infinigpu_GetPhysicalDeviceFormatProperties2(
   VkPhysicalDevice physicalDevice,
   VkFormat format,
   VkFormatProperties2 *pFormatProperties)
{
   pFormatProperties->formatProperties = (VkFormatProperties){ 0 };
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_GetPhysicalDeviceImageFormatProperties2(
   VkPhysicalDevice physicalDevice,
   const VkPhysicalDeviceImageFormatInfo2 *pImageFormatInfo,
   VkImageFormatProperties2 *pImageFormatProperties)
{
   return VK_ERROR_FORMAT_NOT_SUPPORTED;
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
