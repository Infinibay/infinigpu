//! # infinigpu-replay
//!
//! The host-side **Vulkan replay backend** (ADR-0002/0003, Phase-0 Step 5). In the
//! full system this process runs *jailed, one per VM*, decodes a guest command
//! ring, and replays the guest's Vulkan workload against the physical GPU, then
//! presents the result blob. This crate is the GPU-facing half.
//!
//! Right now it exposes [`HostGpu`] — a headless Vulkan context on the physical
//! card (preferring the NVIDIA proprietary driver, which gives us Vulkan for free,
//! no vGPU license) — and [`HostGpu::render_clear`], a minimal render-pass workload
//! that proves the whole submit → execute → fence → DMA-readback datapath runs on
//! real silicon without QEMU. Real command-stream replay layers on top of this.

use ash::vk;
use std::error::Error;
use std::ffi::CStr;

type R<T> = Result<T, Box<dyn Error>>;

/// A rendered frame read back from the GPU into host memory.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed `R8G8B8A8_UNORM` pixels, `width*height*4` bytes.
    pub rgba: Vec<u8>,
}

impl Frame {
    /// Serialize as a binary PPM (P6, RGB — alpha dropped). Openable by any image
    /// viewer; keeps this crate dependency-free.
    pub fn to_ppm(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + (self.width * self.height * 3) as usize);
        out.extend_from_slice(format!("P6\n{} {}\n255\n", self.width, self.height).as_bytes());
        for px in self.rgba.chunks_exact(4) {
            out.push(px[0]);
            out.push(px[1]);
            out.push(px[2]);
        }
        out
    }

    /// Pixel at (x, y) as `[r, g, b, a]`.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[i],
            self.rgba[i + 1],
            self.rgba[i + 2],
            self.rgba[i + 3],
        ]
    }
}

/// A headless Vulkan device on the physical GPU.
pub struct HostGpu {
    entry: ash::Entry,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    device_name: String,
    driver_name: String,
    driver_id: vk::DriverId,
}

impl HostGpu {
    pub fn device_name(&self) -> &str {
        &self.device_name
    }
    pub fn driver_name(&self) -> &str {
        &self.driver_name
    }
    pub fn driver_id(&self) -> vk::DriverId {
        self.driver_id
    }

    /// Open a headless Vulkan context, preferring the NVIDIA proprietary driver on
    /// a discrete GPU. No window/surface, no validation layers.
    pub fn open() -> R<Self> {
        // SAFETY: loads the system Vulkan loader (libvulkan.so.1).
        let entry = unsafe { ash::Entry::load()? };

        let app = vk::ApplicationInfo::default()
            .application_name(c"infinigpu-replay")
            .application_version(vk::make_api_version(0, 0, 0, 1))
            .api_version(vk::make_api_version(0, 1, 3, 0));
        let ci = vk::InstanceCreateInfo::default().application_info(&app);
        let instance = unsafe { entry.create_instance(&ci, None)? };

        let physicals = unsafe { instance.enumerate_physical_devices()? };
        if physicals.is_empty() {
            return Err("no Vulkan physical devices found".into());
        }

        // Score devices: NVIDIA proprietary wins, then any discrete GPU, then anything.
        let mut best: Option<(i32, vk::PhysicalDevice, String, String, vk::DriverId)> = None;
        for &pd in &physicals {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let mut driver = vk::PhysicalDeviceDriverProperties::default();
            let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
            unsafe { instance.get_physical_device_properties2(pd, &mut props2) };

            let name = cstr_to_string(&props.device_name);
            let driver_name = cstr_to_string(&driver.driver_name);
            let mut score = 0;
            if driver.driver_id == vk::DriverId::NVIDIA_PROPRIETARY {
                score += 100;
            }
            if props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU {
                score += 10;
            }
            if best.as_ref().map(|b| score > b.0).unwrap_or(true) {
                best = Some((score, pd, name, driver_name, driver.driver_id));
            }
        }
        let (_, physical, device_name, driver_name, driver_id) = best.ok_or("no suitable GPU")?;

        // A graphics-capable queue family (graphics implies transfer).
        let qfams = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let queue_family = qfams
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .ok_or("no graphics queue family")? as u32;

        let priorities = [1.0f32];
        let qci = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let qcis = [qci];
        let dci = vk::DeviceCreateInfo::default().queue_create_infos(&qcis);
        let device = unsafe { instance.create_device(physical, &dci, None)? };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical) };

        Ok(HostGpu {
            entry,
            instance,
            physical,
            device,
            queue,
            queue_family,
            mem_props,
            device_name,
            driver_name,
            driver_id,
        })
    }

    fn find_mem(&self, type_bits: u32, flags: vk::MemoryPropertyFlags) -> R<u32> {
        (0..self.mem_props.memory_type_count)
            .find(|&i| {
                type_bits & (1 << i) != 0
                    && self.mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .ok_or_else(|| "no compatible memory type".into())
    }

    /// Render a headless frame: allocate a device-local color image, run a graphics
    /// render pass that clears it to `clear` (RGBA, 0.0–1.0), copy the result into a
    /// host-visible buffer, and read it back. Exercises instance/device/queue/
    /// command-buffer/render-pass/submit/fence/image-to-buffer-copy on real silicon.
    pub fn render_clear(&self, width: u32, height: u32, clear: [f32; 4]) -> R<Frame> {
        const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
        let dev = &self.device;

        // ---- color image (device-local) ----
        let image = unsafe {
            dev.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(FORMAT)
                    .extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(
                        vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                    )
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )?
        };
        let img_req = unsafe { dev.get_image_memory_requirements(image) };
        let img_mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(img_req.size)
                    .memory_type_index(self.find_mem(
                        img_req.memory_type_bits,
                        vk::MemoryPropertyFlags::DEVICE_LOCAL,
                    )?),
                None,
            )?
        };
        unsafe { dev.bind_image_memory(image, img_mem, 0)? };

        let view = unsafe {
            dev.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(FORMAT)
                    .subresource_range(color_range()),
                None,
            )?
        };

        // ---- render pass: clear -> store, end in TRANSFER_SRC ----
        let attach = vk::AttachmentDescription::default()
            .format(FORMAT)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        let attachs = [attach];
        let color_ref = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let subpass = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_ref)];
        let render_pass = unsafe {
            dev.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachs)
                    .subpasses(&subpass),
                None,
            )?
        };

        let views = [view];
        let framebuffer = unsafe {
            dev.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(render_pass)
                    .attachments(&views)
                    .width(width)
                    .height(height)
                    .layers(1),
                None,
            )?
        };

        // ---- host-visible readback buffer ----
        // u64 arithmetic: width*height*4 overflows u32 for large geometries (a debug
        // build would panic; callers also bound the geometry — verify-scheduler #1).
        let size = width as u64 * height as u64 * 4;
        let buffer = unsafe {
            dev.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )?
        };
        let buf_req = unsafe { dev.get_buffer_memory_requirements(buffer) };
        let buf_mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(buf_req.size)
                    .memory_type_index(self.find_mem(
                        buf_req.memory_type_bits,
                        vk::MemoryPropertyFlags::HOST_VISIBLE
                            | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )?),
                None,
            )?
        };
        unsafe { dev.bind_buffer_memory(buffer, buf_mem, 0)? };

        // ---- record ----
        let pool = unsafe {
            dev.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(self.queue_family),
                None,
            )?
        };
        let cmd = unsafe {
            dev.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0]
        };

        unsafe {
            dev.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            let clears = [vk::ClearValue {
                color: vk::ClearColorValue { float32: clear },
            }];
            dev.cmd_begin_render_pass(
                cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(render_pass)
                    .framebuffer(framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: vk::Extent2D { width, height },
                    })
                    .clear_values(&clears),
                vk::SubpassContents::INLINE,
            );
            dev.cmd_end_render_pass(cmd);

            // image is now TRANSFER_SRC_OPTIMAL; copy it into the readback buffer.
            let region = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .mip_level(0)
                        .base_array_layer(0)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                });
            dev.cmd_copy_image_to_buffer(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[region],
            );
            dev.end_command_buffer(cmd)?;
        }

        // ---- submit + fence ----
        let fence = unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None)? };
        let cmds = [cmd];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        unsafe {
            dev.queue_submit(self.queue, &submit, fence)?;
            dev.wait_for_fences(&[fence], true, u64::MAX)?;
        }

        // ---- read back ----
        let rgba = unsafe {
            let ptr = dev.map_memory(buf_mem, 0, size, vk::MemoryMapFlags::empty())? as *const u8;
            let slice = std::slice::from_raw_parts(ptr, size as usize);
            let out = slice.to_vec();
            dev.unmap_memory(buf_mem);
            out
        };

        // ---- teardown of the per-render objects ----
        unsafe {
            dev.destroy_fence(fence, None);
            dev.destroy_command_pool(pool, None);
            dev.destroy_buffer(buffer, None);
            dev.free_memory(buf_mem, None);
            dev.destroy_framebuffer(framebuffer, None);
            dev.destroy_render_pass(render_pass, None);
            dev.destroy_image_view(view, None);
            dev.destroy_image(image, None);
            dev.free_memory(img_mem, None);
        }

        Ok(Frame {
            width,
            height,
            rgba,
        })
    }
}

impl Drop for HostGpu {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
        // keep `entry`/`physical` fields read so they aren't flagged unused
        let _ = (&self.entry, self.physical);
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1)
}

fn cstr_to_string(buf: &[std::os::raw::c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}
