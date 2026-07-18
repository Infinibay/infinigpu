/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * ICD entry glue.  The loader (interface v1) resolves everything through
 * vk_icdGetInstanceProcAddr.  vk_icdNegotiateLoaderICDInterfaceVersion and
 * vk_icdGetPhysicalDeviceProcAddr are provided (PUBLIC) by the lite runtime's
 * vk_instance.c and exported for us by the src/vulkan/vulkan.sym version
 * script (see meson.build link_args/link_depends).
 */

#include "infinigpu_private.h"

#include "util/macros.h" /* PUBLIC = __attribute__((visibility("default"))) */

/* PUBLIC overrides the target's -fvisibility=hidden so the symbol survives to
 * default visibility; the vulkan.sym version script then keeps it in the
 * dynamic table. Without PUBLIC it is compiled hidden and the loader can't find
 * it (the other two vk_icd* come PUBLIC from the linked lite runtime). This
 * matches venus's src/virtio/vulkan/vn_icd.h declaration exactly. */
PUBLIC
VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
vk_icdGetInstanceProcAddr(VkInstance instance, const char *pName)
{
   return infinigpu_GetInstanceProcAddr(instance, pName);
}
