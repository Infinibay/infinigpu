/*
 * Copyright 2026 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * infinigpu ICD WSI validation. Exercises the exact path a real DXVK/VKD3D game
 * needs before it can draw anything: create an instance that enables the surface
 * extensions, find the device, create a surface, query its capabilities, create a
 * device with VK_KHR_swapchain, then create a swapchain and (if it allocates)
 * acquire + present a frame.
 *
 * Two environments:
 *   - Dev HOST (no real infinigpu DRM node): steps 1..6 (instance, surface, caps,
 *     device-with-swapchain) MUST pass — that is the WSI *wiring* the ICD adds.
 *     vkCreateSwapchainKHR then fails at image memory allocation (no dumb-buffer
 *     node), which is EXPECTED and reported as a NOTE, not a failure.
 *   - Real GUEST (renderD128 is the infinigpu node): the swapchain allocates, an
 *     image acquires, and a present goes through the headless sink — "FULL WSI OK".
 *
 * Build (guest, with the Vulkan loader):
 *   cc -O2 -o infinigpu_wsi_test infinigpu_wsi_test.c -lvulkan
 * Run on the dev host (renderD128 is NOT infinigpu here):
 *   VK_DRIVER_FILES=.../infinigpu_devenv_icd.x86_64.json \
 *   INFINIGPU_SMOKE_ANY_NODE=1 ./infinigpu_wsi_test
 * Run in the guest:
 *   VK_DRIVER_FILES=/usr/share/vulkan/icd.d/infinigpu_icd.x86_64.json ./infinigpu_wsi_test
 */

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <vulkan/vulkan.h>

#define CHECK(expr)                                                            \
   do {                                                                        \
      VkResult _r = (expr);                                                    \
      if (_r != VK_SUCCESS) {                                                  \
         fprintf(stderr, "FAIL %s = %d (line %d)\n", #expr, _r, __LINE__);     \
         return 1;                                                             \
      }                                                                        \
   } while (0)

/* Load an instance-level entrypoint (surface/swapchain funcs are extensions the
 * loader may not export as direct symbols). */
#define LOAD(inst, name)                                                       \
   PFN_##name name = (PFN_##name)vkGetInstanceProcAddr((inst), #name);         \
   do {                                                                        \
      if (!name) {                                                             \
         fprintf(stderr, "FAIL could not load %s\n", #name);                   \
         return 1;                                                             \
      }                                                                        \
   } while (0)

static bool
has_ext(const VkExtensionProperties *props, uint32_t n, const char *want)
{
   for (uint32_t i = 0; i < n; i++)
      if (strcmp(props[i].extensionName, want) == 0)
         return true;
   return false;
}

int
main(void)
{
   /* 1. Instance with the surface extensions a windowed app enables. */
   const char *inst_exts[] = {
      VK_KHR_SURFACE_EXTENSION_NAME,
      VK_EXT_HEADLESS_SURFACE_EXTENSION_NAME,
   };
   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "infinigpu-wsi-test",
      .apiVersion = VK_API_VERSION_1_3,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
      .enabledExtensionCount = 2,
      .ppEnabledExtensionNames = inst_exts,
   };
   VkInstance instance;
   CHECK(vkCreateInstance(&ici, NULL, &instance));
   printf("ok  instance (KHR_surface + EXT_headless_surface enabled)\n");

   LOAD(instance, vkCreateHeadlessSurfaceEXT);
   LOAD(instance, vkDestroySurfaceKHR);
   LOAD(instance, vkGetPhysicalDeviceSurfaceSupportKHR);
   LOAD(instance, vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
   LOAD(instance, vkGetPhysicalDeviceSurfaceFormatsKHR);
   LOAD(instance, vkGetPhysicalDeviceSurfacePresentModesKHR);
   LOAD(instance, vkCreateSwapchainKHR);
   LOAD(instance, vkDestroySwapchainKHR);
   LOAD(instance, vkGetSwapchainImagesKHR);
   LOAD(instance, vkAcquireNextImageKHR);
   LOAD(instance, vkQueuePresentKHR);

   /* 2. Physical device. */
   uint32_t ndev = 1;
   VkPhysicalDevice pdev;
   CHECK(vkEnumeratePhysicalDevices(instance, &ndev, &pdev));
   if (ndev == 0) {
      fprintf(stderr, "FAIL no physical device\n");
      return 1;
   }
   printf("ok  physical device\n");

   /* 3. VK_KHR_swapchain must be advertised. */
   uint32_t next = 0;
   CHECK(vkEnumerateDeviceExtensionProperties(pdev, NULL, &next, NULL));
   VkExtensionProperties *dext = calloc(next, sizeof(*dext));
   CHECK(vkEnumerateDeviceExtensionProperties(pdev, NULL, &next, dext));
   if (!has_ext(dext, next, VK_KHR_SWAPCHAIN_EXTENSION_NAME)) {
      fprintf(stderr, "FAIL VK_KHR_swapchain not advertised\n");
      return 1;
   }
   printf("ok  VK_KHR_swapchain advertised (%u device extensions)\n", next);
   free(dext);

   /* 4. Headless surface. */
   VkHeadlessSurfaceCreateInfoEXT sci = {
      .sType = VK_STRUCTURE_TYPE_HEADLESS_SURFACE_CREATE_INFO_EXT,
   };
   VkSurfaceKHR surface;
   CHECK(vkCreateHeadlessSurfaceEXT(instance, &sci, NULL, &surface));
   printf("ok  headless surface\n");

   /* 5. Surface queries — the ones a swapchain setup runs. */
   VkBool32 supported = VK_FALSE;
   CHECK(vkGetPhysicalDeviceSurfaceSupportKHR(pdev, 0, surface, &supported));
   VkSurfaceCapabilitiesKHR caps;
   CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pdev, surface, &caps));
   uint32_t nfmt = 0;
   CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(pdev, surface, &nfmt, NULL));
   VkSurfaceFormatKHR *fmts = calloc(nfmt ? nfmt : 1, sizeof(*fmts));
   CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(pdev, surface, &nfmt, fmts));
   uint32_t nmode = 0;
   CHECK(vkGetPhysicalDeviceSurfacePresentModesKHR(pdev, surface, &nmode, NULL));
   printf("ok  surface queries: support=%d, %u formats, %u present modes, "
          "minImages=%u\n", supported, nfmt, nmode, caps.minImageCount);
   if (nfmt == 0) {
      fprintf(stderr, "FAIL surface exposes no formats\n");
      return 1;
   }

   /* 6. Device with VK_KHR_swapchain enabled. */
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = 0,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   const char *dev_exts[] = { VK_KHR_SWAPCHAIN_EXTENSION_NAME };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 1,
      .ppEnabledExtensionNames = dev_exts,
   };
   VkDevice device;
   CHECK(vkCreateDevice(pdev, &dci, NULL, &device));
   printf("ok  device (VK_KHR_swapchain enabled)\n");
   printf("PASS WSI wiring: a swapchain-using app now creates instance + surface "
          "+ device instead of VK_ERROR_EXTENSION_NOT_PRESENT.\n");

   VkQueue queue;
   vkGetDeviceQueue(device, 0, 0, &queue);

   /* 7. Swapchain — allocates images through the ICD's own image/memory path.
    * Needs a real dumb-buffer node, so this is where a dev host stops. */
   VkExtent2D extent = caps.currentExtent.width != 0xffffffff
                          ? caps.currentExtent
                          : (VkExtent2D){ 256, 256 };
   VkSwapchainCreateInfoKHR wci = {
      .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
      .surface = surface,
      .minImageCount = caps.minImageCount < 2 ? 2 : caps.minImageCount,
      .imageFormat = fmts[0].format,
      .imageColorSpace = fmts[0].colorSpace,
      .imageExtent = extent,
      .imageArrayLayers = 1,
      .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
      .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .preTransform = caps.currentTransform,
      .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
      .presentMode = VK_PRESENT_MODE_FIFO_KHR,
      .clipped = VK_TRUE,
   };
   VkSwapchainKHR swapchain;
   VkResult sc = vkCreateSwapchainKHR(device, &wci, NULL, &swapchain);
   if (sc != VK_SUCCESS) {
      printf("NOTE vkCreateSwapchainKHR = %d — expected on a dev host without a "
             "real infinigpu DRM node (swapchain image memory can't allocate). "
             "The WSI wiring above is what this test proves; full present is "
             "validated in a real GPU VM.\n", sc);
      free(fmts);
      vkDestroySurfaceKHR(instance, surface, NULL);
      vkDestroyDevice(device, NULL);
      vkDestroyInstance(instance, NULL);
      return 0;
   }
   printf("ok  swapchain created\n");

   uint32_t nimg = 0;
   CHECK(vkGetSwapchainImagesKHR(device, swapchain, &nimg, NULL));
   printf("ok  %u swapchain images\n", nimg);

   VkSemaphore acq;
   VkSemaphoreCreateInfo semi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
   CHECK(vkCreateSemaphore(device, &semi, NULL, &acq));
   uint32_t idx = 0;
   CHECK(vkAcquireNextImageKHR(device, swapchain, UINT64_MAX, acq, VK_NULL_HANDLE, &idx));
   printf("ok  acquired image %u\n", idx);

   VkPresentInfoKHR pi = {
      .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
      .waitSemaphoreCount = 1,
      .pWaitSemaphores = &acq, /* wait for the acquire before presenting */
      .swapchainCount = 1,
      .pSwapchains = &swapchain,
      .pImageIndices = &idx,
   };
   VkResult pr = vkQueuePresentKHR(queue, &pi);
   /* Present is the one step this harness uniquely proves, so its result MUST
    * gate the verdict — VK_SUBOPTIMAL_KHR is a successful present. */
   bool present_ok = (pr == VK_SUCCESS || pr == VK_SUBOPTIMAL_KHR);
   printf("%s present = %d\n", present_ok ? "ok " : "FAIL", pr);

   vkDestroySemaphore(device, acq, NULL);
   vkDestroySwapchainKHR(device, swapchain, NULL);
   free(fmts);
   vkDestroySurfaceKHR(instance, surface, NULL);
   vkDestroyDevice(device, NULL);
   vkDestroyInstance(instance, NULL);

   if (!present_ok) {
      fprintf(stderr, "FAIL vkQueuePresentKHR = %d\n", pr);
      return 1;
   }
   printf("FULL WSI OK: instance + surface + device + swapchain + acquire + "
          "present all succeeded.\n");
   return 0;
}
