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
use std::ffi::{c_char, CStr};
use std::os::fd::{FromRawFd, OwnedFd};

mod shaders;

pub mod process;

type R<T> = Result<T, Box<dyn Error>>;

/// A rendered frame read back from the GPU into host memory.
#[derive(Debug)]
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

/// A GPU render result exported as an OS file descriptor for **zero-copy** hand-off to
/// another process/consumer (a compositor, an encoder, a peer VM's replay). The fd owns
/// its lifetime — it is closed on drop. Preferred handle type is a Linux **dma-buf**;
/// falls back to an opaque fd if the driver doesn't advertise dma-buf export.
pub struct DmaBufExport {
    fd: OwnedFd,
    size: u64,
    handle_type: &'static str,
}

impl DmaBufExport {
    /// The raw fd (borrowed — the [`DmaBufExport`] retains ownership and closes it).
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }
    /// Size in bytes of the exported allocation.
    pub fn size(&self) -> u64 {
        self.size
    }
    /// `"dma-buf"` or `"opaque-fd"`.
    pub fn handle_type(&self) -> &'static str {
        self.handle_type
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
    /// `VK_KHR_external_memory_fd` loader, present iff the device supports fd export.
    external_fd: Option<ash::khr::external_memory_fd::Device>,
    /// True iff the device also advertises `VK_EXT_external_memory_dma_buf` (→ prefer a
    /// dma-buf handle over an opaque fd when exporting).
    dma_buf_supported: bool,
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

        // External-memory export (dma-buf): enable the fd + dma-buf device extensions
        // only when the device advertises them, so `open()` never fails on a card/driver
        // that lacks them (export just becomes unavailable). `VK_KHR_external_memory` is
        // core in 1.1, so only the fd + dma-buf extensions are requested here.
        let dev_exts = unsafe { instance.enumerate_device_extension_properties(physical)? };
        let has_ext = |name: &CStr| {
            dev_exts
                .iter()
                .any(|e| cstr_from_arr(&e.extension_name) == name)
        };
        let want_fd = has_ext(ash::khr::external_memory_fd::NAME);
        let want_dma_buf = want_fd && has_ext(ash::ext::external_memory_dma_buf::NAME);
        let mut enabled_exts: Vec<*const c_char> = Vec::new();
        if want_fd {
            enabled_exts.push(ash::khr::external_memory_fd::NAME.as_ptr());
        }
        if want_dma_buf {
            enabled_exts.push(ash::ext::external_memory_dma_buf::NAME.as_ptr());
        }

        let priorities = [1.0f32];
        let qci = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let qcis = [qci];
        let dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qcis)
            .enabled_extension_names(&enabled_exts);
        let device = unsafe { instance.create_device(physical, &dci, None)? };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical) };

        let external_fd =
            want_fd.then(|| ash::khr::external_memory_fd::Device::new(&instance, &device));

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
            external_fd,
            dma_buf_supported: want_dma_buf,
        })
    }

    /// Whether this device can export rendered memory as an fd (dma-buf or opaque).
    pub fn can_export(&self) -> bool {
        self.external_fd.is_some()
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

    /// PR5 host-GPU present convert (2D-ADR): upload a guest framebuffer (`src_format`, `pitch`
    /// bytes/row) to the GPU, convert it to tightly-packed **`B8G8R8A8`** via a format-converting
    /// `vkCmdBlitImage` on the A5000, and read it back. This is the host-GPU half of the accelerated
    /// 2D convert path — the layer the device's CPU repack (`present_scanout_damaged`) moves onto the
    /// GPU, and the source of the dma-buf the NVENC ingest (PR7) will consume. Proven on real silicon
    /// by `convert_present_bgra_on_gpu`. Fail-closed on a geometry that overruns `src_bytes`.
    pub fn convert_present(
        &self,
        width: u32,
        height: u32,
        pitch: u32,
        src_format: vk::Format,
        src_bytes: &[u8],
    ) -> R<Frame> {
        const DST: vk::Format = vk::Format::B8G8R8A8_UNORM;
        let dev = &self.device;
        if width == 0 || height == 0 || pitch < width.saturating_mul(4) {
            return Err("convert_present: bad geometry".into());
        }
        if (pitch as u64).saturating_mul(height as u64) > src_bytes.len() as u64 {
            return Err("convert_present: src_bytes too small for geometry".into());
        }

        // ---- staging buffer (host-visible) holding the guest bytes ----
        let staging_size = pitch as u64 * height as u64;
        let staging = unsafe {
            dev.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(staging_size)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )?
        };
        let st_req = unsafe { dev.get_buffer_memory_requirements(staging) };
        let st_mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(st_req.size)
                    .memory_type_index(self.find_mem(
                        st_req.memory_type_bits,
                        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )?),
                None,
            )?
        };
        unsafe {
            dev.bind_buffer_memory(staging, st_mem, 0)?;
            let ptr = dev.map_memory(st_mem, 0, staging_size, vk::MemoryMapFlags::empty())? as *mut u8;
            std::ptr::copy_nonoverlapping(src_bytes.as_ptr(), ptr, staging_size as usize);
            dev.unmap_memory(st_mem);
        }

        // ---- src image (guest format) + dst image (BGRA), both device-local ----
        let mk_image = |format: vk::Format| -> R<(vk::Image, vk::DeviceMemory)> {
            let img = unsafe {
                dev.create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(format)
                        .extent(vk::Extent3D { width, height, depth: 1 })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )?
            };
            let req = unsafe { dev.get_image_memory_requirements(img) };
            let mem = unsafe {
                dev.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(
                            self.find_mem(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?,
                        ),
                    None,
                )?
            };
            unsafe { dev.bind_image_memory(img, mem, 0)? };
            Ok((img, mem))
        };
        let (src_img, src_mem) = mk_image(src_format)?;
        let (dst_img, dst_mem) = mk_image(DST)?;

        // ---- tight readback buffer (host-visible) ----
        let rb_size = width as u64 * height as u64 * 4;
        let rb = unsafe {
            dev.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(rb_size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )?
        };
        let rb_req = unsafe { dev.get_buffer_memory_requirements(rb) };
        let rb_mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(rb_req.size)
                    .memory_type_index(self.find_mem(
                        rb_req.memory_type_bits,
                        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )?),
                None,
            )?
        };
        unsafe { dev.bind_buffer_memory(rb, rb_mem, 0)? };

        // ---- record: staging→src, blit src→dst (format convert), dst→readback ----
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
        let barrier = |img: vk::Image, from: vk::ImageLayout, to: vk::ImageLayout| {
            vk::ImageMemoryBarrier::default()
                .old_layout(from)
                .new_layout(to)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range())
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        };
        unsafe {
            dev.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            // src: UNDEFINED -> TRANSFER_DST, then upload (honoring pitch via buffer_row_length).
            let pre = [
                barrier(src_img, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL),
                barrier(dst_img, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL),
            ];
            dev.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &pre,
            );
            let copy = vk::BufferImageCopy::default()
                .buffer_row_length(pitch / 4) // texels/row (BGRA/RGBA are 4 bytes)
                .buffer_image_height(height)
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D { width, height, depth: 1 });
            dev.cmd_copy_buffer_to_image(
                cmd,
                staging,
                src_img,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[copy],
            );
            // src -> TRANSFER_SRC for the blit.
            let to_src = [barrier(
                src_img,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            )];
            dev.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &to_src,
            );
            // Format-converting blit src(src_format) -> dst(B8G8R8A8): Vulkan swaps channels.
            let sub = |()| {
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1)
            };
            let offsets = [
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D { x: width as i32, y: height as i32, z: 1 },
            ];
            let blit = vk::ImageBlit::default()
                .src_subresource(sub(()))
                .src_offsets(offsets)
                .dst_subresource(sub(()))
                .dst_offsets(offsets);
            dev.cmd_blit_image(
                cmd,
                src_img,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst_img,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[blit],
                vk::Filter::NEAREST,
            );
            // dst -> TRANSFER_SRC, then copy to the tight readback buffer.
            let to_rb = [barrier(
                dst_img,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            )];
            dev.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &to_rb,
            );
            let region = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D { width, height, depth: 1 });
            dev.cmd_copy_image_to_buffer(
                cmd,
                dst_img,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                rb,
                &[region],
            );
            dev.end_command_buffer(cmd)?;
        }

        let fence = unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None)? };
        let cmds = [cmd];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        unsafe {
            dev.queue_submit(self.queue, &submit, fence)?;
            dev.wait_for_fences(&[fence], true, u64::MAX)?;
        }
        let rgba = unsafe {
            let ptr = dev.map_memory(rb_mem, 0, rb_size, vk::MemoryMapFlags::empty())? as *const u8;
            let out = std::slice::from_raw_parts(ptr, rb_size as usize).to_vec();
            dev.unmap_memory(rb_mem);
            out
        };
        unsafe {
            dev.destroy_fence(fence, None);
            dev.destroy_command_pool(pool, None);
            dev.destroy_buffer(rb, None);
            dev.free_memory(rb_mem, None);
            dev.destroy_buffer(staging, None);
            dev.free_memory(st_mem, None);
            dev.destroy_image(dst_img, None);
            dev.free_memory(dst_mem, None);
            dev.destroy_image(src_img, None);
            dev.free_memory(src_mem, None);
        }
        Ok(Frame { width, height, rgba })
    }

    /// Render a **shader-executed** triangle: a graphics pipeline built from our
    /// precompiled SPIR-V (see [`shaders`]) draws 3 hardcoded, per-vertex-coloured
    /// vertices over a cleared `bg`, on the GPU's shader cores — a bare `draw(3,1,0,0)`
    /// with no vertex buffers. Same submit→fence→readback datapath as [`render_clear`],
    /// but proving real SM execution (interpolated gradient), not just a fixed-function
    /// clear. Returns the read-back frame.
    pub fn render_triangle(&self, width: u32, height: u32, bg: [f32; 4]) -> R<Frame> {
        let (frame, _) = self.render_triangle_inner(width, height, bg, false)?;
        Ok(frame)
    }

    /// Render the triangle (as [`render_triangle`](Self::render_triangle)) into a
    /// device-local buffer whose memory is **exported as an fd** for zero-copy hand-off
    /// — a Linux dma-buf where the driver supports it, else an opaque fd. Proves the GPU
    /// output can be shared with another process/consumer without a host round-trip
    /// (ADR-0002/0003: the replay process hands frames to the presenter/encoder). Also
    /// reads the frame back (host-visible copy) so the caller can verify the render.
    /// Errors if the device can't export memory ([`can_export`](Self::can_export)).
    pub fn export_triangle_dmabuf(&self, width: u32, height: u32) -> R<(Frame, DmaBufExport)> {
        let (frame, export) = self.render_triangle_inner(width, height, [0.05, 0.05, 0.08, 1.0], true)?;
        let export = export.ok_or("device does not support external-memory fd export")?;
        Ok((frame, export))
    }

    fn render_triangle_inner(
        &self,
        width: u32,
        height: u32,
        bg: [f32; 4],
        export: bool,
    ) -> R<(Frame, Option<DmaBufExport>)> {
        if export && self.external_fd.is_none() {
            return Err("device does not support external-memory fd export".into());
        }
        const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
        let dev = &self.device;
        let size = width as u64 * height as u64 * 4;

        // ---- color image (device-local) + view ----
        let image = unsafe {
            dev.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(FORMAT)
                    .extent(vk::Extent3D { width, height, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )?
        };
        let img_req = unsafe { dev.get_image_memory_requirements(image) };
        let img_mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(img_req.size)
                    .memory_type_index(
                        self.find_mem(img_req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?,
                    ),
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

        // ---- render pass (clear bg -> store -> TRANSFER_SRC) + framebuffer ----
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
                &vk::RenderPassCreateInfo::default().attachments(&attachs).subpasses(&subpass),
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

        // ---- graphics pipeline (our SPIR-V; no vertex buffers) ----
        let module = unsafe {
            dev.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&shaders::TRIANGLE_SPV),
                None,
            )?
        };
        let layout = unsafe {
            dev.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None)?
        };
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(module)
                .name(c"vs_main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(module)
                .name(c"fs_main"),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_asm = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D { width, height },
        }];
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewports(&viewports)
            .scissors(&scissors);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attach = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(false)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attach);
        let pipeline_ci = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_asm)
            .viewport_state(&viewport_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0);
        let pipeline = unsafe {
            dev.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_ci], None)
                .map_err(|(_, e)| e)?[0]
        };

        // ---- host-visible readback buffer ----
        let readback = unsafe {
            dev.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )?
        };
        let rb_req = unsafe { dev.get_buffer_memory_requirements(readback) };
        let rb_mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(rb_req.size)
                    .memory_type_index(self.find_mem(
                        rb_req.memory_type_bits,
                        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )?),
                None,
            )?
        };
        unsafe { dev.bind_buffer_memory(readback, rb_mem, 0)? };

        // ---- optional device-local, exportable buffer ----
        let (export_buffer, export_mem, export_size, handle_flag, handle_str) = if export {
            let (flag, s) = if self.dma_buf_supported {
                (vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT, "dma-buf")
            } else {
                (vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD, "opaque-fd")
            };
            let mut ext_buf = vk::ExternalMemoryBufferCreateInfo::default().handle_types(flag);
            let ebuf = unsafe {
                dev.create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(size)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .push_next(&mut ext_buf),
                    None,
                )?
            };
            let ereq = unsafe { dev.get_buffer_memory_requirements(ebuf) };
            let mut export_alloc = vk::ExportMemoryAllocateInfo::default().handle_types(flag);
            let emem = unsafe {
                dev.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(ereq.size)
                        .memory_type_index(
                            self.find_mem(ereq.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?,
                        )
                        .push_next(&mut export_alloc),
                    None,
                )?
            };
            unsafe { dev.bind_buffer_memory(ebuf, emem, 0)? };
            (Some(ebuf), Some(emem), ereq.size, flag, s)
        } else {
            (None, None, 0, vk::ExternalMemoryHandleTypeFlags::empty(), "")
        };

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
                color: vk::ClearColorValue { float32: bg },
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
            dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
            dev.cmd_draw(cmd, 3, 1, 0, 0); // 3 vertices, no vertex buffer → SM execution
            dev.cmd_end_render_pass(cmd);

            // image is TRANSFER_SRC_OPTIMAL; copy it into the readback (and export) buffers.
            let region = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .mip_level(0)
                        .base_array_layer(0)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D { width, height, depth: 1 });
            dev.cmd_copy_image_to_buffer(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                readback,
                &[region],
            );
            if let Some(ebuf) = export_buffer {
                dev.cmd_copy_image_to_buffer(
                    cmd,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    ebuf,
                    &[region],
                );
            }
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

        // ---- read back the host-visible copy ----
        let rgba = unsafe {
            let ptr = dev.map_memory(rb_mem, 0, size, vk::MemoryMapFlags::empty())? as *const u8;
            let out = std::slice::from_raw_parts(ptr, size as usize).to_vec();
            dev.unmap_memory(rb_mem);
            out
        };

        // ---- export the device-local buffer's memory as an fd (after the GPU wrote it) ----
        let export = if let Some(emem) = export_mem {
            let raw = unsafe {
                self.external_fd.as_ref().unwrap().get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default().memory(emem).handle_type(handle_flag),
                )?
            };
            // SAFETY: `raw` is a fresh fd owned by us (Vulkan dup'd it); OwnedFd closes it.
            // The exported fd keeps its own reference to the underlying allocation, so
            // freeing the VkDeviceMemory below does not invalidate it (Vulkan spec).
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            Some(DmaBufExport { fd, size: export_size, handle_type: handle_str })
        } else {
            None
        };

        // ---- teardown ----
        unsafe {
            dev.destroy_fence(fence, None);
            dev.destroy_command_pool(pool, None);
            if let (Some(ebuf), Some(emem)) = (export_buffer, export_mem) {
                dev.destroy_buffer(ebuf, None);
                dev.free_memory(emem, None);
            }
            dev.destroy_buffer(readback, None);
            dev.free_memory(rb_mem, None);
            dev.destroy_pipeline(pipeline, None);
            dev.destroy_pipeline_layout(layout, None);
            dev.destroy_shader_module(module, None);
            dev.destroy_framebuffer(framebuffer, None);
            dev.destroy_render_pass(render_pass, None);
            dev.destroy_image_view(view, None);
            dev.destroy_image(image, None);
            dev.free_memory(img_mem, None);
        }

        Ok((Frame { width, height, rgba }, export))
    }
}

impl infinigpu_hal::GpuBackend for HostGpu {
    fn caps(&self) -> infinigpu_hal::GpuCaps {
        use ash::vk::DriverId;
        use infinigpu_hal::Vendor;
        let vendor = match self.driver_id {
            DriverId::NVIDIA_PROPRIETARY => Vendor::Nvidia,
            DriverId::MESA_RADV | DriverId::AMD_PROPRIETARY | DriverId::AMD_OPEN_SOURCE => {
                Vendor::Amd
            }
            DriverId::INTEL_OPEN_SOURCE_MESA | DriverId::INTEL_PROPRIETARY_WINDOWS => Vendor::Intel,
            DriverId::MESA_LLVMPIPE => Vendor::Software,
            _ => Vendor::Other,
        };
        infinigpu_hal::GpuCaps {
            vendor,
            device_name: self.device_name.clone(),
            driver_name: self.driver_name.clone(),
            vulkan_render: true,
            // Vulkan core exposes timestamp queries; external memory (dma-buf) and a
            // global-priority hint are broadly available on the discrete vendors.
            timestamp_queries: true,
            external_memory: true,
            global_priority: matches!(vendor, Vendor::Nvidia | Vendor::Amd | Vendor::Intel),
        }
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
    cstr_from_arr(buf).to_string_lossy().into_owned()
}

fn cstr_from_arr(buf: &[c_char]) -> &CStr {
    // SAFETY: Vulkan guarantees these fixed-size name arrays are NUL-terminated.
    unsafe { CStr::from_ptr(buf.as_ptr()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // GPU-touching; #[ignore]'d so `cargo test` stays green on hosts without a Vulkan device.
    // Run on real silicon with: `cargo test -p infinigpu-replay -- --ignored`.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn convert_present_bgra_on_gpu() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        // A guest scanout delivered as R8G8B8A8 with a padded stride: 4 px wide, 3 rows,
        // pitch = 24 bytes (6 texels) — 2 texels of row padding the convert must skip.
        let (w, h): (u32, u32) = (4, 3);
        let pitch = 24u32; // 6 texels/row
        let mut src = vec![0u8; (pitch * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let o = (y * pitch + x * 4) as usize;
                src[o] = (x * 60 + 5) as u8; // R
                src[o + 1] = (y * 80 + 7) as u8; // G
                src[o + 2] = (x * 20 + y * 3 + 11) as u8; // B
                src[o + 3] = 0xF0; // A
            }
        }
        let frame = gpu
            .convert_present(w, h, pitch, vk::Format::R8G8B8A8_UNORM, &src)
            .expect("convert_present");
        assert_eq!((frame.width, frame.height), (w, h));
        assert_eq!(frame.rgba.len(), (w * h * 4) as usize);
        // The GPU blit R8G8B8A8 -> B8G8R8A8 swaps R and B in memory; output is tightly packed.
        for y in 0..h {
            for x in 0..w {
                let si = (y * pitch + x * 4) as usize;
                let di = ((y * w + x) * 4) as usize;
                assert_eq!(frame.rgba[di], src[si + 2], "B slot @({x},{y})"); // dst.B = src.B
                assert_eq!(frame.rgba[di + 1], src[si + 1], "G slot @({x},{y})"); // dst.G = src.G
                assert_eq!(frame.rgba[di + 2], src[si], "R slot @({x},{y})"); // dst.R = src.R
                assert_eq!(frame.rgba[di + 3], src[si + 3], "A slot @({x},{y})"); // dst.A = src.A
            }
        }
    }

    // A same-format convert (R8G8B8A8 -> ... via BGRA -> compare against a manual swap) also proves
    // the pitch/stride handling: the padded input rows must not leak into the tight output.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn convert_present_strips_row_padding() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let (w, h): (u32, u32) = (2, 2);
        let pitch = 16u32; // 4 texels/row: 2 real + 2 padding
        let mut src = vec![0xAAu8; (pitch * h) as usize]; // padding = 0xAA everywhere
        // real pixels
        let px = [[1u8, 2, 3, 4], [5, 6, 7, 8], [9, 10, 11, 12], [13, 14, 15, 16]];
        for y in 0..h {
            for x in 0..w {
                let o = (y * pitch + x * 4) as usize;
                src[o..o + 4].copy_from_slice(&px[(y * w + x) as usize]);
            }
        }
        let frame = gpu
            .convert_present(w, h, pitch, vk::Format::R8G8B8A8_UNORM, &src)
            .expect("convert_present");
        // No 0xAA padding byte should survive into the tight output.
        for (i, p) in px.iter().enumerate() {
            let di = i * 4;
            assert_eq!(frame.rgba[di], p[2]); // B
            assert_eq!(frame.rgba[di + 1], p[1]); // G
            assert_eq!(frame.rgba[di + 2], p[0]); // R
            assert_eq!(frame.rgba[di + 3], p[3]); // A
        }
    }
}
