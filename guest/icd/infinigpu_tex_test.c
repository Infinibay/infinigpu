/*
 * Copyright 2024 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * infinigpu ICD end-to-end validation for a TEXTURED mesh (Phase-2c). Drives the
 * full descriptor + sampled-texture path a real 3D app / game uses:
 *
 *   vkCreateDescriptorSetLayout(b0 sampled image, b1 sampler) / vkCreateSampler
 *   vkCmdCopyBufferToImage(staging -> texture)   -> ICD records a texture upload
 *   vkUpdateDescriptorSets / vkCmdBindDescriptorSets  -> ICD captures the texture
 *   vkCmdBindVertexBuffers2 + vkCmdBindIndexBuffer + vkCmdDrawIndexed(6)  -> ICD
 *       records a textured quad
 *   vkQueueSubmit  -> ICD forwards a vk_op::FORWARDED_CMDLIST with the RGBA8 texture
 *       pixels; the host uploads them to an image + binds a descriptor set and the
 *       fragment shader textureSample()s them.
 *
 * The 2x2 texture (red/green/blue/white) + quad + shaders are the same the host
 * replay's forwarded_texture_samples_onto_a_quad test renders on the A5000, so a
 * PASS means the guest descriptor/upload/sampling path + KMD + host agree.
 *
 * Build:  cc -O2 -o infinigpu_tex_test infinigpu_tex_test.c -lvulkan
 * Run:    VK_DRIVER_FILES=/usr/share/vulkan/icd.d/infinigpu_icd.x86_64.json ./infinigpu_tex_test
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <vulkan/vulkan.h>

#include "infinigpu_tex_spv.h"

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

/* Fullscreen quad: pos(vec2) + uv(vec2), stride 16. */
static const float QUAD[] = {
   -1.0f, -1.0f,  0.0f, 0.0f,
    1.0f, -1.0f,  1.0f, 0.0f,
    1.0f,  1.0f,  1.0f, 1.0f,
   -1.0f,  1.0f,  0.0f, 1.0f,
};
static const uint16_t QUAD_IDX[6] = { 0, 1, 2, 0, 2, 3 };

/* 2x2 RGBA8 texture: red, green / blue, white. */
static const uint8_t TEX_PIXELS[16] = {
   255, 0, 0, 255,   0, 255, 0, 255,
   0, 0, 255, 255,   255, 255, 255, 255,
};

/* Allocate a dumb-buffer-backed VkDeviceMemory for a buffer + bind + map + fill. */
static VkResult
make_filled_buffer(VkDevice dev, VkBufferUsageFlags usage, const void *data, size_t size,
                   VkBuffer *out_buf, VkDeviceMemory *out_mem)
{
   VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size,
      .usage = usage,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   VkResult r = vkCreateBuffer(dev, &bci, NULL, out_buf);
   if (r != VK_SUCCESS)
      return r;
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, *out_buf, &mr);
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex = 0,
   };
   r = vkAllocateMemory(dev, &mai, NULL, out_mem);
   if (r != VK_SUCCESS)
      return r;
   r = vkBindBufferMemory(dev, *out_buf, *out_mem, 0);
   if (r != VK_SUCCESS)
      return r;
   void *p;
   r = vkMapMemory(dev, *out_mem, 0, VK_WHOLE_SIZE, 0, &p);
   if (r != VK_SUCCESS)
      return r;
   memcpy(p, data, size);
   vkUnmapMemory(dev, *out_mem);
   return VK_SUCCESS;
}

int
main(void)
{
   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "infinigpu-tex-test",
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
      fprintf(stderr, "FAIL: no Vulkan physical device\n");
      return 1;
   }
   VkPhysicalDevice *pds = calloc(pd_count, sizeof(*pds));
   CHECK(vkEnumeratePhysicalDevices(instance, &pd_count, pds));
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
      fprintf(stderr, "FAIL: no 'infinigpu' Vulkan device — refusing to fall back to software.\n");
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

   /* ---- color target ---- */
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
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2, .image = image,
   };
   VkMemoryRequirements2 mr = { .sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2 };
   vkGetImageMemoryRequirements2(dev, &mri, &mr);
   VkMemoryDedicatedAllocateInfo ded = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO, .image = image,
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
      .image = image, .memory = mem, .memoryOffset = 0,
   };
   CHECK(vkBindImageMemory2(dev, 1, &bind));
   VkImageViewCreateInfo iv_ci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = image, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = fmt,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
   };
   VkImageView view;
   CHECK(vkCreateImageView(dev, &iv_ci, NULL, &view));

   /* ---- texture image (2x2 RGBA8, sampled) + staging buffer ---- */
   VkImageCreateInfo tex_ci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = fmt,
      .extent = { 2, 2, 1 },
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_LINEAR,
      .usage = VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   VkImage tex;
   CHECK(vkCreateImage(dev, &tex_ci, NULL, &tex));
   VkImageMemoryRequirementsInfo2 tmri = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2, .image = tex,
   };
   VkMemoryRequirements2 tmr = { .sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2 };
   vkGetImageMemoryRequirements2(dev, &tmri, &tmr);
   VkMemoryDedicatedAllocateInfo tded = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO, .image = tex,
   };
   VkMemoryAllocateInfo tmai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &tded,
      .allocationSize = tmr.memoryRequirements.size,
      .memoryTypeIndex = 0,
   };
   VkDeviceMemory texmem;
   CHECK(vkAllocateMemory(dev, &tmai, NULL, &texmem));
   VkBindImageMemoryInfo texbind = {
      .sType = VK_STRUCTURE_TYPE_BIND_IMAGE_MEMORY_INFO,
      .image = tex, .memory = texmem, .memoryOffset = 0,
   };
   CHECK(vkBindImageMemory2(dev, 1, &texbind));
   VkImageViewCreateInfo tiv_ci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = tex, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = fmt,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
   };
   VkImageView texview;
   CHECK(vkCreateImageView(dev, &tiv_ci, NULL, &texview));

   VkBuffer staging;
   VkDeviceMemory stagingmem;
   CHECK(make_filled_buffer(dev, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, TEX_PIXELS,
                            sizeof(TEX_PIXELS), &staging, &stagingmem));

   /* ---- sampler ---- */
   VkSamplerCreateInfo samp_ci = {
      .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
      .magFilter = VK_FILTER_NEAREST,
      .minFilter = VK_FILTER_NEAREST,
      .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
      .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
      .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
   };
   VkSampler sampler;
   CHECK(vkCreateSampler(dev, &samp_ci, NULL, &sampler));

   /* ---- vertex + index buffers ---- */
   VkBuffer vbuf, ibuf;
   VkDeviceMemory vbmem, ibmem;
   CHECK(make_filled_buffer(dev, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT, QUAD, sizeof(QUAD),
                            &vbuf, &vbmem));
   CHECK(make_filled_buffer(dev, VK_BUFFER_USAGE_INDEX_BUFFER_BIT, QUAD_IDX, sizeof(QUAD_IDX),
                            &ibuf, &ibmem));

   /* ---- descriptor set layout (b0 sampled image, b1 sampler) + pool + set ---- */
   VkDescriptorSetLayoutBinding dslb[2] = {
      { .binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE, .descriptorCount = 1,
        .stageFlags = VK_SHADER_STAGE_FRAGMENT_BIT },
      { .binding = 1, .descriptorType = VK_DESCRIPTOR_TYPE_SAMPLER, .descriptorCount = 1,
        .stageFlags = VK_SHADER_STAGE_FRAGMENT_BIT },
   };
   VkDescriptorSetLayoutCreateInfo dsl_ci = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 2, .pBindings = dslb,
   };
   VkDescriptorSetLayout dsl;
   CHECK(vkCreateDescriptorSetLayout(dev, &dsl_ci, NULL, &dsl));

   VkDescriptorPoolSize psizes[2] = {
      { .type = VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE, .descriptorCount = 1 },
      { .type = VK_DESCRIPTOR_TYPE_SAMPLER, .descriptorCount = 1 },
   };
   VkDescriptorPoolCreateInfo dp_ci = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
      .maxSets = 1, .poolSizeCount = 2, .pPoolSizes = psizes,
   };
   VkDescriptorPool dpool;
   CHECK(vkCreateDescriptorPool(dev, &dp_ci, NULL, &dpool));

   VkDescriptorSetAllocateInfo ds_ai = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
      .descriptorPool = dpool, .descriptorSetCount = 1, .pSetLayouts = &dsl,
   };
   VkDescriptorSet dset;
   CHECK(vkAllocateDescriptorSets(dev, &ds_ai, &dset));

   VkDescriptorImageInfo dii_img = { .imageView = texview,
                                     .imageLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
   VkDescriptorImageInfo dii_samp = { .sampler = sampler };
   VkWriteDescriptorSet writes[2] = {
      { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = dset, .dstBinding = 0,
        .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE, .pImageInfo = &dii_img },
      { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = dset, .dstBinding = 1,
        .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_SAMPLER, .pImageInfo = &dii_samp },
   };
   vkUpdateDescriptorSets(dev, 2, writes, 0, NULL);

   /* ---- pipeline (2 modules, vertex-input pos+uv, layout with the DSL) ---- */
   VkShaderModuleCreateInfo vs_ci = {
      .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(infinigpu_tex_vs_spv), .pCode = infinigpu_tex_vs_spv,
   };
   VkShaderModule vs_mod;
   CHECK(vkCreateShaderModule(dev, &vs_ci, NULL, &vs_mod));
   VkShaderModuleCreateInfo fs_ci = {
      .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(infinigpu_tex_fs_spv), .pCode = infinigpu_tex_fs_spv,
   };
   VkShaderModule fs_mod;
   CHECK(vkCreateShaderModule(dev, &fs_ci, NULL, &fs_mod));

   VkPipelineLayoutCreateInfo pl_ci = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1, .pSetLayouts = &dsl,
   };
   VkPipelineLayout layout;
   CHECK(vkCreatePipelineLayout(dev, &pl_ci, NULL, &layout));

   VkPipelineShaderStageCreateInfo stages[2] = {
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vs_mod, .pName = "main" },
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fs_mod, .pName = "main" },
   };
   VkVertexInputBindingDescription vib = {
      .binding = 0, .stride = 16, .inputRate = VK_VERTEX_INPUT_RATE_VERTEX,
   };
   VkVertexInputAttributeDescription via[2] = {
      { .location = 0, .binding = 0, .format = VK_FORMAT_R32G32_SFLOAT, .offset = 0 },
      { .location = 1, .binding = 0, .format = VK_FORMAT_R32G32_SFLOAT, .offset = 8 },
   };
   VkPipelineVertexInputStateCreateInfo vin = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
      .vertexBindingDescriptionCount = 1, .pVertexBindingDescriptions = &vib,
      .vertexAttributeDescriptionCount = 2, .pVertexAttributeDescriptions = via,
   };
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
   };
   VkViewport vp = { 0, 0, W, H, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { W, H } };
   VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc,
   };
   VkPipelineRasterizationStateCreateInfo rs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE,
      .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE, .lineWidth = 1.0f,
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
      .attachmentCount = 1, .pAttachments = &cba,
   };
   VkPipelineRenderingCreateInfo prc = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,
      .colorAttachmentCount = 1, .pColorAttachmentFormats = &fmt,
   };
   VkGraphicsPipelineCreateInfo gp_ci = {
      .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
      .pNext = &prc, .stageCount = 2, .pStages = stages,
      .pVertexInputState = &vin, .pInputAssemblyState = &ia, .pViewportState = &vps,
      .pRasterizationState = &rs, .pMultisampleState = &ms, .pColorBlendState = &cb,
      .layout = layout,
   };
   VkPipeline pipeline;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gp_ci, NULL, &pipeline));

   /* ---- record + submit ---- */
   VkCommandPoolCreateInfo cp_ci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = 0,
   };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &cp_ci, NULL, &pool));
   VkCommandBufferAllocateInfo cb_ai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1,
   };
   VkCommandBuffer cmd;
   CHECK(vkAllocateCommandBuffers(dev, &cb_ai, &cmd));

   VkCommandBufferBeginInfo begin = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(vkBeginCommandBuffer(cmd, &begin));

   /* Upload the texture (staging -> texture image). Layout transitions are no-ops in this
    * synchronous ICD; the copy lands the pixels in the texture's LINEAR memory. */
   VkBufferImageCopy region = {
      .bufferOffset = 0,
      .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
      .imageExtent = { 2, 2, 1 },
   };
   vkCmdCopyBufferToImage(cmd, staging, tex, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &region);

   VkRenderingAttachmentInfo color = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = view, .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .clearValue = { .color = { .float32 = { 0.1f, 0.1f, 0.12f, 1.0f } } },
   };
   VkRenderingInfo rinfo = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
      .renderArea = { { 0, 0 }, { W, H } }, .layerCount = 1,
      .colorAttachmentCount = 1, .pColorAttachments = &color,
   };
   vkCmdBeginRendering(cmd, &rinfo);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipeline);
   vkCmdSetViewport(cmd, 0, 1, &vp);
   vkCmdSetScissor(cmd, 0, 1, &sc);
   vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, layout, 0, 1, &dset, 0, NULL);
   VkDeviceSize vboff = 0;
   vkCmdBindVertexBuffers(cmd, 0, 1, &vbuf, &vboff);
   vkCmdBindIndexBuffer(cmd, ibuf, 0, VK_INDEX_TYPE_UINT16);
   vkCmdDrawIndexed(cmd, 6, 1, 0, 0, 0);
   vkCmdEndRendering(cmd);
   CHECK(vkEndCommandBuffer(cmd));

   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cmd,
   };
   CHECK(vkQueueSubmit(queue, 1, &si, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));

   /* ---- read back ---- */
   VkImageSubresource sub = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0 };
   VkSubresourceLayout sl;
   vkGetImageSubresourceLayout(dev, image, &sub, &sl);
   void *ptr;
   CHECK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, &ptr));

   uint32_t saw_r = 0, saw_g = 0, saw_b = 0, saw_w = 0;
   FILE *ppm = fopen("infinigpu_tex.ppm", "wb");
   if (ppm)
      fprintf(ppm, "P6\n%d %d\n255\n", W, H);
   for (uint32_t y = 0; y < H; y++) {
      const uint8_t *row = (const uint8_t *)ptr + sl.offset + (uint64_t)y * sl.rowPitch;
      for (uint32_t x = 0; x < W; x++) {
         const uint8_t *px = row + x * 4;
         if (px[0] > 150 && px[1] < 100 && px[2] < 100) saw_r++;
         if (px[1] > 150 && px[0] < 100 && px[2] < 100) saw_g++;
         if (px[2] > 150 && px[0] < 100 && px[1] < 100) saw_b++;
         if (px[0] > 150 && px[1] > 150 && px[2] > 150) saw_w++;
         if (ppm)
            fwrite(px, 1, 3, ppm);
      }
   }
   if (ppm)
      fclose(ppm);
   vkUnmapMemory(dev, mem);

   printf("texel colours seen — red:%u green:%u blue:%u white:%u\n", saw_r, saw_g, saw_b, saw_w);
   if (ppm)
      printf("wrote infinigpu_tex.ppm\n");

   /* ---- teardown ---- */
   vkDestroyFence(dev, fence, NULL);
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyPipeline(dev, pipeline, NULL);
   vkDestroyPipelineLayout(dev, layout, NULL);
   vkDestroyShaderModule(dev, fs_mod, NULL);
   vkDestroyShaderModule(dev, vs_mod, NULL);
   vkDestroyDescriptorPool(dev, dpool, NULL);
   vkDestroyDescriptorSetLayout(dev, dsl, NULL);
   vkDestroySampler(dev, sampler, NULL);
   vkFreeMemory(dev, ibmem, NULL);
   vkDestroyBuffer(dev, ibuf, NULL);
   vkFreeMemory(dev, vbmem, NULL);
   vkDestroyBuffer(dev, vbuf, NULL);
   vkFreeMemory(dev, stagingmem, NULL);
   vkDestroyBuffer(dev, staging, NULL);
   vkDestroyImageView(dev, texview, NULL);
   vkFreeMemory(dev, texmem, NULL);
   vkDestroyImage(dev, tex, NULL);
   vkDestroyImageView(dev, view, NULL);
   vkFreeMemory(dev, mem, NULL);
   vkDestroyImage(dev, image, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(instance, NULL);

   /* A correct textured quad shows all four texel colours (the 2x2 texture mapped over the quad).
    * Missing any one means the texture pixels or the sampling didn't reach the shader. */
   if (saw_r == 0 || saw_g == 0 || saw_b == 0 || saw_w == 0) {
      fprintf(stderr, "FAIL: missing a texel colour (r:%u g:%u b:%u w:%u) — the sampled texture did "
                      "not reach the shader (descriptor/upload/sampling path broken)\n",
              saw_r, saw_g, saw_b, saw_w);
      return 1;
   }
   printf("PASS\n");
   return 0;
}
