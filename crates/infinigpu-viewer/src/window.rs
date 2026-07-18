//! Windowed presentation: a native window (winit — Wayland/Win32, **not** GTK/Qt) with a
//! Vulkan swapchain. Decoded frames are uploaded to a device-local image and
//! `vkCmdBlitImage`'d (linear filter) onto the acquired swapchain image — which both
//! scales the frame to the window and avoids needing a graphics pipeline/shaders. The
//! network+decode work runs on a background thread and hands the newest frame over via a
//! latest-wins [`FrameSlot`]; the render loop presents whatever is current.
//!
//! **Client-side cursor** (see `docs/adr/CLIENT-PLANE-COMPOSITOR.md`): when the guest cursor plane
//! is active, its shape arrives out-of-band via a [`CursorSlot`] and is alpha-blended onto a clean
//! copy of the frame at the **local** pointer position each draw — so the cursor moves at local
//! input latency (zero network round-trip), and the OS cursor is hidden. Still no graphics
//! pipeline: the blend is a CPU pass into the same staging buffer the blit already consumes.
//!
//! This module is compile-validated on a headless box; it needs a Wayland/Win32 display
//! to run. Colour path: openh264 emits RGBA; we upload as BGRA (`B8G8R8A8_UNORM`, the
//! near-universal swapchain format) by swapping R/B, so blit is a same-format scale.

use crate::input;
use crate::stream::{run_stream, CursorShape, CursorSlot, DecodedFrame, FrameSlot};
use ash::vk;
use std::collections::HashSet;
use std::error::Error;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

type R<T> = Result<T, Box<dyn Error>>;

/// Run the windowed client against `url`, blocking until the window closes.
pub fn run(url: &str) -> R<()> {
    let event_loop = EventLoop::new()?;
    // Continuous redraw — a video client always wants the next frame.
    event_loop.set_control_flow(ControlFlow::Poll);
    // Guest-input back-channel: window events → the stream thread → the server WebSocket.
    let (input_tx, input_rx) = channel::<String>();
    let mut app = App {
        url: url.to_string(),
        window: None,
        vk: None,
        slot: FrameSlot::new(),
        cursor_slot: CursorSlot::new(),
        os_cursor_hidden: false,
        net_started: false,
        input_tx,
        input_rx: Some(input_rx),
        pressed_keys: HashSet::new(),
        pressed_buttons: HashSet::new(),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct App {
    url: String,
    window: Option<Arc<Window>>,
    vk: Option<VkViewer>,
    slot: Arc<FrameSlot>,
    /// Client-side cursor hand-off (shape + visibility) from the stream thread.
    cursor_slot: Arc<CursorSlot>,
    /// Whether we've hidden the OS cursor (done once the guest cursor plane is driving ours).
    os_cursor_hidden: bool,
    net_started: bool,
    /// Sends encoded input messages to the stream thread (which forwards them to the server).
    input_tx: Sender<String>,
    /// Moved into the stream thread on first `resumed()`.
    input_rx: Option<Receiver<String>>,
    /// Keys/buttons we've told the guest are DOWN and not yet released. winit delivers no key
    /// events to an unfocused window, so on focus loss we must synthesize releases for these —
    /// otherwise a modifier held across an Alt-Tab sticks in the guest (see `release_all`).
    pressed_keys: HashSet<KeyCode>,
    pressed_buttons: HashSet<MouseButton>,
}

impl App {
    /// Forward one encoded input message to the server (best-effort — a full/closed channel
    /// just drops it, exactly like a dropped video frame).
    fn send_input(&self, msg: Option<String>) {
        if let Some(m) = msg {
            let _ = self.input_tx.send(m);
        }
    }

    /// Release every key and mouse button we believe is held, and forget them. Called on focus
    /// loss: winit stops delivering input to an unfocused window, so a key/button still down
    /// when focus leaves (e.g. Ctrl/Alt during an Alt-Tab out) would never get its release event
    /// — the guest would see the modifier stuck. Synthesizing the ups keeps the guest in sync.
    fn release_all(&mut self) {
        for code in self.pressed_keys.drain().collect::<Vec<_>>() {
            self.send_input(input::key(code, false));
        }
        for button in self.pressed_buttons.drain().collect::<Vec<_>>() {
            self.send_input(input::mouse_button(button, false));
        }
    }
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
        match VkViewer::new(&window, Arc::clone(&self.cursor_slot)) {
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
            let cursor_slot = Arc::clone(&self.cursor_slot);
            let proxy = window.clone();
            let input_rx = self.input_rx.take();
            std::thread::spawn(move || {
                let r = run_stream(&url, input_rx, Some(cursor_slot), |f| {
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
                    // Switch to the client-drawn cursor once the guest cursor plane is driving it
                    // (a shape has arrived): hide the OS cursor. Until then, keep the OS cursor so
                    // there is never a "no cursor at all" window (D8) and no double cursor in
                    // server mode (the guest still bakes its cursor into the video).
                    let want_hidden = self.cursor_slot.has_shape();
                    if want_hidden != self.os_cursor_hidden {
                        win.set_cursor_visible(!want_hidden);
                        self.os_cursor_hidden = want_hidden;
                    }
                    let frame = self.slot.take();
                    if let Err(e) = vk.draw(frame) {
                        log::error!("draw failed: {e}");
                    }
                    win.request_redraw(); // keep presenting (frame or not)
                }
            }
            // ---- guest input: forwarded to the server only while the pointer is in the window
            // (CursorMoved / MouseInput / MouseWheel fire only then) and the window has focus
            // (KeyboardInput). winit scopes them — but that scoping is also the hazard: an
            // unfocused window gets NO events, so a key/button held when focus leaves never gets
            // its release. We track what's down and flush it all on `Focused(false)` below.
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(win) = self.window.as_ref() {
                    let size = win.inner_size();
                    if size.width > 0 && size.height > 0 {
                        let x = position.x / size.width as f64;
                        let y = position.y / size.height as f64;
                        // Draw the client cursor at the LOCAL pointer immediately (zero lag) — the
                        // same window-normalized coords we send the guest, so they stay aligned.
                        if let Some(vk) = self.vk.as_mut() {
                            vk.set_local_cursor(Some((x as f32, y as f32)));
                        }
                        self.send_input(Some(input::mouse_move(x, y)));
                    }
                }
            }
            // Pointer left the window / lost focus: park the client cursor so it isn't drawn at a
            // stale position (and, on focus loss, release held keys/buttons — see release_all).
            WindowEvent::CursorLeft { .. } => {
                if let Some(vk) = self.vk.as_mut() {
                    vk.set_local_cursor(None);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                if pressed {
                    self.pressed_buttons.insert(button);
                } else {
                    self.pressed_buttons.remove(&button);
                }
                self.send_input(input::mouse_button(button, pressed));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };
                self.send_input(input::wheel(dy));
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // Skip auto-repeat: send one down + the release; the guest repeats a held key.
                if !event.repeat {
                    if let PhysicalKey::Code(code) = event.physical_key {
                        let pressed = event.state == ElementState::Pressed;
                        if pressed {
                            self.pressed_keys.insert(code);
                        } else {
                            self.pressed_keys.remove(&code);
                        }
                        self.send_input(input::key(code, pressed));
                    }
                }
            }
            // Focus left the window (Alt-Tab out, click-away). winit will deliver no further
            // releases until focus returns, so release everything now — otherwise a held
            // modifier (Ctrl/Shift/Alt) or mouse button sticks in the guest.
            WindowEvent::Focused(false) => {
                self.release_all();
                if let Some(vk) = self.vk.as_mut() {
                    vk.set_local_cursor(None);
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

    // ---- client-side cursor overlay (composited into the frame on the CPU) ----
    /// Cursor shape/visibility hand-off from the stream thread.
    cursor_slot: Arc<CursorSlot>,
    /// Persistent **clean** (cursor-free) copy of the last decoded frame, BGRA `clean_w*clean_h*4`.
    /// The cursor is blended onto a fresh copy of this into the staging buffer each recomposite, so
    /// the previous cursor position is always erased.
    clean_frame: Vec<u8>,
    clean_w: u32,
    clean_h: u32,
    /// Local pointer position, window-normalized (0..1). `None` when the pointer left the window.
    local_cursor: Option<(f32, f32)>,
    /// Cached sprite + the slot version it came from (re-clone only when the shape changes).
    cached_shape: Option<Arc<CursorShape>>,
    cached_version: u64,
    cursor_visible: bool,
    /// Change key of the last recomposite `(cursor top-left px, version, visible)` — skip the
    /// per-draw copy+blend+upload when nothing changed (idle cost stays ~0).
    last_cursor_key: Option<(i32, i32, u64, bool)>,
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
    fn new(window: &Window, cursor_slot: Arc<CursorSlot>) -> R<Self> {
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
            cursor_slot,
            clean_frame: Vec::new(),
            clean_w: 0,
            clean_h: 0,
            local_cursor: None,
            cached_shape: None,
            cached_version: 0,
            cursor_visible: false,
            last_cursor_key: None,
        };
        me.recreate_swapchain()?;
        Ok(me)
    }

    fn mark_dirty(&mut self, w: u32, h: u32) {
        self.want_extent = vk::Extent2D { width: w.max(1), height: h.max(1) };
        self.dirty = true;
    }

    /// Update the local (window-normalized) pointer position — the zero-lag source for the
    /// client-drawn cursor. `None` parks it (pointer left the window / focus lost).
    fn set_local_cursor(&mut self, pos: Option<(f32, f32)>) {
        self.local_cursor = pos;
    }

    /// Change key for the current cursor draw: `(top-left px, shape version, drawn?)`. When it is
    /// unchanged between draws, the copy+blend+upload is skipped, so a static screen costs ~0.
    fn cursor_key(&self) -> (i32, i32, u64, bool) {
        let (px, py) = match (self.local_cursor, self.cached_shape.as_ref()) {
            (Some((nx, ny)), Some(sh)) => (
                (nx * self.clean_w as f32) as i32 - sh.hot_x as i32,
                (ny * self.clean_h as f32) as i32 - sh.hot_y as i32,
            ),
            _ => (i32::MIN, i32::MIN),
        };
        let drawn = self.cursor_visible && self.local_cursor.is_some() && self.cached_shape.is_some();
        (px, py, self.cached_version, drawn)
    }

    /// Copy the clean (cursor-free) frame into the host-visible staging buffer and alpha-blend the
    /// cursor sprite at the local pointer position on top. Marks the frame for upload. The whole
    /// cursor lift is this CPU blend + the existing blit — no graphics pipeline/shaders. Because the
    /// decoded frame IS the guest framebuffer, the sprite (guest-pixel scale) needs no rescaling;
    /// the stretch-to-fill blit then scales cursor and video together, so they stay aligned.
    fn composite_into_staging(&mut self) {
        let (fw, fh) = (self.clean_w as usize, self.clean_h as usize);
        let need = fw * fh * 4;
        if need == 0 || self.clean_frame.len() < need {
            return;
        }
        let Some(fi) = self.frame.as_mut() else {
            return;
        };
        if fi.width as usize != fw || fi.height as usize != fh {
            return; // frame image not yet resized to match (transient) — skip this draw
        }
        let staging_ptr = fi.staging_ptr;
        fi.pending_upload = true;
        // `fi`'s borrow of self.frame ends here; the raw slice below aliases device memory, not self.
        let dst = unsafe { std::slice::from_raw_parts_mut(staging_ptr, need) };
        dst.copy_from_slice(&self.clean_frame[..need]);

        // Blend the cursor only when we're drawing it (visible + a shape + the pointer is inside).
        let (Some((nx, ny)), Some(shape)) = (self.local_cursor, self.cached_shape.as_ref()) else {
            return;
        };
        if !self.cursor_visible {
            return;
        }
        let px0 = (nx * fw as f32) as i32 - shape.hot_x as i32;
        let py0 = (ny * fh as f32) as i32 - shape.hot_y as i32;
        blend_sprite(
            dst,
            fw,
            fh,
            &shape.bgra,
            shape.w as usize,
            shape.h as usize,
            px0,
            py0,
            shape.premultiplied,
        );
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

        // Present mode: prefer MAILBOX (low-latency triple-buffer, no tearing, uncaps from the
        // display refresh) → IMMEDIATE (lowest latency, may tear) → FIFO (vsync, always present).
        // FIFO alone adds up to a full refresh interval (~16.7ms) of display latency.
        let present_modes = unsafe {
            self.surface_loader
                .get_physical_device_surface_present_modes(self.physical, self.surface)?
        };
        let present_mode = if present_modes.contains(&vk::PresentModeKHR::MAILBOX) {
            vk::PresentModeKHR::MAILBOX
        } else if present_modes.contains(&vk::PresentModeKHR::IMMEDIATE) {
            vk::PresentModeKHR::IMMEDIATE
        } else {
            vk::PresentModeKHR::FIFO
        };

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
            .present_mode(present_mode) // MAILBOX/IMMEDIATE when available, else FIFO vsync
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

        // Ingest a new decoded frame into the persistent CLEAN (cursor-free) buffer, so the
        // cursor can be re-blended at the moving local pointer each draw without smearing.
        let mut need_composite = false;
        if let Some(f) = new_frame {
            self.ensure_frame_image(f.width, f.height)?;
            let px = (f.width * f.height) as usize;
            self.clean_frame.resize(px * 4, 0);
            // openh264 RGBA -> BGRA (swap R/B); alpha forced opaque.
            for (s, d) in f.rgba.chunks_exact(4).zip(self.clean_frame.chunks_exact_mut(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 255;
            }
            self.clean_w = f.width;
            self.clean_h = f.height;
            need_composite = true;
        }

        // Pull the latest cursor state; re-clone the sprite Arc only when its version changes.
        let (_active, visible, shape, version) = self.cursor_slot.snapshot();
        self.cursor_visible = visible;
        if version != self.cached_version {
            self.cached_shape = shape;
            self.cached_version = version;
            need_composite = true;
        }
        // A cursor move / hide / show with no new video frame still needs exactly one recomposite.
        let key = self.cursor_key();
        if self.last_cursor_key != Some(key) {
            self.last_cursor_key = Some(key);
            need_composite = true;
        }
        if need_composite {
            self.composite_into_staging();
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

/// Alpha-blend a BGRA `sprite` (`cw`×`ch`) into a BGRA `dst` frame (`fw`×`fh`) with the sprite's
/// top-left at `(px0,py0)`, clipping to the frame. `premul` selects premultiplied (`out = src +
/// dst*(1-a)`) vs straight (`out = src*a + dst*(1-a)`) alpha. Pure + bounds-safe: any off-screen or
/// partially-clipped placement is skipped per-pixel, never an out-of-range index.
#[allow(clippy::too_many_arguments)]
fn blend_sprite(
    dst: &mut [u8],
    fw: usize,
    fh: usize,
    sprite: &[u8],
    cw: usize,
    ch: usize,
    px0: i32,
    py0: i32,
    premul: bool,
) {
    if sprite.len() < cw * ch * 4 {
        return;
    }
    for sy in 0..ch as i32 {
        let dy = py0 + sy;
        if dy < 0 || dy >= fh as i32 {
            continue;
        }
        for sx in 0..cw as i32 {
            let dx = px0 + sx;
            if dx < 0 || dx >= fw as i32 {
                continue;
            }
            let si = (sy as usize * cw + sx as usize) * 4;
            let s = &sprite[si..si + 4];
            let sa = s[3] as u32;
            if sa == 0 {
                continue; // fully transparent — leave the video pixel untouched
            }
            let di = (dy as usize * fw + dx as usize) * 4;
            for c in 0..3 {
                let sc = s[c] as u32;
                let dc = dst[di + c] as u32;
                dst[di + c] = if premul {
                    (sc + dc * (255 - sa) / 255).min(255) as u8
                } else {
                    ((sc * sa + dc * (255 - sa)) / 255) as u8
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::blend_sprite;

    #[test]
    fn blend_sprite_composites_and_clips_safely() {
        // 4x4 opaque-black BGRA frame.
        let mut frame = vec![0u8; 4 * 4 * 4];
        // 2x2 sprite: top-left opaque white, top-right fully transparent, bottom row 50% white.
        let mut sprite = vec![0u8; 2 * 2 * 4];
        sprite[0..4].copy_from_slice(&[255, 255, 255, 255]); // (0,0) opaque white
        sprite[4..8].copy_from_slice(&[255, 255, 255, 0]); //   (1,0) transparent
        // (0,1) and (1,1): premultiplied 50% white → color already *0.5 = 128, alpha 128.
        sprite[8..12].copy_from_slice(&[128, 128, 128, 128]);
        sprite[12..16].copy_from_slice(&[128, 128, 128, 128]);

        blend_sprite(&mut frame, 4, 4, &sprite, 2, 2, 1, 1, true);

        let px = |x: usize, y: usize| {
            let i = (y * 4 + x) * 4;
            [frame[i], frame[i + 1], frame[i + 2], frame[i + 3]]
        };
        assert_eq!(px(1, 1), [255, 255, 255, 0], "opaque white pasted at (1,1)");
        assert_eq!(px(2, 1), [0, 0, 0, 0], "transparent sprite pixel left the frame black");
        // Premultiplied 50% white over black: out = 128 + 0*(255-128)/255 = 128.
        assert_eq!(px(1, 2), [128, 128, 128, 0], "premultiplied blend");
        // Pixels outside the 2x2 placement are untouched.
        assert_eq!(px(0, 0), [0, 0, 0, 0]);

        // A placement partly off the right/bottom edge must not panic or corrupt out-of-range.
        blend_sprite(&mut frame, 4, 4, &sprite, 2, 2, 3, 3, true);
        // A wholly off-screen placement is a no-op.
        let before = frame.clone();
        blend_sprite(&mut frame, 4, 4, &sprite, 2, 2, -10, -10, true);
        assert_eq!(frame, before);
        // A truncated sprite buffer is rejected (no OOB).
        blend_sprite(&mut frame, 4, 4, &[0u8; 3], 2, 2, 0, 0, true);
    }
}
