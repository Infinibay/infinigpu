/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * VkInstance handling.  Cribbed from lavapipe (lvp_CreateInstance /
 * lvp_GetInstanceProcAddr / lvp_EnumerateInstanceExtensionProperties).
 */

#include "infinigpu_private.h"

#include "vk_alloc.h"
#include "vk_log.h"

/* Surface/WSI instance extensions. KHR_surface + KHR_get_surface_capabilities2
 * are the generic swapchain prerequisites (headless present is always compiled
 * in on non-Windows). KHR_display is added when the build has the KMS display
 * backend (VK_USE_PLATFORM_DISPLAY_KHR — the -Dplatforms= empty build defines it
 * whenever the host has KMS+libdrm). No xcb/xlib/wayland client libs are linked,
 * so those surface extensions are deliberately absent. The entrypoints backing
 * these come from wsi_instance_entrypoints, merged in infinigpu_CreateInstance. */
const struct vk_instance_extension_table infinigpu_instance_extensions = {
   .KHR_surface                    = true,
   .KHR_get_surface_capabilities2  = true,
   .EXT_headless_surface           = true,
#ifdef VK_USE_PLATFORM_DISPLAY_KHR
   .KHR_display                    = true,
#endif
};

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_EnumerateInstanceVersion(uint32_t *pApiVersion)
{
   *pApiVersion = INFINIGPU_API_VERSION;
   return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_EnumerateInstanceExtensionProperties(const char *pLayerName,
                                               uint32_t *pPropertyCount,
                                               VkExtensionProperties *pProperties)
{
   if (pLayerName)
      return vk_error(NULL, VK_ERROR_LAYER_NOT_PRESENT);

   /* vk_enumerate_instance_extension_properties: src/vulkan/runtime/vk_instance.h */
   return vk_enumerate_instance_extension_properties(
      &infinigpu_instance_extensions, pPropertyCount, pProperties);
}

VKAPI_ATTR VkResult VKAPI_CALL
infinigpu_CreateInstance(const VkInstanceCreateInfo *pCreateInfo,
                         const VkAllocationCallbacks *pAllocator,
                         VkInstance *pInstance)
{
   struct infinigpu_instance *instance;
   VkResult result;

   assert(pCreateInfo->sType == VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO);

   if (pAllocator == NULL)
      pAllocator = vk_default_allocator();

   instance = vk_zalloc(pAllocator, sizeof(*instance), 8,
                        VK_SYSTEM_ALLOCATION_SCOPE_INSTANCE);
   if (!instance)
      return vk_error(NULL, VK_ERROR_OUT_OF_HOST_MEMORY);

   struct vk_instance_dispatch_table dispatch_table;
   vk_instance_dispatch_table_from_entrypoints(
      &dispatch_table, &infinigpu_instance_entrypoints, true);
   /* Merge the common WSI surface entrypoints (vkCreateHeadlessSurfaceEXT,
    * vkDestroySurfaceKHR, …). overwrite=false: our own entries always win. */
   vk_instance_dispatch_table_from_entrypoints(
      &dispatch_table, &wsi_instance_entrypoints, false);

   /* vk_instance_init: src/vulkan/runtime/vk_instance.h
    *   (instance, supported_extensions, dispatch_table, pCreateInfo, alloc) */
   result = vk_instance_init(&instance->vk,
                             &infinigpu_instance_extensions,
                             &dispatch_table,
                             pCreateInfo,
                             pAllocator);
   if (result != VK_SUCCESS) {
      vk_free(pAllocator, instance);
      return vk_error(NULL, result);
   }

   /* Common physical-device management (vk_common_EnumeratePhysicalDevices
    * calls ->enumerate; DestroyInstance frees pdevs via ->destroy). */
   instance->vk.physical_devices.enumerate = infinigpu_enumerate_physical_devices;
   instance->vk.physical_devices.destroy = infinigpu_physical_device_destroy;

   *pInstance = infinigpu_instance_to_handle(instance);
   return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL
infinigpu_DestroyInstance(VkInstance _instance,
                          const VkAllocationCallbacks *pAllocator)
{
   VK_FROM_HANDLE(infinigpu_instance, instance, _instance);

   if (!instance)
      return;

   vk_instance_finish(&instance->vk);
   vk_free(&instance->vk.alloc, instance);
}

VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
infinigpu_GetInstanceProcAddr(VkInstance _instance, const char *pName)
{
   VK_FROM_HANDLE(vk_instance, instance, _instance);
   /* vk_instance_get_proc_addr: src/vulkan/runtime/vk_instance.h
    *   (instance, entrypoints, name) */
   return vk_instance_get_proc_addr(instance,
                                    &infinigpu_instance_entrypoints,
                                    pName);
}
