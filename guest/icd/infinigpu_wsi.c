/*
 * Copyright 2026 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * WSI (VK_KHR_surface / VK_KHR_swapchain) via Mesa's common WSI layer.
 *
 * The whole point: a real DXVK/VKD3D game creates a swapchain and presents.
 * Without WSI the ICD advertises no surface/swapchain extensions, so such an
 * app dies at vkCreateInstance/vkCreateDevice with VK_ERROR_EXTENSION_NOT_PRESENT
 * before it draws anything. This wires the driver into wsi_common so instance +
 * device + swapchain all create, and present flows through the common layer.
 *
 * SOFTWARE present path. We pass wsi_device_options.sw_device = true and set
 * wsi_device.wants_linear = true, exactly like lavapipe (lvp_wsi.c). That selects
 * wsi_common's CPU-copy/no-blit branch: the common layer allocates each swapchain
 * image as a LINEAR, host-visible, mapped image using OUR OWN standard entrypoints
 * (CreateImage LINEAR + GetImageSubresourceLayout + AllocateMemory host-visible +
 * MapMemory) and memcpy's from that CPU map at present. The infinigpu image/memory
 * model already satisfies every one of those obligations (linear packed rows,
 * a single DEVICE_LOCAL|HOST_VISIBLE|HOST_COHERENT type, dumb-buffer maps), so we
 * write ZERO present/copy code here.
 *
 * display_fd = the infinigpu PRIMARY DRM node. On a real guest that node carries
 * KMS, so VK_KHR_display can page-flip a swapchain image straight onto the
 * infinigpu scanout (the frame the host encodes + streams) — the native VDI
 * present path (gamescope / direct-KMS style; needs DRM master). On the dev host
 * the fd is a render node (or -1), which has no KMS, so display enumeration is
 * simply empty and harmless. The surface/swapchain entrypoints themselves come
 * from the wsi_*_entrypoints tables merged in instance/physical_device/device.
 */

#include "infinigpu_private.h"

/* infinigpu_physical_device_to_handle(). */
static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
infinigpu_wsi_proc_addr(VkPhysicalDevice physicalDevice, const char *pName)
{
   VK_FROM_HANDLE(infinigpu_physical_device, pdev, physicalDevice);
   /* wsi_common fetches the driver entrypoints it needs (CreateImage,
    * AllocateMemory, MapMemory, …) through this unchecked lookup. */
   return vk_instance_get_proc_addr_unchecked(pdev->vk.instance, pName);
}

VkResult
infinigpu_init_wsi(struct infinigpu_physical_device *pdev)
{
   /* NOT sw_device: VK_KHR_display's swapchain HARD-CODES WSI_IMAGE_TYPE_DRM
    * (wsi_common_display.c: wsi_display_surface_create_swapchain), so its images
    * MUST be dma-buf-exported (drmPrimeFDToHandle + drmModeAddFB2 onto the infinigpu
    * scanout). Under .sw_device=true, wsi_common never even fetches GetMemoryFdKHR
    * (`if (!wsi->sw)` in wsi_common.c) → the DRM image path derefs a NULL callback →
    * vkCreateSwapchainKHR SIGSEGVs (this was THE blocker for a real fullscreen app
    * presenting on the GPU). As a real (non-sw) device wsi_common fetches our
    * GetMemoryFdKHR (PRIME-export of the drm_gem_dma BO) and takes the legacy
    * no-modifier "scanout" path (num_modifier_lists==0 → no VK_EXT_image_drm_format_
    * modifier needed). The proven zink RENDER path is offscreen (FBO, no swapchain),
    * so it is unaffected by this WSI-present change. */
   VkResult result =
      wsi_device_init(&pdev->wsi_device,
                      infinigpu_physical_device_to_handle(pdev),
                      infinigpu_wsi_proc_addr,
                      &pdev->vk.instance->alloc,
                      pdev->drm_fd, /* display_fd: card* PRIMARY node (KMS) in-guest */
                      NULL,
                      &(struct wsi_device_options){ .sw_device = false });
   if (result != VK_SUCCESS)
      return result;

   /* Our images are always LINEAR packed-pitch dumb buffers (drm_gem_dma); the WSI
    * "scanout" no-blit path scans them out directly (GetImageSubresourceLayout
    * reports the linear pitch drmModeAddFB2 uses). */
   pdev->wsi_device.wants_linear = true;
   pdev->vk.wsi_device = &pdev->wsi_device;

   return VK_SUCCESS;
}

void
infinigpu_finish_wsi(struct infinigpu_physical_device *pdev)
{
   /* NULL the pointer first so the common layer won't touch a torn-down device;
    * must run BEFORE the owner closes drm_fd (wsi references but never owns it). */
   pdev->vk.wsi_device = NULL;
   wsi_device_finish(&pdev->wsi_device, &pdev->vk.instance->alloc);
}
