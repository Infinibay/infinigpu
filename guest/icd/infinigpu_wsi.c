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
   VkResult result =
      wsi_device_init(&pdev->wsi_device,
                      infinigpu_physical_device_to_handle(pdev),
                      infinigpu_wsi_proc_addr,
                      &pdev->vk.instance->alloc,
                      pdev->drm_fd, /* display_fd: KMS present in-guest, empty on host */
                      NULL,
                      &(struct wsi_device_options){ .sw_device = true });
   if (result != VK_SUCCESS)
      return result;

   /* Force the linear no-blit CPU image path (no dma-buf, no GetMemoryFdKHR). */
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
