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
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#include <xf86drm.h>

#include "vk_alloc.h"
#include "vk_log.h"
#include "vk_util.h"

#define INFINIGPU_RENDER_NODE "/dev/dri/renderD128"
#define INFINIGPU_DRM_NAME    "infinigpu"

/* VK_KHR_swapchain: the device half of WSI. Advertised unconditionally because
 * the headless surface backend is always compiled in (non-Windows) and we always
 * initialize wsi_device. Its entrypoints (vkCreateSwapchainKHR / AcquireNextImage
 * / QueuePresentKHR / GetSwapchainImagesKHR) come from wsi_device_entrypoints,
 * merged into the device dispatch table in infinigpu_CreateDevice. */
const struct vk_device_extension_table infinigpu_device_extensions = {
   .KHR_swapchain = true,
   /* Zink selects a physical device by matching the DRM node major/minor it opened
    * against VkPhysicalDeviceDrmPropertiesEXT; without this extension (and the DRM
    * properties filled in infinigpu_get_properties) zink_get_display_device() matches
    * nothing and bails with "ZINK: failed to choose pdev". Pure pdev metadata reported
    * guest-side — the A5000/replay is not involved in device selection. */
   .EXT_physical_device_drm = true,

   /* --- OpenGL/Zink M1: the extension STRINGS zink hard-requires to stand up a real
    * (hardware) GL screen. zink_get_physical_device_info() marks these `required=True`
    * (zink_device_info.py) and does `goto fail` — which makes zink_internal_create_screen()
    * return NULL ("failed to create dri2 screen") — if any is absent from
    * vkEnumerateDeviceExtensionProperties. There is NO core-version fallback: zink matches
    * the STRING, not the promoted core version, so advertising the core-1.3 FEATURE is not
    * enough. All resolve to Mesa's generated vk_common_* device entrypoints (CreateRenderPass2,
    * CmdBeginRenderPass2, CreateFramebuffer[imageless], CreateDescriptorUpdateTemplate,
    * UpdateDescriptorSetWithTemplate, CmdBeginRendering) — vk_device_init() auto-merges
    * vk_common_device_entrypoints (overwrite=false), and those common impls delegate to the
    * driver hooks we already implement. No wire/replay work.
    *
    * VERSION NOTE: the required set GREW after the Mesa 25.0.7 this ICD builds against.
    * The guest's runtime zink is Mesa 26.0.3, which additionally marks VK_KHR_dynamic_rendering
    * required=True (zink_device_info.py:206). Its absence made zink_get_physical_device_info()
    * hit `debug_printf("ZINK: VK_KHR_dynamic_rendering required!")` — compiled OUT of a release
    * Mesa — then `goto fail`, so the DRI loader fell back to kms_swrast/llvmpipe with NO visible
    * error (the silent "creates screen? no → software" trap). Confirmed by extracting the exact
    * 26.0.3 source (apt-get source mesa) in the live guest. We already advertise the
    * dynamicRendering feature (infinigpu_get_features) and implement CmdBeginRendering. */
   .KHR_maintenance1              = true,
   .KHR_create_renderpass2        = true,
   .KHR_imageless_framebuffer     = true,
   .KHR_descriptor_update_template = true,
   .KHR_dynamic_rendering         = true,

   /* zink hard-requires a timeline semaphore (else "zink: KHR_timeline_semaphore is
    * required"). We satisfy it BOTH ways: the core-1.2 feature (VkPhysicalDeviceVulkan12
    * Features.timelineSemaphore, see infinigpu_get_features) AND this extension string.
    * The timeline is EMULATED guest-side on the binary sync (vk_sync_timeline over
    * infinigpu_sync_type); IMMEDIATE synchronous submit means every point is already
    * signalled when a submit returns, so the wire/replay never sees a timeline. */
   .KHR_timeline_semaphore        = true,

   /* zink_drm_create_screen() checks have_KHR_external_memory_fd AFTER a successful
    * internal create and, if false, destroys the screen and returns NULL — the DRM-path
    * gate the DRI2/DRI3/kopper loader always hits. The screen-creation gate only checks
    * the STRING is present (satisfied here). Real dmabuf export (vkGetMemoryFdKHR) is a
    * present-time concern (WSI/M2), not a screen-creation blocker. */
   .KHR_external_memory           = true,
   .KHR_external_memory_fd        = true,

   /* Mesa 26.0.3 zink adds a SECOND hard gate right after feature detection
    * (zink_screen.c:3459): `if (!screen->info.rb2_feats.nullDescriptor) { mesa_loge("Zink
    * requires the nullDescriptor feature of KHR/EXT robustness2."); goto fail; }`. So the
    * device must advertise VK_EXT_robustness2 AND report its nullDescriptor feature true
    * (set in infinigpu_get_features). robustness2 has no entrypoints — it is pure feature/
    * property metadata; Mesa's vk_common GetPhysicalDeviceFeatures2 surfaces nullDescriptor
    * from vk_features. nullDescriptor semantics (a VK_NULL_HANDLE descriptor reads 0 / writes
    * are discarded) are what zink relies on to bind unused slots; the forwarded-draw path
    * builds its descriptor set from the bound resources, so an unbound (null) slot is simply
    * absent — consistent with the promise. This was not a zink requirement in 25.0.7. */
   .EXT_robustness2               = true,
};

/* Fill VkPhysicalDeviceDrmPropertiesEXT (mapped by Mesa's generated
 * GetPhysicalDeviceProperties2 into these flat vk_properties fields). Derived
 * DYNAMICALLY from the opened node — never hardcode renderD128; the render minor
 * must equal the node the guest GL loader actually stat()s, or zink still rejects
 * the pdev. The ICD opens the PRIMARY node (card*); its render sibling is found via
 * libdrm. Both are reported so zink can match on either. */
static void
infinigpu_fill_drm_properties(struct vk_properties *p, int drm_fd)
{
   struct stat st;

   if (drm_fd < 0)
      return;

   /* The opened fd is the primary (card*) node. */
   if (fstat(drm_fd, &st) == 0 && S_ISCHR(st.st_mode)) {
      p->drmHasPrimary = true;
      p->drmPrimaryMajor = major(st.st_rdev);
      p->drmPrimaryMinor = minor(st.st_rdev);
   }

   drmDevicePtr dev = NULL;
   if (drmGetDevice2(drm_fd, 0, &dev) == 0 && dev) {
      if ((dev->available_nodes & (1 << DRM_NODE_RENDER)) &&
          dev->nodes[DRM_NODE_RENDER]) {
         struct stat rst;
         if (stat(dev->nodes[DRM_NODE_RENDER], &rst) == 0 && S_ISCHR(rst.st_mode)) {
            p->drmHasRender = true;
            p->drmRenderMajor = major(rst.st_rdev);
            p->drmRenderMinor = minor(rst.st_rdev);
         }
      }
      if (!p->drmHasPrimary &&
          (dev->available_nodes & (1 << DRM_NODE_PRIMARY)) &&
          dev->nodes[DRM_NODE_PRIMARY]) {
         struct stat pst;
         if (stat(dev->nodes[DRM_NODE_PRIMARY], &pst) == 0 && S_ISCHR(pst.st_mode)) {
            p->drmHasPrimary = true;
            p->drmPrimaryMajor = major(pst.st_rdev);
            p->drmPrimaryMinor = minor(pst.st_rdev);
         }
      }
      drmFreeDevice(&dev);
   }
}

static void
infinigpu_get_properties(const struct infinigpu_physical_device *pdev,
                         struct vk_properties *p, int drm_fd)
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

      /* Descriptor / attachment / vertex-input / framebuffer / sample limits that
       * zink reads to SIZE its resource usage. Leaving them zero (the old default)
       * makes zink clamp-to-zero, divide-by-zero, or hard-abort at context/pipeline
       * build — AFTER a non-NULL screen (the "screen succeeds, first context crashes"
       * trap). Values are generous, spec-compliant ceilings: the A5000 backs them and
       * SPIR-V is forwarded verbatim. Where the WIRE is structurally narrower we report
       * the honest cap (maxVertexInputAttributes == INFINIGPU_MAX_ATTRS; single-sample
       * framebuffers because the replay hardcodes VK_SAMPLE_COUNT_1_BIT). Deeper wire
       * limits (one descriptor set, one UBO/SSBO, <=8 textures, one color target) are
       * an M2 correctness matter, not a device-selection or sizing limit. */
      .maxTexelBufferElements               = 1u << 16,
      .maxUniformBufferRange                = 1u << 16,   /* zink asserts >= 16384 */
      .maxStorageBufferRange                = 1u << 30,   /* zink asserts >= 1<<27 */
      .maxPerStageDescriptorSamplers        = 1024,
      .maxPerStageDescriptorUniformBuffers  = 1024,
      .maxPerStageDescriptorStorageBuffers  = 1024,
      .maxPerStageDescriptorSampledImages   = 1024,
      .maxPerStageDescriptorStorageImages   = 1024,
      .maxPerStageDescriptorInputAttachments = 1024,
      .maxPerStageResources                 = 1024,
      .maxDescriptorSetSamplers             = 1024,
      .maxDescriptorSetUniformBuffers       = 1024,
      .maxDescriptorSetUniformBuffersDynamic = 8,
      .maxDescriptorSetStorageBuffers       = 1024,
      .maxDescriptorSetStorageBuffersDynamic = 8,
      .maxDescriptorSetSampledImages        = 1024,
      .maxDescriptorSetStorageImages        = 1024,
      .maxDescriptorSetInputAttachments     = 1024,
      .maxVertexInputAttributes             = INFINIGPU_MAX_ATTRS,   /* 16 — wire cap */
      .maxVertexInputBindings               = INFINIGPU_MAX_ATTRS,
      .maxVertexInputAttributeOffset        = 2047,
      .maxVertexInputBindingStride          = 2048,
      .maxVertexOutputComponents            = 128,
      .maxFragmentInputComponents           = 128,
      .maxFragmentOutputAttachments         = 8,
      .maxFragmentDualSrcAttachments        = 1,
      .maxFragmentCombinedOutputResources   = 16,
      .maxColorAttachments                  = 8,
      .maxDrawIndexedIndexValue             = UINT32_MAX,
      .maxDrawIndirectCount                 = 1,
      .maxSamplerLodBias                    = 16.0f,
      .maxSamplerAnisotropy                 = 16.0f,
      .maxFramebufferWidth                  = 16384,
      .maxFramebufferHeight                 = 16384,
      .maxFramebufferLayers                 = 2048,
      .framebufferColorSampleCounts         = VK_SAMPLE_COUNT_1_BIT,
      .framebufferDepthSampleCounts         = VK_SAMPLE_COUNT_1_BIT,
      .framebufferStencilSampleCounts       = VK_SAMPLE_COUNT_1_BIT,
      .framebufferNoAttachmentsSampleCounts = VK_SAMPLE_COUNT_1_BIT,
      .sampledImageColorSampleCounts        = VK_SAMPLE_COUNT_1_BIT,
      .sampledImageIntegerSampleCounts      = VK_SAMPLE_COUNT_1_BIT,
      .sampledImageDepthSampleCounts        = VK_SAMPLE_COUNT_1_BIT,
      .sampledImageStencilSampleCounts      = VK_SAMPLE_COUNT_1_BIT,
      .storageImageSampleCounts             = VK_SAMPLE_COUNT_1_BIT,
      .maxSampleMaskWords                   = 1,
      .maxClipDistances                     = 8,
      .maxCullDistances                     = 8,
      .maxCombinedClipAndCullDistances      = 8,
      .pointSizeRange                       = { 1.0f, 64.0f },
      .lineWidthRange                       = { 1.0f, 1.0f },
      .pointSizeGranularity                 = (1.0f / 8.0f),
      .lineWidthGranularity                 = 0.0f,
      .maxTexelOffset                       = 7,
      .minTexelOffset                       = -8,
      .subPixelPrecisionBits                = 8,
      .subTexelPrecisionBits                = 8,
      .mipmapPrecisionBits                  = 8,

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

   /* VkPhysicalDeviceDrmPropertiesEXT — required for Zink to select this pdev. */
   infinigpu_fill_drm_properties(p, drm_fd);
}

/* Feature set. The forwarded-draw path uses dynamicRendering (CmdBeginRendering) +
 * synchronization2 (CmdPipelineBarrier2). The rest are the OpenGL/Zink M1 features:
 *   - timelineSemaphore: the ONE feature whose absence hard-fails zink screen creation
 *     (zink_screen.c:3451). Emulated on the binary sync (see the sync registration in
 *     infinigpu_physical_device_init) — IMMEDIATE synchronous submit resolves every
 *     point on return, so the wire/replay never sees a timeline.
 *   - imagelessFramebuffer: the feature behind VK_KHR_imageless_framebuffer; Mesa's
 *     common CreateFramebuffer honours the IMAGELESS flag only when this is enabled.
 * Both are backed entirely guest-side (metadata + Mesa common runtime); no replay work. */
static void
infinigpu_get_features(struct vk_features *f)
{
   *f = (struct vk_features){
      .dynamicRendering    = true,
      .synchronization2    = true,
      .timelineSemaphore   = true,
      .imagelessFramebuffer = true,
      /* VK_EXT_robustness2 nullDescriptor: hard-required by Mesa 26.0.3 zink screen
       * creation (zink_screen.c:3459). Only nullDescriptor is demanded — leave
       * robustBufferAccess2/robustImageAccess2 false (honest: the wire does not
       * enforce the tightened robustness2 bounds). See the EXT_robustness2 note on
       * the extension table. */
      .nullDescriptor      = true,
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
   /* Merge the common WSI surface-query entrypoints (GetPhysicalDeviceSurface*,
    * GetPhysicalDeviceDisplay*, …). overwrite=false: our own entries always win. */
   vk_physical_device_dispatch_table_from_entrypoints(
      &dispatch_table, &wsi_physical_device_entrypoints, false);

   struct vk_properties properties;
   infinigpu_get_properties(pdev, &properties, drm_fd);
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

   /* Register the driver's binary CPU sync PLUS an emulated timeline built on top of
    * it (vk_sync_timeline over the binary type — the same construction lavapipe uses).
    * OpenGL/Zink hard-requires a working timeline VkSemaphore; emulating it guest-side
    * is free here because submit is IMMEDIATE and synchronous — driver_submit blocks on
    * the forwarded-draw ioctl, so every timeline point is already signalled when a
    * submit returns and the wire/replay never sees a timeline. The binary type exposes
    * exactly the features vk_sync_timeline needs (CPU_WAIT/SIGNAL/RESET + WAIT_PENDING,
    * see infinigpu_sync.c). */
   pdev->sync_timeline_type = vk_sync_timeline_get_type(&infinigpu_sync_type);
   pdev->sync_types[0] = &infinigpu_sync_type;
   pdev->sync_types[1] = &pdev->sync_timeline_type.sync;
   pdev->sync_types[2] = NULL;
   pdev->vk.supported_sync_types = pdev->sync_types;

   pdev->drm_fd = drm_fd;

   /* Bring up WSI (needs vk.instance + the handle + drm_fd — all set by now).
    * On failure, unwind the vk_physical_device so the caller frees cleanly. */
   result = infinigpu_init_wsi(pdev);
   if (result != VK_SUCCESS) {
      IGPU_TRACE("pdev_init: infinigpu_init_wsi -> %d", result);
      vk_physical_device_finish(&pdev->vk);
      return result;
   }

   return VK_SUCCESS;
}

void
infinigpu_physical_device_destroy(struct vk_physical_device *vk_pdev)
{
   struct infinigpu_physical_device *pdev =
      container_of(vk_pdev, struct infinigpu_physical_device, vk);

   /* Tear down WSI before closing drm_fd — wsi_device holds it as display_fd
    * (referenced, never owned) and must stop using it before we close it. */
   infinigpu_finish_wsi(pdev);
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

/* Depth/stencil formats. zink probes have_X8_D24_UNORM_PACK32 / have_D24_UNORM_S8_UINT /
 * have_D32_SFLOAT_S8_UINT at screen-create (zink_screen.c) to decide depth support, and the
 * DRI/GL frontend enumerates depth-buffered GLX/EGL framebuffer configs from these — with ZERO
 * depth support every 3D app that asks for a depth buffer is stuck (glGetString aside, no depth
 * test). REQUIRED for real 3D even though it is not, on its own, the zink screen-selection gate.
 * These land in the same dumb-buffer storage as color; the host replay already depth-tests
 * forwarded meshes, so a depth attachment is backed. LINEAR is honest (row-major, no tiling). */
static bool
infinigpu_depth_format_supported(VkFormat format)
{
   switch (format) {
   case VK_FORMAT_D16_UNORM:
   case VK_FORMAT_X8_D24_UNORM_PACK32:
   case VK_FORMAT_D32_SFLOAT:
   case VK_FORMAT_D24_UNORM_S8_UINT:
   case VK_FORMAT_D32_SFLOAT_S8_UINT:
   case VK_FORMAT_S8_UINT:
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
   if (infinigpu_depth_format_supported(format)) {
      /* A depth/stencil image is a render target (host depth-tests into it) and a
       * sampled/blit/transfer source (shadow maps, readback). No BLEND on depth. */
      const VkFormatFeatureFlags dfeats =
         VK_FORMAT_FEATURE_DEPTH_STENCIL_ATTACHMENT_BIT |
         VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT |
         VK_FORMAT_FEATURE_BLIT_SRC_BIT |
         VK_FORMAT_FEATURE_TRANSFER_SRC_BIT |
         VK_FORMAT_FEATURE_TRANSFER_DST_BIT;
      p->optimalTilingFeatures |= dfeats;
      p->linearTilingFeatures |= dfeats;
   }
   if (infinigpu_vertex_format_supported(format))
      p->bufferFeatures |= VK_FORMAT_FEATURE_VERTEX_BUFFER_BIT;

   /* VkFormatProperties3 (core 1.3 / VK_KHR_format_feature_flags2) carries the SAME
    * features as the 64-bit VkFormatFeatureFlags2. This is NOT optional metadata for us:
    * a consumer that sees apiVersion >= 1.3 reads its format features from THIS struct,
    * not the legacy 32-bit VkFormatProperties above. zink is exactly such a consumer —
    * zink_init_format_props() takes the `have_vulkan13` branch and caches
    * props3.optimalTilingFeatures. If we leave props3 zero, zink believes NO color format
    * can be a RENDER_TARGET, so dri_fill_in_modes builds ZERO GLX/EGL configs,
    * driCreateConfigs fails, the DRI screen is NULL, and GL silently falls back to
    * llvmpipe (the exact failure this fixes — found via a debug Mesa 26.0.3 build in the
    * guest). Because we override GetPhysicalDeviceFormatProperties2 (rather than deferring
    * to vk_common) WE own the pNext walk. The FeatureFlags2 bit values equal the legacy
    * FeatureFlags for every bit we set, so mirror the 32-bit masks widened to 64-bit. */
   vk_foreach_struct(ext, pFormatProperties->pNext) {
      if (ext->sType == VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_3) {
         VkFormatProperties3 *p3 = (VkFormatProperties3 *)ext;
         p3->linearTilingFeatures  = p->linearTilingFeatures;
         p3->optimalTilingFeatures = p->optimalTilingFeatures;
         p3->bufferFeatures        = p->bufferFeatures;
      }
   }
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_GetPhysicalDeviceImageFormatProperties2(
   VkPhysicalDevice physicalDevice,
   const VkPhysicalDeviceImageFormatInfo2 *pImageFormatInfo,
   VkImageFormatProperties2 *pImageFormatProperties)
{
   if ((!infinigpu_format_supported(pImageFormatInfo->format) &&
        !infinigpu_depth_format_supported(pImageFormatInfo->format)) ||
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
