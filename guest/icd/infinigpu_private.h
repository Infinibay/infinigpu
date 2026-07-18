/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * Phase-0 skeleton Vulkan ICD for the "infinigpu" remote GPU.
 * Based in part on Mesa's lavapipe and venus drivers.
 */

#ifndef INFINIGPU_PRIVATE_H
#define INFINIGPU_PRIVATE_H

#include <stdbool.h>
#include <stdint.h>

#include "vk_device.h"
#include "vk_instance.h"
#include "vk_physical_device.h"
#include "vk_queue.h"

/* Generated (vk_entrypoints_gen.py --prefix infinigpu --proto --weak):
 * declares infinigpu_{instance,physical_device,device}_entrypoints tables and
 * VKAPI_ATTR prototypes for every infinigpu_* entrypoint. */
#include "infinigpu_entrypoints.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Advertised device apiVersion (physical device). */
#define INFINIGPU_API_VERSION VK_API_VERSION_1_3

struct infinigpu_instance {
   struct vk_instance vk;
};

struct infinigpu_physical_device {
   struct vk_physical_device vk;

   /* Open fd of /dev/dri/renderD128 whose drm name == "infinigpu". */
   int drm_fd;
};

struct infinigpu_queue {
   struct vk_queue vk;
   struct infinigpu_device *device;
};

struct infinigpu_device {
   struct vk_device vk;
   struct infinigpu_physical_device *physical_device;
   struct infinigpu_queue queue;
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

/* Minimal supported-extension tables.  Phase 0 advertises none: an empty
 * instance table means vkCreateInstance never fails on an unsupported enabled
 * extension, and an empty device table lets vkCreateDevice succeed with zero
 * enabled extensions (which is what vulkaninfo does). */
extern const struct vk_instance_extension_table infinigpu_instance_extensions;
extern const struct vk_device_extension_table infinigpu_device_extensions;

/* infinigpu_physical_device.c */
VkResult infinigpu_enumerate_physical_devices(struct vk_instance *vk_instance);
void infinigpu_physical_device_destroy(struct vk_physical_device *vk_pdev);

/* infinigpu_device.c */
VkResult infinigpu_queue_submit(struct vk_queue *vk_queue,
                                struct vk_queue_submit *submit);

#ifdef __cplusplus
}
#endif

#endif /* INFINIGPU_PRIVATE_H */
