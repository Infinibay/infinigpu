//! Windowed presentation: a native window (winit — Wayland/Win32, **not** GTK/Qt) with a
//! Vulkan swapchain. Decoded frames are uploaded to a device-local image and
//! `vkCmdBlitImage`'d (linear filter) onto the acquired swapchain image — which both
//! scales the frame to the window and avoids needing a graphics pipeline/shaders. The
//! network+decode work runs on a background thread and hands the newest frame over via a
//! latest-wins [`FrameSlot`]; the render loop presents whatever is current.
//!
//! This module is compile-validated on a headless box; it needs a Wayland/Win32 display
//! to run. Colour path: openh264 emits RGBA; we upload as BGRA (`B8G8R8A8_UNORM`, the
//! near-universal swapchain format) by swapping R/B, so blit is a same-format scale.

use crate::stream::{run_stream, DecodedFrame, FrameSlot};
use ash::vk;
use std::error::Error;
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

type R<T> = Result<T, Box<dyn Error>>;

/// Run the windowed client against `url`, blocking until the window closes.
pub fn run(url: &str) -> R<()> {
    let event_loop = EventLoop::new()?;
    // Continuous redraw — a video client always wants the next frame.
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App {
        url: url.to_string(),
        window: None,
        vk: None,
        slot: FrameSlot::new(),
        net_started: false,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct App {
    url: String,
    window: Option<Arc<Window>>,
    vk: Option<VkViewer>,
    slot: Arc<FrameSlot>,
    net_started: bool,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("infinigpu-viewer");
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };
        match VkViewer::new(&window) {
            Ok(vk) => self.vk = Some(vk),
            Err(e) => {
                log::error!("Vulkan init failed: {e}");
                event_loop.exit();
                return;
            }
        }
        // Start the network+decode thread now that we have a place to send frames.
        if !self.net_started {
            self.net_started = true;
            let url = self.url.clone();
            let slot = Arc::clone(&self.slot);
            let proxy = window.clone();
            std::thread::spawn(move || {
                let r = run_stream(&url, |f| {
                    slot.put(f);
                    proxy.request_redraw(); // wake the render loop
                    true
                });
                if let Err(e) = r {
                    log::error!("stream thread ended: {e}");
                }
            });
        }
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(vk) = self.vk.as_mut() {
                    vk.mark_dirty(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(vk), Some(win)) = (self.vk.as_mut(), self.window.as_ref()) {
                    let frame = self.slot.take();
                    if let Err(e) = vk.draw(frame) {
                        log::error!("draw failed: {e}");
                    }
                    win.request_redraw(); // keep presenting (frame or not)
                }
            }
            _ => {}
        }
    }
}

// ------------------------------- Vulkan presentation --------------------------------

/// Format used for both the frame image and (preferentially) the swapchain, so the blit
/// is a same-format scale. We upload BGRA to match.
const FMT: vk::Format = vk::Format::B8G8R8A8_UNORM;

struct VkViewer {
    _entry: ash::Entry,
    instance: ash::Instance,
    surface_loader: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    swapchain_loader: ash::khr::swapchain::Device,

    swapchain: vk::SwapchainKHR,
    swap_images: Vec<vk::Image>,
    extent: vk::Extent2D,

    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    img_available: vk::Semaphore,
    render_done: vk::Semaphore,
    in_flight: vk::Fence,

    // Persistent upload target for the decoded frame (recreated on size change).
    frame: Option<FrameImage>,
    want_extent: vk::Extent2D,
    dirty: bool,
}

struct FrameImage {
    width: u32,
    height: u32,
    image: vk::Image,
    memory: vk::DeviceMemory,
    staging: vk::Buffer,
    staging_mem: vk::DeviceMemory,
    staging_ptr: *mut u8,
    initialized: bool,     // has valid contents in TRANSFER_SRC layout
    pending_upload: bool,  // staging holds a new frame to copy into the image
}

impl VkViewer {
    fn new(window: &Window) -> R<Self> {
        use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
        let entry = unsafe { ash::Entry::load()? };

        let display_handle = window.display_handle()?.as_raw();
        let window_handle = window.window_handle()?.as_raw();
        let required = ash_window::enumerate_required_extensions(display_handle)?.to_vec();

        let app = vk::ApplicationInfo::default()
            .application_name(c"infinigpu-viewer")
            .api_version(vk::make_api_version(0, 1, 3, 0));
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app)
                    .enabled_extension_names(&required),
                None,
            )?
        };

        let surface_loader = ash::khr::surface::Instance::new(&entry, &instance);
        let surface = unsafe {
            ash_window::create_surface(&entry, &instance, display_handle, window_handle, None)?
        };

        // Pick a device + queue family that supports graphics AND present to our surface.
        let physicals = unsafe { instance.enumerate_physical_devices()? };
        let mut chosen: Option<(vk::PhysicalDevice, u32, i32)> = None;
        for pd in physicals {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let qfams = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            for (qf, q) in qfams.iter().enumerate() {
                let graphics = q.queue_flags.contains(vk::QueueFlags::GRAPHICS);
                let present = unsafe {
                    surface_loader.get_physical_device_surface_support(pd, qf as u32, surface)?
                };
                if graphics && present {
                    let score = if props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU {
                        10
                    } else {
                        1
                    };
                    if chosen.as_ref().map(|c| score > c.2).unwrap_or(true) {
                        chosen = Some((pd, qf as u32, score));
                    }
                }
            }
        }
        let (physical, queue_family, _) = chosen.ok_or("no graphics+present queue family")?;

        let priorities = [1.0f32];
        let qci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let dev_exts = [ash::khr::swapchain::NAME.as_ptr()];
        let device = unsafe {
            instance.create_device(
                physical,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&qci)
                    .enabled_extension_names(&dev_exts),
                None,
            )?
        };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical) };
        let swapchain_loader = ash::khr::swapchain::Device::new(&instance, &device);

        let pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?
        };
        let cmd = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0]
        };
        let img_available =
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? };
        let render_done =
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? };
        let in_flight = unsafe {
            device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )?
        };

        let size = window.inner_size();
        let mut me = VkViewer {
            _entry: entry,
            instance,
            surface_loader,
            surface,
            physical,
            device,
            queue,
            mem_props,
            swapchain_loader,
            swapchain: vk::SwapchainKHR::null(),
            swap_images: Vec::new(),
            extent: vk::Extent2D { width: 0, height: 0 },
            pool,
            cmd,
            img_available,
            render_done,
            in_flight,
            frame: None,
            want_extent: vk::Extent2D { width: size.width.max(1), height: size.height.max(1) },
            dirty: true,
        };
        me.recreate_swapchain()?;
        Ok(me)
    }

    fn mark_dirty(&mut self, w: u32, h: u32) {
        self.want_extent = vk::Extent2D { width: w.max(1), height: h.max(1) };
        self.dirty = true;
    }

    fn find_mem(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> R<u32> {
        (0..self.mem_props.memory_type_count)
            .find(|&i| {
                bits & (1 << i) != 0
                    && self.mem_props.memory_types[i as usize].property_flags.contains(flags)
            })
            .ok_or_else(|| "no compatible memory type".into())
    }

    fn recreate_swapchain(&mut self) -> R<()> {
        unsafe { self.device.device_wait_idle()? };
        let caps = unsafe {
            self.surface_loader
                .get_physical_device_surface_capabilities(self.physical, self.surface)?
        };
        // current_extent == u32::MAX means "pick your own" (Wayland); else honour it.
        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: self
                    .want_extent
                    .width
                    .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: self
                    .want_extent
                    .height
                    .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };
        if extent.width == 0 || extent.height == 0 {
            return Ok(()); // minimized; skip
        }

        let formats = unsafe {
            self.surface_loader
                .get_physical_device_surface_formats(self.physical, self.surface)?
        };
        let surface_format = formats
            .iter()
            .find(|f| f.format == FMT)
            .copied()
            .unwrap_or(formats[0]);

        let mut image_count = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            image_count = image_count.min(caps.max_image_count);
        }
        let old = self.swapchain;
        let ci = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count(image_count)
            .image_format(surface_format.format)
            .image_color_space(surface_format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            // TRANSFER_DST so we can blit the frame straight onto the swapchain image.
            .image_usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(vk::PresentModeKHR::FIFO) // vsync, always supported
            .clipped(true)
            .old_swapchain(old);
        let swapchain = unsafe { self.swapchain_loader.create_swapchain(&ci, None)? };
        if old != vk::SwapchainKHR::null() {
            unsafe { self.swapchain_loader.destroy_swapchain(old, None) };
        }
        self.swapchain = swapchain;
        self.swap_images = unsafe { self.swapchain_loader.get_swapchain_images(swapchain)? };
        self.extent = extent;
        self.dirty = false;
        Ok(())
    }

    /// Ensure the frame image matches `w`×`h`, (re)creating it on a resolution change.
    fn ensure_frame_image(&mut self, w: u32, h: u32) -> R<()> {
        if let Some(f) = &self.frame {
            if f.width == w && f.height == h {
                return Ok(());
            }
        }
        if let Some(f) = self.frame.take() {
            unsafe { self.destroy_frame_image(f) };
        }
        let dev = &self.device;
        let image = unsafe {
            dev.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(FMT)
                    .extent(vk::Extent3D { width: w, height: h, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )?
        };
        let req = unsafe { dev.get_image_memory_requirements(image) };
        let memory = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(
                        self.find_mem(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?,
                    ),
                None,
            )?
        };
        unsafe { dev.bind_image_memory(image, memory, 0)? };

        let staging_bytes = (w as u64) * (h as u64) * 4;
        let staging = unsafe {
            dev.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(staging_bytes)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )?
        };
        let sreq = unsafe { dev.get_buffer_memory_requirements(staging) };
        let staging_mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(sreq.size)
                    .memory_type_index(self.find_mem(
                        sreq.memory_type_bits,
                        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )?),
                None,
            )?
        };
        unsafe { dev.bind_buffer_memory(staging, staging_mem, 0)? };
        let staging_ptr =
            unsafe { dev.map_memory(staging_mem, 0, sreq.size, vk::MemoryMapFlags::empty())? }
                as *mut u8;

        self.frame = Some(FrameImage {
            width: w,
            height: h,
            image,
            memory,
            staging,
            staging_mem,
            staging_ptr,
            initialized: false,
            pending_upload: false,
        });
        Ok(())
    }

    unsafe fn destroy_frame_image(&self, f: FrameImage) {
        let dev = &self.device;
        dev.unmap_memory(f.staging_mem);
        dev.destroy_buffer(f.staging, None);
        dev.free_memory(f.staging_mem, None);
        dev.destroy_image(f.image, None);
        dev.free_memory(f.memory, None);
    }

    /// Present one frame. If `new_frame` is `Some`, upload it first; otherwise re-present
    /// the last uploaded frame (or clear to black if nothing has arrived yet).
    fn draw(&mut self, new_frame: Option<DecodedFrame>) -> R<()> {
        if self.dirty {
            self.recreate_swapchain()?;
        }
        if self.extent.width == 0 || self.swapchain == vk::SwapchainKHR::null() {
            return Ok(());
        }

        // Upload the decoded frame into the (host-visible) staging buffer as BGRA.
        if let Some(f) = new_frame {
            self.ensure_frame_image(f.width, f.height)?;
            let fi = self.frame.as_mut().unwrap();
            let px = (f.width * f.height) as usize;
            // openh264 RGBA -> BGRA (swap R/B); alpha forced opaque.
            let dst = unsafe { std::slice::from_raw_parts_mut(fi.staging_ptr, px * 4) };
            for (s, d) in f.rgba.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 255;
            }
            fi.pending_upload = true;
        }

        unsafe {
            self.device.wait_for_fences(&[self.in_flight], true, u64::MAX)?;
        }

        let (image_index, suboptimal) = match unsafe {
            self.swapchain_loader.acquire_next_image(
                self.swapchain,
                u64::MAX,
                self.img_available,
                vk::Fence::null(),
            )
        } {
            Ok(v) => v,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.dirty = true;
                return Ok(());
            }
            Err(e) => return Err(Box::new(e)),
        };
        if suboptimal {
            self.dirty = true;
        }
        unsafe { self.device.reset_fences(&[self.in_flight])? };

        let swap_image = self.swap_images[image_index as usize];
        self.record(swap_image)?;

        let wait = [self.img_available];
        let wait_stages = [vk::PipelineStageFlags::TRANSFER];
        let cmds = [self.cmd];
        let signal = [self.render_done];
        let submit = [vk::SubmitInfo::default()
            .wait_semaphores(&wait)
            .wait_dst_stage_mask(&wait_stages)
            .command_buffers(&cmds)
            .signal_semaphores(&signal)];
        unsafe { self.device.queue_submit(self.queue, &submit, self.in_flight)? };

        let swapchains = [self.swapchain];
        let indices = [image_index];
        let present = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal)
            .swapchains(&swapchains)
            .image_indices(&indices);
        match unsafe { self.swapchain_loader.queue_present(self.queue, &present) } {
            Ok(false) => {}
            Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.dirty = true,
            Err(e) => return Err(Box::new(e)),
        }
        Ok(())
    }

    /// Record the per-frame command buffer: (optionally) copy staging→frame image, then
    /// blit the frame image onto the swapchain image and leave it PRESENT_SRC.
    fn record(&mut self, swap_image: vk::Image) -> R<()> {
        let dev = &self.device;
        unsafe {
            dev.reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())?;
            dev.begin_command_buffer(
                self.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            // (a) upload the frame image if a new frame is pending.
            let mut have_frame = false;
            let mut frame_extent = vk::Extent2D { width: 0, height: 0 };
            if let Some(fi) = self.frame.as_mut() {
                frame_extent = vk::Extent2D { width: fi.width, height: fi.height };
                if fi.pending_upload {
                    let old_layout = if fi.initialized {
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL
                    } else {
                        vk::ImageLayout::UNDEFINED
                    };
                    image_barrier(
                        dev,
                        self.cmd,
                        fi.image,
                        old_layout,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    );
                    let region = vk::BufferImageCopy::default()
                        .image_subresource(color_layers())
                        .image_extent(vk::Extent3D {
                            width: fi.width,
                            height: fi.height,
                            depth: 1,
                        });
                    dev.cmd_copy_buffer_to_image(
                        self.cmd,
                        fi.staging,
                        fi.image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region],
                    );
                    image_barrier(
                        dev,
                        self.cmd,
                        fi.image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    );
                    fi.initialized = true;
                    fi.pending_upload = false;
                }
                have_frame = fi.initialized;
            }

            // (b) swapchain image → TRANSFER_DST.
            image_barrier(
                dev,
                self.cmd,
                swap_image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );

            if have_frame {
                let fi = self.frame.as_ref().unwrap();
                let blit = vk::ImageBlit::default()
                    .src_subresource(color_layers())
                    .src_offsets([
                        vk::Offset3D { x: 0, y: 0, z: 0 },
                        vk::Offset3D {
                            x: frame_extent.width as i32,
                            y: frame_extent.height as i32,
                            z: 1,
                        },
                    ])
                    .dst_subresource(color_layers())
                    .dst_offsets([
                        vk::Offset3D { x: 0, y: 0, z: 0 },
                        vk::Offset3D {
                            x: self.extent.width as i32,
                            y: self.extent.height as i32,
                            z: 1,
                        },
                    ]);
                dev.cmd_blit_image(
                    self.cmd,
                    fi.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    swap_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[blit],
                    vk::Filter::LINEAR,
                );
            } else {
                // Nothing decoded yet — clear to a dark background so the window isn't garbage.
                let clear = vk::ClearColorValue { float32: [0.02, 0.02, 0.03, 1.0] };
                dev.cmd_clear_color_image(
                    self.cmd,
                    swap_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &clear,
                    &[color_range()],
                );
            }

            // (c) swapchain image → PRESENT_SRC.
            image_barrier(
                dev,
                self.cmd,
                swap_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::PRESENT_SRC_KHR,
            );
            dev.end_command_buffer(self.cmd)?;
        }
        Ok(())
    }
}

impl Drop for VkViewer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            if let Some(f) = self.frame.take() {
                self.destroy_frame_image(f);
            }
            self.device.destroy_semaphore(self.img_available, None);
            self.device.destroy_semaphore(self.render_done, None);
            self.device.destroy_fence(self.in_flight, None);
            self.device.destroy_command_pool(self.pool, None);
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swapchain_loader.destroy_swapchain(self.swapchain, None);
            }
            self.surface_loader.destroy_surface(self.surface, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
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

fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .mip_level(0)
        .base_array_layer(0)
        .layer_count(1)
}

/// A conservative full-subresource layout transition (image memory barrier). A viewer is
/// not perf-critical, so we use ALL_COMMANDS masks rather than tight per-stage scopes.
fn image_barrier(
    dev: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
) {
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(old)
        .new_layout(new)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(color_range())
        .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);
    unsafe {
        dev.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
    }
}
