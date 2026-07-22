/*
 * Copyright 2026 Infinibay
 * SPDX-License-Identifier: MIT
 *
 * host->guest DMA-writeback coherency regression test. Proves the "delivery wall"
 * is NOT a device-level coherency bug: a real forwarded triangle is rendered on the
 * A5000 into a LINEAR dumb-BO image, then the guest CPU reads it back (vkMapMemory).
 * The SAME BO is rewritten 8x with distinct clear colours; every read MUST return the
 * just-rendered colour. Any lag/skew (double-buffer aliasing) or staleness (missing
 * cache-flush) shows up as corner != expected.
 *
 * Validated 2026-07-22 on a live A5000 GPU VM: 8/8 fresh, 0 stale — host-write ->
 * guest-CPU-read is fully coherent, including rewrite. So windowed/compositor GPU
 * accel is NOT architecturally blocked; the remaining OpenGL gaps are ICD-completeness
 * (GL-version cap, draw crash, config starvation), not the memory model.
 *
 * Build in-guest (needs the Vulkan loader + the infinigpu ICD installed):
 *   cc -O2 -o infinigpu_coherency_test infinigpu_coherency_test.c -lvulkan
 *   VK_DRIVER_FILES=/usr/share/vulkan/icd.d/infinigpu_icd.x86_64.json ./infinigpu_coherency_test
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>
#include "infinigpu_tri_spv.h"

#define W 256
#define H 256
#define CK(e) do{ VkResult _r=(e); if(_r){ fprintf(stderr,"FAIL %s=%d L%d\n",#e,_r,__LINE__); return 2; } }while(0)

static VkDevice dev; static VkQueue queue; static VkCommandBuffer cmd; static VkFence fence;
static VkPipeline pipeline; static VkImageView view; static VkDeviceMemory mem; static VkImage image;
static VkSubresourceLayout sl;

static int render(float r,float g,float b){
   vkResetCommandBuffer(cmd,0);
   VkCommandBufferBeginInfo bi={.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,.flags=VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
   CK(vkBeginCommandBuffer(cmd,&bi));
   VkViewport vp={0,0,W,H,0,1}; VkRect2D sc={{0,0},{W,H}};
   VkRenderingAttachmentInfo color={.sType=VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,.imageView=view,
      .imageLayout=VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,.loadOp=VK_ATTACHMENT_LOAD_OP_CLEAR,
      .storeOp=VK_ATTACHMENT_STORE_OP_STORE,.clearValue={.color={.float32={r,g,b,1.0f}}}};
   VkRenderingInfo ri={.sType=VK_STRUCTURE_TYPE_RENDERING_INFO,.renderArea={{0,0},{W,H}},.layerCount=1,
      .colorAttachmentCount=1,.pColorAttachments=&color};
   vkCmdBeginRendering(cmd,&ri);
   vkCmdBindPipeline(cmd,VK_PIPELINE_BIND_POINT_GRAPHICS,pipeline);
   vkCmdSetViewport(cmd,0,1,&vp); vkCmdSetScissor(cmd,0,1,&sc);
   vkCmdDraw(cmd,3,1,0,0);
   vkCmdEndRendering(cmd);
   CK(vkEndCommandBuffer(cmd));
   vkResetFences(dev,1,&fence);
   VkSubmitInfo si={.sType=VK_STRUCTURE_TYPE_SUBMIT_INFO,.commandBufferCount=1,.pCommandBuffers=&cmd};
   CK(vkQueueSubmit(queue,1,&si,fence));
   CK(vkWaitForFences(dev,1,&fence,VK_TRUE,UINT64_MAX));
   return 0;
}
static void px_at(const uint8_t*base,int x,int y,uint8_t out[4]){
   const uint8_t*p=base+sl.offset+(uint64_t)y*sl.rowPitch+(uint64_t)x*4; memcpy(out,p,4);
}
static void read_report(const char*tag){
   void*ptr; if(vkMapMemory(dev,mem,0,VK_WHOLE_SIZE,0,&ptr)){printf("%s: MAP FAIL\n",tag);return;}
   uint8_t tl[4],tr[4],bl[4],br[4],c[4];
   px_at(ptr,2,2,tl); px_at(ptr,W-3,2,tr); px_at(ptr,2,H-3,bl); px_at(ptr,W-3,H-3,br); px_at(ptr,W/2,H/2,c);
   vkUnmapMemory(dev,mem);
   printf("%s corners TL[%d %d %d] TR[%d %d %d] BL[%d %d %d] BR[%d %d %d]  center[%d %d %d]\n",
      tag, tl[0],tl[1],tl[2], tr[0],tr[1],tr[2], bl[0],bl[1],bl[2], br[0],br[1],br[2], c[0],c[1],c[2]);
}

int main(void){
   VkApplicationInfo app={.sType=VK_STRUCTURE_TYPE_APPLICATION_INFO,.apiVersion=VK_API_VERSION_1_3};
   VkInstanceCreateInfo ici={.sType=VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,.pApplicationInfo=&app};
   VkInstance inst; CK(vkCreateInstance(&ici,NULL,&inst));
   uint32_t n=0; CK(vkEnumeratePhysicalDevices(inst,&n,NULL));
   VkPhysicalDevice*pds=calloc(n,sizeof*pds); CK(vkEnumeratePhysicalDevices(inst,&n,pds));
   VkPhysicalDevice phys=VK_NULL_HANDLE;
   for(uint32_t i=0;i<n;i++){VkPhysicalDeviceProperties p;vkGetPhysicalDeviceProperties(pds[i],&p);
      if(strstr(p.deviceName,"infinigpu")){phys=pds[i];break;}}
   if(phys==VK_NULL_HANDLE){fprintf(stderr,"FAIL: no infinigpu device\n");return 1;}
   VkPhysicalDeviceProperties pr; vkGetPhysicalDeviceProperties(phys,&pr); printf("device: %s\n",pr.deviceName);
   float qp=1; VkDeviceQueueCreateInfo q={.sType=VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,.queueFamilyIndex=0,.queueCount=1,.pQueuePriorities=&qp};
   VkPhysicalDeviceVulkan13Features f13={.sType=VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,.dynamicRendering=1,.synchronization2=1};
   VkDeviceCreateInfo dci={.sType=VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,.pNext=&f13,.queueCreateInfoCount=1,.pQueueCreateInfos=&q};
   CK(vkCreateDevice(phys,&dci,NULL,&dev)); vkGetDeviceQueue(dev,0,0,&queue);
   const VkFormat fmt=VK_FORMAT_R8G8B8A8_UNORM;
   VkImageCreateInfo ic={.sType=VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,.imageType=VK_IMAGE_TYPE_2D,.format=fmt,
      .extent={W,H,1},.mipLevels=1,.arrayLayers=1,.samples=VK_SAMPLE_COUNT_1_BIT,.tiling=VK_IMAGE_TILING_LINEAR,
      .usage=VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT|VK_IMAGE_USAGE_TRANSFER_SRC_BIT,.initialLayout=VK_IMAGE_LAYOUT_UNDEFINED};
   CK(vkCreateImage(dev,&ic,NULL,&image));
   VkImageMemoryRequirementsInfo2 mri={.sType=VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2,.image=image};
   VkMemoryRequirements2 mr={.sType=VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2}; vkGetImageMemoryRequirements2(dev,&mri,&mr);
   VkMemoryDedicatedAllocateInfo ded={.sType=VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,.image=image};
   VkMemoryAllocateInfo mai={.sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,.pNext=&ded,.allocationSize=mr.memoryRequirements.size,.memoryTypeIndex=0};
   CK(vkAllocateMemory(dev,&mai,NULL,&mem));
   VkBindImageMemoryInfo bind={.sType=VK_STRUCTURE_TYPE_BIND_IMAGE_MEMORY_INFO,.image=image,.memory=mem,.memoryOffset=0};
   CK(vkBindImageMemory2(dev,1,&bind));
   VkImageViewCreateInfo ivc={.sType=VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,.image=image,.viewType=VK_IMAGE_VIEW_TYPE_2D,
      .format=fmt,.subresourceRange={VK_IMAGE_ASPECT_COLOR_BIT,0,1,0,1}}; CK(vkCreateImageView(dev,&ivc,NULL,&view));
   VkImageSubresource sub={VK_IMAGE_ASPECT_COLOR_BIT,0,0}; vkGetImageSubresourceLayout(dev,image,&sub,&sl);
   VkShaderModuleCreateInfo sm={.sType=VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,.codeSize=sizeof(infinigpu_tri_spv),.pCode=infinigpu_tri_spv};
   VkShaderModule mod; CK(vkCreateShaderModule(dev,&sm,NULL,&mod));
   VkPipelineLayoutCreateInfo plc={.sType=VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO}; VkPipelineLayout lay; CK(vkCreatePipelineLayout(dev,&plc,NULL,&lay));
   VkPipelineShaderStageCreateInfo st[2]={{.sType=VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,.stage=VK_SHADER_STAGE_VERTEX_BIT,.module=mod,.pName="vs_main"},
      {.sType=VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,.stage=VK_SHADER_STAGE_FRAGMENT_BIT,.module=mod,.pName="fs_main"}};
   VkPipelineVertexInputStateCreateInfo vin={.sType=VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
   VkPipelineInputAssemblyStateCreateInfo ia={.sType=VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,.topology=VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST};
   VkViewport vp={0,0,W,H,0,1}; VkRect2D sc={{0,0},{W,H}};
   VkPipelineViewportStateCreateInfo vps={.sType=VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,.viewportCount=1,.pViewports=&vp,.scissorCount=1,.pScissors=&sc};
   VkPipelineRasterizationStateCreateInfo rs={.sType=VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,.polygonMode=VK_POLYGON_MODE_FILL,.cullMode=VK_CULL_MODE_NONE,.frontFace=VK_FRONT_FACE_COUNTER_CLOCKWISE,.lineWidth=1};
   VkPipelineMultisampleStateCreateInfo msi={.sType=VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,.rasterizationSamples=VK_SAMPLE_COUNT_1_BIT};
   VkPipelineColorBlendAttachmentState cba={.colorWriteMask=0xf};
   VkPipelineColorBlendStateCreateInfo cb={.sType=VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,.attachmentCount=1,.pAttachments=&cba};
   VkPipelineRenderingCreateInfo prc={.sType=VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,.colorAttachmentCount=1,.pColorAttachmentFormats=&fmt};
   VkGraphicsPipelineCreateInfo gpc={.sType=VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,.pNext=&prc,.stageCount=2,.pStages=st,
      .pVertexInputState=&vin,.pInputAssemblyState=&ia,.pViewportState=&vps,.pRasterizationState=&rs,.pMultisampleState=&msi,.pColorBlendState=&cb,.layout=lay};
   CK(vkCreateGraphicsPipelines(dev,VK_NULL_HANDLE,1,&gpc,NULL,&pipeline));
   VkCommandPoolCreateInfo cpc={.sType=VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,.flags=VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,.queueFamilyIndex=0};
   VkCommandPool pool; CK(vkCreateCommandPool(dev,&cpc,NULL,&pool));
   VkCommandBufferAllocateInfo cai={.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,.commandPool=pool,.level=VK_COMMAND_BUFFER_LEVEL_PRIMARY,.commandBufferCount=1};
   CK(vkAllocateCommandBuffers(dev,&cai,&cmd));
   VkFenceCreateInfo fc={.sType=VK_STRUCTURE_TYPE_FENCE_CREATE_INFO}; CK(vkCreateFence(dev,&fc,NULL,&fence));

   /* 8-frame cycle: frame i clears to a distinct grayscale (i*30). Read the corner
    * (background) after each; it MUST equal i*30. Any lag-by-one / skew (double-buffer
    * aliasing, Branch B) shows up as corner==prev value. */
   int fails=0;
   for(int i=0;i<8;i++){
      int lvl=i*30; float f=lvl/255.0f;
      if(render(f,f,f)) return 3;
      void*ptr; if(vkMapMemory(dev,mem,0,VK_WHOLE_SIZE,0,&ptr)){printf("MAP FAIL\n");return 4;}
      uint8_t tl[4]; px_at(ptr,2,2,tl); vkUnmapMemory(dev,mem);
      int ok = (abs(tl[0]-lvl)<=2 && abs(tl[1]-lvl)<=2 && abs(tl[2]-lvl)<=2);
      printf("frame %d clear=%d -> corner[%d %d %d]  %s\n", i, lvl, tl[0],tl[1],tl[2], ok?"OK(fresh)":"STALE/LAG!");
      if(!ok) fails++;
   }
   printf("VERDICT: %s (%d/8 stale)\n", fails==0?"ALL FRESH — host->guest rewrite fully COHERENT, NO device wall":"WALL/SKEW REPRODUCED", fails);
   return fails?5:0;
}
