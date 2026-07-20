/*
 * Copyright 2026 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * infinigpu ICD end-to-end validation for a REAL MESH (Phase-2b). Unlike
 * infinigpu_tri_test.c (a bufferless shader-generated triangle), this drives the
 * vertex-buffer path the way a real 3D app / game does:
 *
 *   vkCreateGraphicsPipelines(vertex-input: binding0 stride20, 2 attrs)  -> ICD
 *       captures the vertex-input layout
 *   vkCmdBindVertexBuffers2(binding 0) / vkCmdDraw(3)                     -> ICD
 *       records the bound VBO + the draw
 *   vkQueueSubmit + vkWaitForFences  -> ICD forwards a vk_op::FORWARDED_CMDLIST
 *       (real mesh) over DRM_IOCTL_INFINIGPU_SUBMIT_FORWARDED; the host replays it
 *       on the real GPU (reading the forwarded vertex buffer) and DMA-writes back.
 *
 * The vertex buffer (3 vertices: pos vec2 + colour vec3, stride 20) + shaders are
 * the same mesh the host replay's forwarded_vbo_triangle_renders_mesh_colors test
 * renders on the A5000, so a PASS here means the guest VBO recording + KMD + host
 * agree end-to-end.
 *
 * Build (in the guest, with the Vulkan loader):
 *   cc -O2 -o infinigpu_mesh_test infinigpu_mesh_test.c -lvulkan
 * Run (the guest's renderD128 must be the infinigpu node):
 *   VK_DRIVER_FILES=/usr/share/vulkan/icd.d/infinigpu_icd.x86_64.json ./infinigpu_mesh_test
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <vulkan/vulkan.h>

#include "infinigpu_mesh_spv.h"

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

/* 3 vertices, [x, y, r, g, b], stride 20 B — a triangle in NDC with primary colours. */
static const float MESH_VERTS[] = {
    0.0f, -0.6f,   1.0f, 0.0f, 0.0f, /* top    — red   */
   -0.6f,  0.6f,   0.0f, 1.0f, 0.0f, /* left   — green */
    0.6f,  0.6f,   0.0f, 0.0f, 1.0f, /* right  — blue  */
};

int
main(void)
{
   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "infinigpu-mesh-test",
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
      fprintf(stderr, "FAIL: no Vulkan physical device (infinigpu module loaded, /dev/dri/card0?)\n");
      return 1;
   }
   VkPhysicalDevice *pds = calloc(pd_count, sizeof(*pds));
   CHECK(vkEnumeratePhysicalDevices(instance, &pd_count, pds));
   /* REQUIRE the infinigpu device — never fall back to a software renderer, which would render
    * on the CPU and report a dishonest PASS. */
   VkPhysicalDevice phys = VK_NULL_HANDLE;
   for (uint32_t i = 0; i < pd_count; i++) {
      VkPhysicalDeviceProperties props;
      vkGetPhysicalDeviceProperties(pds[i], &props);
      if (strstr(props.deviceName, "infinigpu")) {
         phys = pds[i];
         break;
      }
   }
   free(pds);
   if (phys == VK_NULL_HANDLE) {
      fprintf(stderr, "FAIL: no 'infinigpu' Vulkan device enumerated — the ICD is not driving the "
                      "GPU render node. Refusing to fall back to a software renderer.\n");
      return 1;
   }

   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(phys, &props);
   printf("device: %s\n", props.deviceName);

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

   /* ---- vertex buffer (host-visible, dumb-buffer backed) ---- */
   VkBufferCreateInfo buf_ci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = sizeof(MESH_VERTS),
      .usage = VK_BUFFER_USAGE_VERTEX_BUFFER_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   VkBuffer vbuf;
   CHECK(vkCreateBuffer(dev, &buf_ci, NULL, &vbuf));

   VkMemoryRequirements vbmr;
   vkGetBufferMemoryRequirements(dev, vbuf, &vbmr);
   VkMemoryAllocateInfo vb_mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = vbmr.size,
      .memoryTypeIndex = 0,
   };
   VkDeviceMemory vbmem;
   CHECK(vkAllocateMemory(dev, &vb_mai, NULL, &vbmem));
   CHECK(vkBindBufferMemory(dev, vbuf, vbmem, 0));

   void *vbptr;
   CHECK(vkMapMemory(dev, vbmem, 0, VK_WHOLE_SIZE, 0, &vbptr));
   memcpy(vbptr, MESH_VERTS, sizeof(MESH_VERTS));
   vkUnmapMemory(dev, vbmem);

   /* ---- pipeline: two modules (vs, fs), each entry "main"; vertex-input binding 0 ---- */
   VkShaderModuleCreateInfo vs_ci = {
      .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(infinigpu_mesh_vs_spv),
      .pCode = infinigpu_mesh_vs_spv,
   };
   VkShaderModule vs_mod;
   CHECK(vkCreateShaderModule(dev, &vs_ci, NULL, &vs_mod));
   VkShaderModuleCreateInfo fs_ci = {
      .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(infinigpu_mesh_fs_spv),
      .pCode = infinigpu_mesh_fs_spv,
   };
   VkShaderModule fs_mod;
   CHECK(vkCreateShaderModule(dev, &fs_ci, NULL, &fs_mod));

   VkPipelineLayoutCreateInfo pl_ci = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
   };
   VkPipelineLayout layout;
   CHECK(vkCreatePipelineLayout(dev, &pl_ci, NULL, &layout));

   VkPipelineShaderStageCreateInfo stages[2] = {
      {
         .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .stage = VK_SHADER_STAGE_VERTEX_BIT,
         .module = vs_mod,
         .pName = "main",
      },
      {
         .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
         .module = fs_mod,
         .pName = "main",
      },
   };
   /* Vertex-input: one interleaved binding (stride 20), pos=vec2 @loc0 off0, colour=vec3 @loc1 off8. */
   VkVertexInputBindingDescription vib = {
      .binding = 0,
      .stride = 20,
      .inputRate = VK_VERTEX_INPUT_RATE_VERTEX,
   };
   VkVertexInputAttributeDescription via[2] = {
      { .location = 0, .binding = 0, .format = VK_FORMAT_R32G32_SFLOAT,    .offset = 0 },
      { .location = 1, .binding = 0, .format = VK_FORMAT_R32G32B32_SFLOAT, .offset = 8 },
   };
   VkPipelineVertexInputStateCreateInfo vin = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
      .vertexBindingDescriptionCount = 1,
      .pVertexBindingDescriptions = &vib,
      .vertexAttributeDescriptionCount = 2,
      .pVertexAttributeDescriptions = via,
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
   VkDeviceSize vboff = 0;
   vkCmdBindVertexBuffers(cmd, 0, 1, &vbuf, &vboff);
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
   uint32_t lit = 0, bgpx = 0;
   uint32_t saw_r = 0, saw_g = 0, saw_b = 0; /* the mesh's three vertex colours must all appear */
   FILE *ppm = fopen("infinigpu_mesh.ppm", "wb");
   if (ppm)
      fprintf(ppm, "P6\n%d %d\n255\n", W, H);
   for (uint32_t y = 0; y < H; y++) {
      const uint8_t *row = (const uint8_t *)ptr + sl.offset + (uint64_t)y * sl.rowPitch;
      for (uint32_t x = 0; x < W; x++) {
         const uint8_t *px = row + x * 4; /* R8G8B8A8 */
         if (abs(px[0] - bg[0]) > 12 || abs(px[1] - bg[1]) > 12 || abs(px[2] - bg[2]) > 12) {
            lit++;
            if (px[0] > 150 && px[1] < 100 && px[2] < 100) saw_r++;
            if (px[1] > 150 && px[0] < 100 && px[2] < 100) saw_g++;
            if (px[2] > 150 && px[0] < 100 && px[1] < 100) saw_b++;
         } else {
            bgpx++;
         }
         if (ppm)
            fwrite(px, 1, 3, ppm);
      }
   }
   if (ppm)
      fclose(ppm);
   vkUnmapMemory(dev, mem);

   printf("lit pixels: %u / %u  (background %u)\n", lit, W * H, bgpx);
   printf("vertex colours seen — red:%u green:%u blue:%u\n", saw_r, saw_g, saw_b);
   if (ppm)
      printf("wrote infinigpu_mesh.ppm\n");

   /* ---- teardown ---- */
   vkDestroyFence(dev, fence, NULL);
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyPipeline(dev, pipeline, NULL);
   vkDestroyPipelineLayout(dev, layout, NULL);
   vkDestroyShaderModule(dev, fs_mod, NULL);
   vkDestroyShaderModule(dev, vs_mod, NULL);
   vkFreeMemory(dev, vbmem, NULL);
   vkDestroyBuffer(dev, vbuf, NULL);
   vkDestroyImageView(dev, view, NULL);
   vkFreeMemory(dev, mem, NULL);
   vkDestroyImage(dev, image, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(instance, NULL);

   if (lit < 100) {
      fprintf(stderr, "FAIL: too few lit pixels (%u) — the forwarded mesh did not render\n", lit);
      return 1;
   }
   if (bgpx < (W * H) / 8) {
      fprintf(stderr, "FAIL: no cleared-background region (%u px) — likely an untouched buffer, "
                      "not a real mesh over the background\n", bgpx);
      return 1;
   }
   /* The Gouraud-shaded triangle interpolates from red→green→blue, so a correct render shows all
    * three primary vertex colours — proof the vertex buffer's per-vertex colours were read, not a
    * uniform fill. */
   if (saw_r == 0 || saw_g == 0 || saw_b == 0) {
      fprintf(stderr, "FAIL: missing a vertex colour (red:%u green:%u blue:%u) — the per-vertex "
                      "VBO colours did not reach the shader (mesh path broken)\n", saw_r, saw_g, saw_b);
      return 1;
   }
   printf("PASS\n");
   return 0;
}
