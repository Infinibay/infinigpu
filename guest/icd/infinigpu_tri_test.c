/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * infinigpu ICD end-to-end validation: a headless Vulkan app that renders the
 * built-in RGB triangle THROUGH THE STANDARD VULKAN API and writes the result to
 * a PPM. Unlike submit3d_test.c (which drove the DRM_IOCTL directly), this drives
 * the whole own-remoting stack the way a real app does:
 *
 *   vkCreateGraphicsPipelines(vs="vs_main", fs="fs_main")  -> ICD captures SPIR-V
 *   vkCmdBeginRendering / vkCmdDraw(3)                      -> ICD records the draw
 *   vkQueueSubmit + vkWaitForFences                         -> ICD forwards it over
 *       DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED; the host replays the SPIR-V on the
 *       real GPU and DMA-writes the pixels into the image's dumb buffer.
 *   vkMapMemory + write PPM                                 -> read the result back
 *
 * Build (in the guest, with the Vulkan loader + libdrm dev headers):
 *   cc -O2 -o infinigpu_tri_test infinigpu_tri_test.c -lvulkan
 * Run (points the loader at the infinigpu ICD; the guest's renderD128 must be the
 * infinigpu node):
 *   VK_DRIVER_FILES=/usr/share/vulkan/icd.d/infinigpu_icd.x86_64.json ./infinigpu_tri_test
 *
 * The SPIR-V is the same module the host replay is unit-tested against, so a PASS
 * here means the guest ICD + KMD + host agree end to end.
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <vulkan/vulkan.h>

#include "infinigpu_tri_spv.h"

#define W 256
#define H 256

#define CHECK(expr)                                                            \
   do {                                                                        \
      VkResult _r = (expr);                                                    \
      if (_r != VK_SUCCESS) {                                                  \
         fprintf(stderr, "FAIL %s = %d (line %d)\n", #expr, _r, __LINE__);     \
         return 1;                                                             \
      }                                                                        \
   } while (0)

int
main(void)
{
   /* ---- instance ---- */
   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "infinigpu-tri-test",
      .apiVersion = VK_API_VERSION_1_3,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance instance;
   CHECK(vkCreateInstance(&ici, NULL, &instance));

   uint32_t pd_count = 0;
   CHECK(vkEnumeratePhysicalDevices(instance, &pd_count, NULL));
   if (pd_count == 0) {
      fprintf(stderr, "FAIL: no Vulkan physical device (is renderD128 the infinigpu node?)\n");
      return 1;
   }
   VkPhysicalDevice *pds = calloc(pd_count, sizeof(*pds));
   CHECK(vkEnumeratePhysicalDevices(instance, &pd_count, pds));
   VkPhysicalDevice phys = pds[0];
   for (uint32_t i = 0; i < pd_count; i++) {
      VkPhysicalDeviceProperties props;
      vkGetPhysicalDeviceProperties(pds[i], &props);
      if (strstr(props.deviceName, "infinigpu")) {
         phys = pds[i];
         break;
      }
   }
   free(pds);

   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(phys, &props);
   printf("device: %s (api %u.%u.%u)\n", props.deviceName,
          VK_API_VERSION_MAJOR(props.apiVersion),
          VK_API_VERSION_MINOR(props.apiVersion),
          VK_API_VERSION_PATCH(props.apiVersion));

   /* ---- device (queue family 0, dynamic rendering + sync2) ---- */
   float qp = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = 0,
      .queueCount = 1,
      .pQueuePriorities = &qp,
   };
   VkPhysicalDeviceVulkan13Features f13 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
      .dynamicRendering = VK_TRUE,
      .synchronization2 = VK_TRUE,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &f13,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
   };
   VkDevice dev;
   CHECK(vkCreateDevice(phys, &dci, NULL, &dev));

   VkQueue queue;
   vkGetDeviceQueue(dev, 0, 0, &queue);

   const VkFormat fmt = VK_FORMAT_R8G8B8A8_UNORM;

   /* ---- color image (LINEAR, dumb-buffer backed) ---- */
   VkImageCreateInfo img_ci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = fmt,
      .extent = { W, H, 1 },
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_LINEAR,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   VkImage image;
   CHECK(vkCreateImage(dev, &img_ci, NULL, &image));

   VkImageMemoryRequirementsInfo2 mri = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2,
      .image = image,
   };
   VkMemoryRequirements2 mr = { .sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2 };
   vkGetImageMemoryRequirements2(dev, &mri, &mr);

   /* Dedicated allocation at offset 0 (the KMD writes the BO base). */
   VkMemoryDedicatedAllocateInfo ded = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
      .image = image,
   };
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &ded,
      .allocationSize = mr.memoryRequirements.size,
      .memoryTypeIndex = 0,
   };
   VkDeviceMemory mem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));

   VkBindImageMemoryInfo bind = {
      .sType = VK_STRUCTURE_TYPE_BIND_IMAGE_MEMORY_INFO,
      .image = image,
      .memory = mem,
      .memoryOffset = 0,
   };
   CHECK(vkBindImageMemory2(dev, 1, &bind));

   VkImageViewCreateInfo iv_ci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = image,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = fmt,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
   };
   VkImageView view;
   CHECK(vkCreateImageView(dev, &iv_ci, NULL, &view));

   /* ---- pipeline: forward vs_main + fs_main from the one triangle module ---- */
   VkShaderModuleCreateInfo sm_ci = {
      .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(infinigpu_tri_spv),
      .pCode = infinigpu_tri_spv,
   };
   VkShaderModule module;
   CHECK(vkCreateShaderModule(dev, &sm_ci, NULL, &module));

   VkPipelineLayoutCreateInfo pl_ci = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
   };
   VkPipelineLayout layout;
   CHECK(vkCreatePipelineLayout(dev, &pl_ci, NULL, &layout));

   VkPipelineShaderStageCreateInfo stages[2] = {
      {
         .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .stage = VK_SHADER_STAGE_VERTEX_BIT,
         .module = module,
         .pName = "vs_main",
      },
      {
         .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
         .module = module,
         .pName = "fs_main",
      },
   };
   VkPipelineVertexInputStateCreateInfo vin = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
   };
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
   };
   VkViewport vp = { 0, 0, W, H, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { W, H } };
   VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1,
      .pViewports = &vp,
      .scissorCount = 1,
      .pScissors = &sc,
   };
   VkPipelineRasterizationStateCreateInfo rs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .polygonMode = VK_POLYGON_MODE_FILL,
      .cullMode = VK_CULL_MODE_NONE,
      .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE,
      .lineWidth = 1.0f,
   };
   VkPipelineMultisampleStateCreateInfo ms = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
      .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT,
   };
   VkPipelineColorBlendAttachmentState cba = {
      .colorWriteMask = VK_COLOR_COMPONENT_R_BIT | VK_COLOR_COMPONENT_G_BIT |
                        VK_COLOR_COMPONENT_B_BIT | VK_COLOR_COMPONENT_A_BIT,
   };
   VkPipelineColorBlendStateCreateInfo cb = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
      .attachmentCount = 1,
      .pAttachments = &cba,
   };
   VkPipelineRenderingCreateInfo prc = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,
      .colorAttachmentCount = 1,
      .pColorAttachmentFormats = &fmt,
   };
   VkGraphicsPipelineCreateInfo gp_ci = {
      .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
      .pNext = &prc,
      .stageCount = 2,
      .pStages = stages,
      .pVertexInputState = &vin,
      .pInputAssemblyState = &ia,
      .pViewportState = &vps,
      .pRasterizationState = &rs,
      .pMultisampleState = &ms,
      .pColorBlendState = &cb,
      .layout = layout,
   };
   VkPipeline pipeline;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gp_ci, NULL, &pipeline));

   /* ---- record + submit ---- */
   VkCommandPoolCreateInfo cp_ci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = 0,
   };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &cp_ci, NULL, &pool));

   VkCommandBufferAllocateInfo cb_ai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cmd;
   CHECK(vkAllocateCommandBuffers(dev, &cb_ai, &cmd));

   VkCommandBufferBeginInfo begin = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(vkBeginCommandBuffer(cmd, &begin));

   VkRenderingAttachmentInfo color = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = view,
      .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .clearValue = { .color = { .float32 = { 0.1f, 0.1f, 0.12f, 1.0f } } },
   };
   VkRenderingInfo rinfo = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
      .renderArea = { { 0, 0 }, { W, H } },
      .layerCount = 1,
      .colorAttachmentCount = 1,
      .pColorAttachments = &color,
   };
   vkCmdBeginRendering(cmd, &rinfo);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipeline);
   vkCmdSetViewport(cmd, 0, 1, &vp);
   vkCmdSetScissor(cmd, 0, 1, &sc);
   vkCmdDraw(cmd, 3, 1, 0, 0);
   vkCmdEndRendering(cmd);
   CHECK(vkEndCommandBuffer(cmd));

   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));

   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cmd,
   };
   CHECK(vkQueueSubmit(queue, 1, &si, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));

   /* ---- read back ---- */
   VkImageSubresource sub = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0 };
   VkSubresourceLayout sl;
   vkGetImageSubresourceLayout(dev, image, &sub, &sl);

   void *ptr;
   CHECK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, &ptr));

   const uint8_t bg[3] = { 26, 26, 31 }; /* ~ the clear colour in 8-bit */
   uint32_t lit = 0;
   FILE *ppm = fopen("infinigpu_tri.ppm", "wb");
   if (ppm)
      fprintf(ppm, "P6\n%d %d\n255\n", W, H);
   for (uint32_t y = 0; y < H; y++) {
      const uint8_t *row = (const uint8_t *)ptr + sl.offset + (uint64_t)y * sl.rowPitch;
      for (uint32_t x = 0; x < W; x++) {
         const uint8_t *px = row + x * 4; /* R8G8B8A8 */
         if (abs(px[0] - bg[0]) > 12 || abs(px[1] - bg[1]) > 12 || abs(px[2] - bg[2]) > 12)
            lit++;
         if (ppm)
            fwrite(px, 1, 3, ppm);
      }
   }
   if (ppm)
      fclose(ppm);
   vkUnmapMemory(dev, mem);

   printf("lit (non-background) pixels: %u / %u\n", lit, W * H);
   if (ppm)
      printf("wrote infinigpu_tri.ppm\n");

   /* ---- teardown ---- */
   vkDestroyFence(dev, fence, NULL);
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyPipeline(dev, pipeline, NULL);
   vkDestroyPipelineLayout(dev, layout, NULL);
   vkDestroyShaderModule(dev, module, NULL);
   vkDestroyImageView(dev, view, NULL);
   vkFreeMemory(dev, mem, NULL);
   vkDestroyImage(dev, image, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(instance, NULL);

   if (lit < 100) {
      fprintf(stderr, "FAIL: too few lit pixels (%u) — the forwarded triangle did not render\n", lit);
      return 1;
   }
   printf("PASS\n");
   return 0;
}
