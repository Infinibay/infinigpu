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
use std::collections::HashMap;
use std::error::Error;
use std::ffi::{c_char, c_void, CStr};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

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

/// Timing/result of a present-callback render (see [`HostGpu::render_forwarded_present`]).
pub struct PresentStats {
    /// How long the `present` closure ran — the single readback→guest copy, in µs.
    pub present_us: u64,
    /// What `present` returned (e.g. whether the guest scanout was fully mapped).
    pub presented: bool,
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

/// A graphics draw whose shaders + minimal pipeline state are **forwarded from the guest**
/// (the guest ICD serialized a real app's `vkCreateShaderModule`/`vkCreateGraphicsPipelines`
/// into the wire; see `docs/adr/GUEST-ICD-IMPLEMENTATION.md` Phase 1). The host compiles the
/// SPIR-V with the real driver — SPIR-V is vendor-neutral, so no in-guest compile — and
/// replays the draw on the physical GPU. This is the "narrow-faithful" subset: fixed single
/// R8G8B8A8 color target, no vertex buffers/descriptors yet; later phases forward full state.
pub struct ForwardedDraw<'a> {
    /// Vertex-stage SPIR-V (as u32 words) + its entry point.
    pub vertex_spirv: &'a [u32],
    pub vertex_entry: &'a CStr,
    /// Fragment-stage SPIR-V + entry point. May alias `vertex_spirv` for a combined module.
    pub fragment_spirv: &'a [u32],
    pub fragment_entry: &'a CStr,
    /// Vertices for the `draw(vertex_count, 1, 0, 0)` (no vertex buffers — SM-generated). Ignored
    /// when [`geometry`](Self::geometry) is `Some` (the geometry's draw list drives the draws).
    pub vertex_count: u32,
    /// Primitive topology as the wire's `infinigpu_abi::wire::vk_topology` u32 (0 = triangle
    /// list, 1 = triangle strip). Kept as a plain u32 so this public API — and the host device
    /// crate that builds a `ForwardedDraw` from the wire — need not depend on ash/Vulkan headers;
    /// [`map_topology`] converts it just before pipeline creation.
    pub topology: u32,
    /// Phase-2b command list: real mesh geometry (a vertex buffer + optional index buffer + a
    /// vertex-input layout + an ordered draw list). `None` ⇒ the Phase-1 bufferless path (one
    /// `draw(vertex_count)`, empty vertex input) — the built-in triangle and fullscreen passes.
    /// `Some` ⇒ the pipeline gets a non-empty vertex-input state and the record path binds the
    /// buffers and replays each draw. This is the field that lets any real vertex-buffered app render.
    pub geometry: Option<Geometry<'a>>,
}

/// One vertex attribute: what the vertex shader reads at `location`, its format, and its byte
/// offset within a vertex. Formats are the wire's `infinigpu_abi::wire::vk_vformat` u32 (mapped by
/// [`map_vformat`]) so this public API needn't depend on ash — same discipline as [`topology`].
///
/// [`topology`]: ForwardedDraw::topology
#[derive(Clone, Copy)]
pub struct VertexAttr {
    pub location: u32,
    /// Wire `vk_vformat` u32.
    pub format: u32,
    pub offset: u32,
}

/// One draw in a forwarded command list — a `vkCmdDraw`/`vkCmdDrawIndexed` with its own viewport.
#[derive(Clone, Copy)]
pub struct DrawCmd {
    /// `vertex_count` (non-indexed) or `index_count` (indexed, i.e. [`Geometry::index_data`] set).
    pub count: u32,
    /// Instance count; `0` is treated as `1`.
    pub instance_count: u32,
    /// `first_vertex` (non-indexed) or `first_index` (indexed).
    pub first: u32,
    /// Added to every index before the vertex fetch (indexed only). Signed.
    pub vertex_offset: i32,
    /// Viewport `(x, y, w, h)` in pixels; `w == 0.0` ⇒ the full render target.
    pub viewport: [f32; 4],
}

/// Real mesh geometry forwarded from the guest: one interleaved vertex buffer (binding 0) with a
/// `vertex_stride` + `attrs` layout, an optional index buffer, and an ordered list of draws. Borrows
/// the CPU-side bytes; the host uploads them to a GPU buffer just before the draw. This is the
/// "make a real mesh render" payload (Phase-2b) — everything the fixed built-in triangle path lacks.
pub struct Geometry<'a> {
    /// Interleaved vertex bytes (binding 0). Empty is invalid when the draw list is non-empty.
    pub vertex_data: &'a [u8],
    /// Bytes per vertex (binding 0 stride). Must be non-zero when `vertex_data` is non-empty.
    pub vertex_stride: u32,
    /// Vertex-input attribute layout the vertex shader reads.
    pub attrs: &'a [VertexAttr],
    /// Index bytes; empty ⇒ non-indexed draws.
    pub index_data: &'a [u8],
    /// `true` ⇒ 32-bit indices, `false` ⇒ 16-bit.
    pub index_u32: bool,
    /// The draws to replay, in order.
    pub draws: &'a [DrawCmd],
}

/// Wire `vk_topology` u32 → `VkPrimitiveTopology`. Unknown values fall back to a triangle list
/// (fail-safe: an unrecognized topology still draws something rather than erroring).
fn map_topology(t: u32) -> vk::PrimitiveTopology {
    match t {
        1 => vk::PrimitiveTopology::TRIANGLE_STRIP,
        _ => vk::PrimitiveTopology::TRIANGLE_LIST,
    }
}

/// Wire `vk_vformat` u32 → `VkFormat` for a vertex attribute. Unknown values fall back to the widest
/// float (RGBA32F): a fail-safe, since the driver reads at most the declared vertex stride and never
/// past the bound buffer, so a bogus format can't induce an out-of-bounds fetch.
fn map_vformat(f: u32) -> vk::Format {
    use infinigpu_abi::wire::vk_vformat as V;
    match f {
        V::R32_SFLOAT => vk::Format::R32_SFLOAT,
        V::R32G32_SFLOAT => vk::Format::R32G32_SFLOAT,
        V::R32G32B32_SFLOAT => vk::Format::R32G32B32_SFLOAT,
        V::R8G8B8A8_UNORM => vk::Format::R8G8B8A8_UNORM,
        V::R32_UINT => vk::Format::R32_UINT,
        _ => vk::Format::R32G32B32A32_SFLOAT,
    }
}

impl<'a> ForwardedDraw<'a> {
    /// The built-in RGB triangle (the embedded [`shaders::TRIANGLE_SPV`], entries
    /// `vs_main`/`fs_main`, 3 vertices, triangle-list) — used to drive the host executor
    /// through the forwarded path with a known-good workload.
    pub fn builtin_triangle() -> Self {
        ForwardedDraw {
            vertex_spirv: &shaders::TRIANGLE_SPV,
            vertex_entry: c"vs_main",
            fragment_spirv: &shaders::TRIANGLE_SPV,
            fragment_entry: c"fs_main",
            vertex_count: 3,
            topology: 0, // vk_topology::TRIANGLE_LIST
            geometry: None,
        }
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
    /// Fix A: persistent driver pipeline cache passed to `create_graphics_pipelines` (vs. a null
    /// handle) so warm compiles are reused; `null` when caching is disabled.
    pipeline_cache: vk::PipelineCache,
    /// Optional on-disk pipeline-cache blob shared across VM device processes (env
    /// `INFINIGPU_PIPELINE_CACHE_FILE`). Loaded at `open()` to warm-start compiles from other VMs
    /// (attacks the N× redundant-compile cross-VM cost / boot-storm stutter) and merged+saved on
    /// drop. `None` disables the feature (the cache is then process-local, as before).
    pipeline_cache_file: Option<PathBuf>,
    /// Fix A: memoize shader modules + pipelines across submits (env `INFINIGPU_PIPELINE_CACHE`,
    /// default on). Off restores the per-submit compile path for a before/after measurement.
    cache_enabled: bool,
    /// Fix A: the compile-heavy Vulkan objects the 3D submit path would otherwise rebuild every
    /// frame, memoized and reused. Guarded by a `Mutex` (near-uncontended: one submit thread per
    /// device process) so `HostGpu` stays `Sync`.
    obj_cache: Mutex<GpuObjCache>,
    /// Fix B (host): reuse the per-frame alloc-heavy objects (image/memory/view/framebuffer/
    /// readback+persistent map/pool/fence) across submits, keyed by (w,h). Env
    /// `INFINIGPU_SCRATCH_CACHE` (default **on**; `=0` restores the per-frame-alloc path); only
    /// takes effect together with the pipeline cache.
    scratch_enabled: bool,
    scratch_cache: Mutex<HashMap<(u32, u32), SizedScratch>>,
    /// Opt-in per-phase timing of the cached render path (env `INFINIGPU_BREAKDOWN`); `None` in prod.
    breakdown: Option<Breakdown>,
    /// Micro-opt: spin-poll the fence for up to this many µs before falling back to a blocking
    /// `wait_for_fences` (env `INFINIGPU_FENCE_SPIN_US`, default 0 = always block). A short spin
    /// catches the common fast completion without a sleep+wakeup context switch.
    fence_spin_us: u64,
    /// Fix D (zero-copy scanout): `VK_EXT_external_memory_host` loader, present iff the device
    /// supports importing a host pointer as `VkDeviceMemory`. Lets the GPU DMA a rendered frame
    /// **straight into the guest scanout** (memfd-backed guest RAM), skipping the CPU readback copy.
    ext_mem_host: Option<ash::ext::external_memory_host::Device>,
    /// The alignment a host pointer must satisfy to be imported (`minImportedHostPointerAlignment`,
    /// 4096 on NVIDIA); 0 when import is unsupported.
    host_ptr_align: u64,
    /// Fix D: imported guest scanout buffers, keyed by `(host_ptr, size)`. Stable per VM boot (the
    /// scanout address rarely changes), so an import is reused across frames. Invalidated via
    /// [`Self::forget_guest_import`] when the device unmaps that guest RAM (else the import dangles).
    guest_imports: Mutex<HashMap<(usize, u64), ImportedGuest>>,
}

/// Fix D: a guest-RAM region imported as a `TRANSFER_DST` buffer the GPU can copy a frame into.
struct ImportedGuest {
    mem: vk::DeviceMemory,
    buffer: vk::Buffer,
}

impl ImportedGuest {
    /// # Safety
    /// `dev` must be the device the buffer/memory were created on, and no submission may still
    /// reference the buffer.
    unsafe fn destroy(&self, dev: &ash::Device) {
        dev.destroy_buffer(self.buffer, None);
        dev.free_memory(self.mem, None); // does NOT unmap the guest RAM (imported, not owned)
    }
}

/// RAII cleanup for the per-render Vulkan objects of [`HostGpu::render_triangle_inner`]. Every
/// handle is registered here as it is created and destroyed on `Drop` — so an early `?` return
/// from ANY fallible driver call (e.g. a guest-forwarded pipeline that fails to compile) frees the
/// resources instead of leaking them on the long-lived, tenant-shared `VkDevice`. Without this, a
/// guest could flood malformed FORWARDED draws and exhaust VRAM for every co-tenant (found by the
/// Phase-1b adversarial review). All handles start null; `Drop` skips the ones never created, in
/// dependency-safe order (objects before the memory they bind).
struct RenderScratch<'a> {
    dev: &'a ash::Device,
    fence: vk::Fence,
    pool: vk::CommandPool,
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    vs_module: vk::ShaderModule,
    fs_module: vk::ShaderModule,
    framebuffer: vk::Framebuffer,
    render_pass: vk::RenderPass,
    view: vk::ImageView,
    image: vk::Image,
    img_mem: vk::DeviceMemory,
    readback: vk::Buffer,
    rb_mem: vk::DeviceMemory,
    export_buffer: vk::Buffer,
    export_mem: vk::DeviceMemory,
}

impl<'a> RenderScratch<'a> {
    fn new(dev: &'a ash::Device) -> Self {
        RenderScratch {
            dev,
            fence: vk::Fence::null(),
            pool: vk::CommandPool::null(),
            pipeline: vk::Pipeline::null(),
            layout: vk::PipelineLayout::null(),
            vs_module: vk::ShaderModule::null(),
            fs_module: vk::ShaderModule::null(),
            framebuffer: vk::Framebuffer::null(),
            render_pass: vk::RenderPass::null(),
            view: vk::ImageView::null(),
            image: vk::Image::null(),
            img_mem: vk::DeviceMemory::null(),
            readback: vk::Buffer::null(),
            rb_mem: vk::DeviceMemory::null(),
            export_buffer: vk::Buffer::null(),
            export_mem: vk::DeviceMemory::null(),
        }
    }

    /// Null every handle so `Drop` frees nothing — used after ownership of the built objects has
    /// been transferred elsewhere (Fix B `build_sized_scratch`).
    fn disarm(&mut self) {
        self.fence = vk::Fence::null();
        self.pool = vk::CommandPool::null();
        self.pipeline = vk::Pipeline::null();
        self.layout = vk::PipelineLayout::null();
        self.vs_module = vk::ShaderModule::null();
        self.fs_module = vk::ShaderModule::null();
        self.framebuffer = vk::Framebuffer::null();
        self.render_pass = vk::RenderPass::null();
        self.view = vk::ImageView::null();
        self.image = vk::Image::null();
        self.img_mem = vk::DeviceMemory::null();
        self.readback = vk::Buffer::null();
        self.rb_mem = vk::DeviceMemory::null();
        self.export_buffer = vk::Buffer::null();
        self.export_mem = vk::DeviceMemory::null();
    }
}

impl Drop for RenderScratch<'_> {
    fn drop(&mut self) {
        let d = self.dev;
        unsafe {
            if self.fence != vk::Fence::null() {
                d.destroy_fence(self.fence, None);
            }
            if self.pool != vk::CommandPool::null() {
                d.destroy_command_pool(self.pool, None); // also frees its command buffers
            }
            if self.export_buffer != vk::Buffer::null() {
                d.destroy_buffer(self.export_buffer, None);
            }
            if self.export_mem != vk::DeviceMemory::null() {
                d.free_memory(self.export_mem, None);
            }
            if self.readback != vk::Buffer::null() {
                d.destroy_buffer(self.readback, None);
            }
            if self.rb_mem != vk::DeviceMemory::null() {
                d.free_memory(self.rb_mem, None);
            }
            if self.pipeline != vk::Pipeline::null() {
                d.destroy_pipeline(self.pipeline, None);
            }
            if self.layout != vk::PipelineLayout::null() {
                d.destroy_pipeline_layout(self.layout, None);
            }
            if self.vs_module != vk::ShaderModule::null() {
                d.destroy_shader_module(self.vs_module, None);
            }
            if self.fs_module != vk::ShaderModule::null() {
                d.destroy_shader_module(self.fs_module, None);
            }
            if self.framebuffer != vk::Framebuffer::null() {
                d.destroy_framebuffer(self.framebuffer, None);
            }
            if self.render_pass != vk::RenderPass::null() {
                d.destroy_render_pass(self.render_pass, None);
            }
            if self.view != vk::ImageView::null() {
                d.destroy_image_view(self.view, None);
            }
            if self.image != vk::Image::null() {
                d.destroy_image(self.image, None);
            }
            if self.img_mem != vk::DeviceMemory::null() {
                d.free_memory(self.img_mem, None);
            }
        }
    }
}

/// RAII cleanup for the per-call Vulkan objects of [`HostGpu::convert_present_inner`] — the 2D
/// format-converting present path. Same rationale as [`RenderScratch`]: register every handle as
/// it is created so any early `?` frees it instead of leaking on the long-lived, tenant-shared
/// `VkDevice`. The old success-only teardown block leaked all of these on any error path
/// (Phase-1 audit, HIGH). `Drop` skips null handles, in dependency-safe order (objects before
/// the memory they bind).
struct ConvertScratch<'a> {
    dev: &'a ash::Device,
    fence: vk::Fence,
    pool: vk::CommandPool,
    export_buffer: vk::Buffer,
    export_mem: vk::DeviceMemory,
    rb: vk::Buffer,
    rb_mem: vk::DeviceMemory,
    staging: vk::Buffer,
    st_mem: vk::DeviceMemory,
    dst_img: vk::Image,
    dst_mem: vk::DeviceMemory,
    src_img: vk::Image,
    src_mem: vk::DeviceMemory,
}

impl<'a> ConvertScratch<'a> {
    fn new(dev: &'a ash::Device) -> Self {
        ConvertScratch {
            dev,
            fence: vk::Fence::null(),
            pool: vk::CommandPool::null(),
            export_buffer: vk::Buffer::null(),
            export_mem: vk::DeviceMemory::null(),
            rb: vk::Buffer::null(),
            rb_mem: vk::DeviceMemory::null(),
            staging: vk::Buffer::null(),
            st_mem: vk::DeviceMemory::null(),
            dst_img: vk::Image::null(),
            dst_mem: vk::DeviceMemory::null(),
            src_img: vk::Image::null(),
            src_mem: vk::DeviceMemory::null(),
        }
    }
}

impl Drop for ConvertScratch<'_> {
    fn drop(&mut self) {
        let d = self.dev;
        unsafe {
            if self.fence != vk::Fence::null() {
                d.destroy_fence(self.fence, None);
            }
            if self.pool != vk::CommandPool::null() {
                d.destroy_command_pool(self.pool, None);
            }
            if self.export_buffer != vk::Buffer::null() {
                d.destroy_buffer(self.export_buffer, None);
            }
            if self.export_mem != vk::DeviceMemory::null() {
                d.free_memory(self.export_mem, None);
            }
            if self.rb != vk::Buffer::null() {
                d.destroy_buffer(self.rb, None);
            }
            if self.rb_mem != vk::DeviceMemory::null() {
                d.free_memory(self.rb_mem, None);
            }
            if self.staging != vk::Buffer::null() {
                d.destroy_buffer(self.staging, None);
            }
            if self.st_mem != vk::DeviceMemory::null() {
                d.free_memory(self.st_mem, None);
            }
            if self.dst_img != vk::Image::null() {
                d.destroy_image(self.dst_img, None);
            }
            if self.dst_mem != vk::DeviceMemory::null() {
                d.free_memory(self.dst_mem, None);
            }
            if self.src_img != vk::Image::null() {
                d.destroy_image(self.src_img, None);
            }
            if self.src_mem != vk::DeviceMemory::null() {
                d.free_memory(self.src_mem, None);
            }
        }
    }
}

/// Fix A cache bounds — a guest flooding unique SPIR-V can't grow the memo without limit.
const MAX_CACHED_MODULES: usize = 512;
const MAX_CACHED_PIPELINES: usize = 256;

/// Content hash of SPIR-V words — a stable key for the shader/pipeline cache (Fix A). The whole
/// blob is re-hashed on every submit (the ICD forwards the full SPIR-V each time — finding #4), so
/// this is **word-wise** (one round per u32, ~4× fewer than byte-wise) to keep per-submit CPU work
/// proportional to shader size but small. The length is folded into the seed so different-length
/// blobs can't collide on a shared prefix. Not cryptographic: cache dedup only.
fn hash_spirv(data: &[u32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ data.len() as u64;
    for &word in data {
        h = (h ^ word as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Key for a reusable graphics pipeline: the two shader blobs (by hash), the primitive topology,
/// the color format, and the vertex-input layout (Phase-2b — a mesh's stride+attributes change the
/// pipeline). Resolution is deliberately absent — the pipeline uses dynamic viewport+scissor so one
/// entry serves every frame size (Fix A).
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
struct PipelineKey {
    vs_hash: u64,
    fs_hash: u64,
    topology: i32,
    format: i32,
    /// Hash of the vertex-input state (stride + each attribute's location/format/offset); `0` for
    /// the bufferless empty-vertex-input path so the built-in triangle keys exactly as before.
    vinput_hash: u64,
}

/// Content hash of a [`ForwardedDraw`]'s vertex-input layout (binding stride + attributes), folded
/// into [`PipelineKey`] so two meshes with different layouts get different pipelines but the same
/// mesh reuses one. Returns `0` when there is no vertex buffer (empty vertex input) — the Phase-1
/// bufferless key, unchanged.
fn hash_vinput(draw: &ForwardedDraw) -> u64 {
    let Some(g) = draw.geometry.as_ref() else {
        return 0;
    };
    if g.vertex_stride == 0 {
        return 0;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ (g.vertex_stride as u64).rotate_left(1);
    for a in g.attrs {
        for v in [a.location, a.format, a.offset] {
            h = (h ^ v as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

#[derive(Clone, Copy)]
struct CachedPipeline {
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
}

/// SPIR-V-keyed memo of the compile-heavy Vulkan objects the 3D submit path would otherwise
/// rebuild every frame (Fix A). Bounded fail-closed: at the cap the whole cache is torn down and
/// rebuilt on demand (a compile storm only under a hostile flood; in normal use a handful of
/// pipelines recur). Lives for the process (one VM) lifetime.
#[derive(Default)]
struct GpuObjCache {
    shader_modules: HashMap<u64, vk::ShaderModule>,
    render_passes: HashMap<i32, vk::RenderPass>,
    pipelines: HashMap<PipelineKey, CachedPipeline>,
    /// Cumulative pipeline-cache hit/miss counts (Phase-2 instrumentation): the hit rate quantifies
    /// Fix A's effect — in steady state it should approach 100%.
    hits: u64,
    misses: u64,
}

impl GpuObjCache {
    /// Destroy every cached object and empty the maps. Safe between submits: a completed submit
    /// (fence already waited) holds no live reference into the cache.
    ///
    /// # Safety
    /// `dev` must be the device the cached handles were created on, and no command buffer
    /// referencing them may still be executing.
    unsafe fn clear(&mut self, dev: &ash::Device) {
        for (_, p) in self.pipelines.drain() {
            dev.destroy_pipeline(p.pipeline, None);
            dev.destroy_pipeline_layout(p.layout, None);
        }
        for (_, m) in self.shader_modules.drain() {
            dev.destroy_shader_module(m, None);
        }
        for (_, rp) in self.render_passes.drain() {
            dev.destroy_render_pass(rp, None);
        }
    }
}

/// Fix B cache bound — a handful of resolutions in normal use; evict-all past this.
const MAX_CACHED_SCRATCH: usize = 8;

/// Per-(w,h) reusable per-frame objects for the 3D readback path (Fix B). The alloc-heavy pieces —
/// the color image + its device memory, the readback buffer + its host-visible memory (mapped once,
/// `rb_ptr`), the framebuffer, command pool, and fence — persist across submits; only the command
/// buffer is re-recorded each frame. `rb_ptr` is a `usize` so the struct stays `Send+Sync` (HostGpu
/// is shared); the pointee lives as long as `rb_mem`. The render pass is owned by the pipeline cache.
struct SizedScratch {
    image: vk::Image,
    img_mem: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    readback: vk::Buffer,
    rb_mem: vk::DeviceMemory,
    rb_ptr: usize,
    /// Whether `rb_mem` is HOST_COHERENT; if not it is HOST_CACHED and must be invalidated before
    /// each CPU read so the GPU's writes are visible.
    rb_coherent: bool,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    size: u64,
}

/// Phase-2b: the guest's mesh uploaded to GPU-visible buffers for **one** submit. Host-visible +
/// coherent so the forwarded bytes memcpy straight in (no staging buffer/copy — correctness-first;
/// a by-id resource cache that skips re-uploading a static mesh every frame is a perf follow-up, see
/// `docs/3D-COMPLETENESS-ROADMAP.md`). Created per submit and destroyed only after that submit's
/// fence completes — the recorded draws reference these buffers until then.
struct GeometryGpu {
    vbo: vk::Buffer,
    vbo_mem: vk::DeviceMemory,
    /// `(buffer, memory)` — `None` for non-indexed geometry.
    ibo: Option<(vk::Buffer, vk::DeviceMemory)>,
}

impl GeometryGpu {
    /// # Safety
    /// `dev` must be the device the buffers were created on, and no command buffer referencing them
    /// may still be executing (the caller waits the submit fence first).
    unsafe fn destroy(&self, dev: &ash::Device) {
        dev.destroy_buffer(self.vbo, None);
        dev.free_memory(self.vbo_mem, None);
        if let Some((b, m)) = self.ibo {
            dev.destroy_buffer(b, None);
            dev.free_memory(m, None);
        }
    }
}

impl SizedScratch {
    /// # Safety
    /// `dev` must be the owning device and no submit using these objects may still be in flight.
    unsafe fn destroy(self, dev: &ash::Device) {
        dev.unmap_memory(self.rb_mem);
        dev.destroy_fence(self.fence, None);
        dev.destroy_command_pool(self.pool, None); // frees self.cmd too
        dev.destroy_buffer(self.readback, None);
        dev.free_memory(self.rb_mem, None);
        dev.destroy_framebuffer(self.framebuffer, None);
        dev.destroy_image_view(self.view, None);
        dev.destroy_image(self.image, None);
        dev.free_memory(self.img_mem, None);
    }
}

/// Per-phase latency accumulator for the cached render path (env `INFINIGPU_BREAKDOWN`). Splits the
/// remaining hot-path cost after Fix A/B — setup (locks + cache lookups), command recording, the
/// GPU submit+fence-wait (the CPU stall on the GPU), and the readback copy — so the next
/// micro-optimization targets the real bottleneck instead of a guess. Logged every 1000 submits.
#[derive(Default)]
struct Breakdown {
    setup_ns: AtomicU64,
    record_ns: AtomicU64,
    gpu_ns: AtomicU64,
    copy_ns: AtomicU64,
    /// The `present` closure alone (the readback→dst memcpy); `copy_ns - present_ns` ≈ the
    /// per-frame `vkInvalidateMappedMemoryRanges`.
    present_ns: AtomicU64,
    count: AtomicU64,
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
        // Fix D: import a host pointer (the guest scanout) as VkDeviceMemory for zero-copy writeback.
        let want_mem_host = has_ext(ash::ext::external_memory_host::NAME);
        let mut enabled_exts: Vec<*const c_char> = Vec::new();
        if want_fd {
            enabled_exts.push(ash::khr::external_memory_fd::NAME.as_ptr());
        }
        if want_dma_buf {
            enabled_exts.push(ash::ext::external_memory_dma_buf::NAME.as_ptr());
        }
        if want_mem_host {
            enabled_exts.push(ash::ext::external_memory_host::NAME.as_ptr());
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

        // Fix D: build the host-pointer-import loader and read the required alignment.
        let (ext_mem_host, host_ptr_align) = if want_mem_host {
            let mut hp = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut hp);
            unsafe { instance.get_physical_device_properties2(physical, &mut p2) };
            (
                Some(ash::ext::external_memory_host::Device::new(&instance, &device)),
                hp.min_imported_host_pointer_alignment,
            )
        } else {
            (None, 0)
        };

        // Fix A: pipeline/shader caching on by default; INFINIGPU_PIPELINE_CACHE=0/false restores
        // the per-submit compile path so the owner can measure the tail before/after on one binary.
        let cache_enabled = std::env::var("INFINIGPU_PIPELINE_CACHE")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);
        // Optional shared on-disk cache blob: warm-start compiles from other VMs (and this VM's
        // previous boots). The driver validates the blob header and silently ignores a mismatched
        // one, so loading another process's/driver's blob is always safe.
        let pipeline_cache_file = std::env::var_os("INFINIGPU_PIPELINE_CACHE_FILE").map(PathBuf::from);
        let pipeline_cache = if cache_enabled {
            let blob = pipeline_cache_file
                .as_ref()
                .and_then(|p| std::fs::read(p).ok())
                .unwrap_or_default();
            let mut ci = vk::PipelineCacheCreateInfo::default();
            if !blob.is_empty() {
                ci = ci.initial_data(&blob);
                eprintln!(
                    "infinigpu-replay: warm-started pipeline cache from {} bytes",
                    blob.len()
                );
            }
            unsafe {
                device
                    .create_pipeline_cache(&ci, None)
                    .unwrap_or(vk::PipelineCache::null())
            }
        } else {
            vk::PipelineCache::null()
        };

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
            pipeline_cache,
            pipeline_cache_file,
            cache_enabled,
            obj_cache: Mutex::new(GpuObjCache::default()),
            // Fix B (host): default ON (measured −92% single-VM p99, −83% multi-VM worst-p99; render
            // validated identical). `INFINIGPU_SCRATCH_CACHE=0` restores the per-frame-alloc path for
            // an A/B. Only meaningful with the pipeline cache (which owns the render pass the cached
            // framebuffers bind to), so it stays gated behind `cache_enabled`.
            scratch_enabled: cache_enabled
                && std::env::var("INFINIGPU_SCRATCH_CACHE")
                    .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                    .unwrap_or(true),
            scratch_cache: Mutex::new(HashMap::new()),
            breakdown: std::env::var_os("INFINIGPU_BREAKDOWN").map(|_| Breakdown::default()),
            fence_spin_us: std::env::var("INFINIGPU_FENCE_SPIN_US")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            ext_mem_host,
            host_ptr_align,
            guest_imports: Mutex::new(HashMap::new()),
        })
    }

    /// Fix D: whether this device can DMA a rendered frame straight into an imported guest pointer
    /// (i.e. `VK_EXT_external_memory_host` is available). The device server checks this before
    /// attempting the zero-copy path and falls back to the one-copy present otherwise.
    pub fn supports_zerocopy_scanout(&self) -> bool {
        // The device bounds-checks the scanout region rounded to a 4 KiB DMA page before handing us
        // the pointer, and import_guest_buffer rounds the allocation to `host_ptr_align`. Require
        // align ≤ 4096 so the import can never reference a page past what the device validated
        // (NVIDIA reports exactly 4096; a hypothetical larger alignment falls back to the copy path).
        self.ext_mem_host.is_some() && self.host_ptr_align != 0 && self.host_ptr_align <= 4096
    }

    /// Merge this process's compiled pipelines into the shared on-disk cache blob (env
    /// `INFINIGPU_PIPELINE_CACHE_FILE`) and write it back atomically, so a concurrent VM's entries
    /// are never lost and the next VM warm-starts from the union. No-op if the feature is off or the
    /// cache is empty. Called on drop (off the hot path).
    fn save_pipeline_cache(&self) {
        let Some(path) = self.pipeline_cache_file.as_ref() else {
            return;
        };
        if self.pipeline_cache == vk::PipelineCache::null() {
            return;
        }
        let dev = &self.device;
        unsafe {
            // Seed a temp cache from whatever's on disk now, merge ours in (so we never shrink the
            // shared blob under concurrent writers), and dump the union.
            let disk = std::fs::read(path).unwrap_or_default();
            let mut ci = vk::PipelineCacheCreateInfo::default();
            if !disk.is_empty() {
                ci = ci.initial_data(&disk);
            }
            let Ok(merged) = dev.create_pipeline_cache(&ci, None) else {
                return;
            };
            let _ = dev.merge_pipeline_caches(merged, &[self.pipeline_cache]);
            let data = dev.get_pipeline_cache_data(merged).unwrap_or_default();
            dev.destroy_pipeline_cache(merged, None);
            if data.is_empty() {
                return;
            }
            // Atomic replace: write a per-pid temp then rename (POSIX rename is atomic, so a
            // concurrent reader/writer never sees a torn blob; last writer wins, and each write is a
            // superset of the disk blob so nothing is lost).
            let tmp = path.with_extension(format!("tmp{}", std::process::id()));
            if std::fs::write(&tmp, &data).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    /// Wait for `fence`, spin-polling for up to `fence_spin_us` first (micro-opt: skips the
    /// sleep+wakeup context switch when the GPU finishes quickly) before a blocking wait.
    fn wait_fence(&self, fence: vk::Fence) -> R<()> {
        let dev = &self.device;
        if self.fence_spin_us != 0 {
            let start = Instant::now();
            loop {
                match unsafe { dev.get_fence_status(fence) } {
                    Ok(true) => return Ok(()),
                    Ok(false) => {
                        if start.elapsed().as_micros() as u64 >= self.fence_spin_us {
                            break;
                        }
                        std::hint::spin_loop();
                    }
                    Err(_) => break, // fall through to the blocking wait (surfaces the real error)
                }
            }
        }
        unsafe { dev.wait_for_fences(&[fence], true, u64::MAX)? };
        Ok(())
    }

    /// Whether this device can export rendered memory as an fd (dma-buf or opaque).
    pub fn can_export(&self) -> bool {
        self.external_fd.is_some()
    }

    /// Phase-2 instrumentation: cumulative pipeline-cache `(hits, misses, cached_pipeline_count)`.
    /// Hit rate → ~100% in steady state is the direct signal that Fix A is working.
    pub fn cache_stats(&self) -> (u64, u64, usize) {
        let c = self.obj_cache.lock().unwrap_or_else(|e| e.into_inner());
        (c.hits, c.misses, c.pipelines.len())
    }

    /// Fix A: build the color render pass (CLEAR → STORE, ending TRANSFER_SRC) for `format`, with
    /// an explicit external subpass dependency ordering the color write before the post-pass
    /// transfer read (the image→buffer readback) — previously left to implicit sync (audit LOW).
    fn build_render_pass(&self, format: vk::Format) -> R<vk::RenderPass> {
        let attach = vk::AttachmentDescription::default()
            .format(format)
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
        let deps = [vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)];
        let rp = unsafe {
            self.device.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachs)
                    .subpasses(&subpass)
                    .dependencies(&deps),
                None,
            )?
        };
        Ok(rp)
    }

    /// Fix A: build the graphics pipeline (+ its empty layout) from two shader modules, with
    /// **dynamic** viewport+scissor so the pipeline is resolution-independent (set per-frame at
    /// record time). `cache` is the driver pipeline cache (may be null). On pipeline-build failure
    /// the layout is freed so it can't leak.
    fn build_pipeline(
        &self,
        render_pass: vk::RenderPass,
        draw: &ForwardedDraw,
        topology: vk::PrimitiveTopology,
        vs_module: vk::ShaderModule,
        fs_module: vk::ShaderModule,
        cache: vk::PipelineCache,
    ) -> R<(vk::PipelineLayout, vk::Pipeline)> {
        let dev = &self.device;
        let layout =
            unsafe { dev.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None)? };
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vs_module)
                .name(draw.vertex_entry),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fs_module)
                .name(draw.fragment_entry),
        ];
        // Vertex input (Phase-2b): a non-empty binding+attribute layout when the draw carries a
        // vertex buffer, else the empty state (bufferless — the built-in triangle path, unchanged).
        // The `bindings`/`attrs` Vecs must outlive `create_graphics_pipelines`, so they live here.
        let (bindings, attrs): (
            Vec<vk::VertexInputBindingDescription>,
            Vec<vk::VertexInputAttributeDescription>,
        ) = match draw.geometry.as_ref() {
            Some(g) if g.vertex_stride != 0 => (
                vec![vk::VertexInputBindingDescription::default()
                    .binding(0)
                    .stride(g.vertex_stride)
                    .input_rate(vk::VertexInputRate::VERTEX)],
                g.attrs
                    .iter()
                    .map(|a| {
                        vk::VertexInputAttributeDescription::default()
                            .location(a.location)
                            .binding(0)
                            .format(map_vformat(a.format))
                            .offset(a.offset)
                    })
                    .collect(),
            ),
            _ => (Vec::new(), Vec::new()),
        };
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&bindings)
            .vertex_attribute_descriptions(&attrs);
        let input_asm = vk::PipelineInputAssemblyStateCreateInfo::default().topology(topology);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyn_states);
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
            .dynamic_state(&dynamic_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0);
        let pipeline = unsafe { dev.create_graphics_pipelines(cache, &[pipeline_ci], None) };
        match pipeline {
            Ok(p) => Ok((layout, p[0])),
            Err((_, e)) => {
                unsafe { dev.destroy_pipeline_layout(layout, None) };
                Err(e.into())
            }
        }
    }

    /// Fix A: resolve the (render pass, pipeline) for `draw`. When caching is enabled the
    /// compile-heavy objects are memoized on `self` and reused across submits (steady-state
    /// submits skip all compilation); when disabled they are built fresh and registered into `sc`
    /// for per-frame teardown (the pre-Fix-A path, for before/after measurement). In the cached
    /// case the returned handles outlive the submit and must NOT be registered into `sc`.
    fn pipeline_for(
        &self,
        sc: &mut RenderScratch,
        draw: &ForwardedDraw,
        format: vk::Format,
    ) -> R<(vk::RenderPass, vk::Pipeline)> {
        let dev = &self.device;
        let topology = map_topology(draw.topology);

        if !self.cache_enabled {
            let rp = self.build_render_pass(format)?;
            sc.render_pass = rp;
            let vs = unsafe {
                dev.create_shader_module(
                    &vk::ShaderModuleCreateInfo::default().code(draw.vertex_spirv),
                    None,
                )?
            };
            sc.vs_module = vs;
            let fs = unsafe {
                dev.create_shader_module(
                    &vk::ShaderModuleCreateInfo::default().code(draw.fragment_spirv),
                    None,
                )?
            };
            sc.fs_module = fs;
            let (layout, pipe) =
                self.build_pipeline(rp, draw, topology, vs, fs, vk::PipelineCache::null())?;
            sc.layout = layout;
            sc.pipeline = pipe;
            return Ok((rp, pipe));
        }

        let vs_hash = hash_spirv(draw.vertex_spirv);
        let fs_hash = hash_spirv(draw.fragment_spirv);
        let key = PipelineKey {
            vs_hash,
            fs_hash,
            topology: topology.as_raw(),
            format: format.as_raw(),
            vinput_hash: hash_vinput(draw),
        };
        let mut cache = self.obj_cache.lock().unwrap_or_else(|e| e.into_inner());
        // Fail-closed bound before inserting anything new.
        if cache.pipelines.len() >= MAX_CACHED_PIPELINES
            || cache.shader_modules.len() >= MAX_CACHED_MODULES
        {
            eprintln!(
                "infinigpu-replay: pipeline cache at cap — evicting all (a guest may be flooding unique shaders)"
            );
            unsafe { cache.clear(dev) };
        }
        let fmt_raw = format.as_raw();
        let rp = match cache.render_passes.get(&fmt_raw) {
            Some(&rp) => rp,
            None => {
                let rp = self.build_render_pass(format)?;
                cache.render_passes.insert(fmt_raw, rp);
                rp
            }
        };
        if let Some(cp) = cache.pipelines.get(&key).copied() {
            cache.hits += 1;
            return Ok((rp, cp.pipeline));
        }
        // Resolve shader modules, tracking which we create fresh so a failed pipeline build (e.g. a
        // hostile bad entry point) caches nothing and frees exactly the modules we just made.
        let (vs, vs_new) = match cache.shader_modules.get(&vs_hash) {
            Some(&m) => (m, false),
            None => (
                unsafe {
                    dev.create_shader_module(
                        &vk::ShaderModuleCreateInfo::default().code(draw.vertex_spirv),
                        None,
                    )?
                },
                true,
            ),
        };
        let (fs, fs_new) = if fs_hash == vs_hash {
            (vs, false) // identical blob → one module serves both stages
        } else {
            match cache.shader_modules.get(&fs_hash) {
                Some(&m) => (m, false),
                None => match unsafe {
                    dev.create_shader_module(
                        &vk::ShaderModuleCreateInfo::default().code(draw.fragment_spirv),
                        None,
                    )
                } {
                    Ok(m) => (m, true),
                    Err(e) => {
                        if vs_new {
                            unsafe { dev.destroy_shader_module(vs, None) };
                        }
                        return Err(e.into());
                    }
                },
            }
        };
        match self.build_pipeline(rp, draw, topology, vs, fs, self.pipeline_cache) {
            Ok((layout, pipe)) => {
                if vs_new {
                    cache.shader_modules.insert(vs_hash, vs);
                }
                if fs_new {
                    cache.shader_modules.insert(fs_hash, fs);
                }
                cache.misses += 1;
                cache.pipelines.insert(key, CachedPipeline { pipeline: pipe, layout });
                Ok((rp, pipe))
            }
            Err(e) => {
                // Cache nothing; free the modules we just created (cached ones stay). When both are
                // fresh they are distinct objects (fs_hash != vs_hash), so there is no double-free.
                unsafe {
                    if vs_new {
                        dev.destroy_shader_module(vs, None);
                    }
                    if fs_new {
                        dev.destroy_shader_module(fs, None);
                    }
                }
                Err(e)
            }
        }
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

    /// Pick a host-visible memory type for CPU readback, preferring **HOST_CACHED** — cached reads
    /// are ~10× faster than the default write-combined/uncached host-visible mapping, and the
    /// readback copy is the dominant remaining hot-path cost (breakdown: ~72% of a small frame).
    /// Prefers co-available HOST_COHERENT (no manual invalidate). Returns (type_index, coherent).
    fn find_readback_mem(&self, type_bits: u32) -> R<(u32, bool)> {
        use vk::MemoryPropertyFlags as F;
        if let Ok(i) = self.find_mem(type_bits, F::HOST_VISIBLE | F::HOST_CACHED | F::HOST_COHERENT)
        {
            return Ok((i, true));
        }
        if let Ok(i) = self.find_mem(type_bits, F::HOST_VISIBLE | F::HOST_CACHED) {
            return Ok((i, false)); // cached but not coherent → invalidate before reading
        }
        let i = self.find_mem(type_bits, F::HOST_VISIBLE | F::HOST_COHERENT)?;
        Ok((i, true))
    }

    /// Render a headless frame: allocate a device-local color image, run a graphics
    /// render pass that clears it to `clear` (RGBA, 0.0–1.0), copy the result into a
    /// host-visible buffer, and read it back. Exercises instance/device/queue/
    /// command-buffer/render-pass/submit/fence/image-to-buffer-copy on real silicon.
    pub fn render_clear(&self, width: u32, height: u32, clear: [f32; 4]) -> R<Frame> {
        const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
        let dev = &self.device;
        // RAII cleanup: register each handle into `sc` as it is created so ANY early `?`
        // frees everything instead of leaking it on the long-lived, tenant-shared VkDevice.
        // (Same guard render_triangle_inner uses; the old success-only teardown block below
        // leaked every object on every error path — Phase-1 audit finding, HIGH.)
        let mut sc = RenderScratch::new(dev);

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
        sc.image = image;
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
        sc.img_mem = img_mem;
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
        sc.view = view;

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
        sc.render_pass = render_pass;

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
        sc.framebuffer = framebuffer;

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
        sc.readback = buffer;
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
        sc.rb_mem = buf_mem;
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
        sc.pool = pool;

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
        sc.fence = fence;
        let cmds = [cmd];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        unsafe {
            dev.queue_submit(self.queue, &submit, fence)?;
        }
        self.wait_fence(fence)?;

        // ---- read back ----
        let rgba = unsafe {
            let ptr = dev.map_memory(buf_mem, 0, size, vk::MemoryMapFlags::empty())? as *const u8;
            let slice = std::slice::from_raw_parts(ptr, size as usize);
            let out = slice.to_vec();
            dev.unmap_memory(buf_mem);
            out
        };

        // Per-render objects are freed by `sc` on drop — including on every early-`?` path
        // above (the previous success-only teardown block leaked them on any error).
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
    /// GPU. Proven on real silicon by `convert_present_bgra_on_gpu`. Fail-closed on a geometry that
    /// overruns `src_bytes`. For the zero-copy dma-buf variant (PR7 NVENC ingest) see
    /// [`convert_present_export`](Self::convert_present_export).
    pub fn convert_present(
        &self,
        width: u32,
        height: u32,
        pitch: u32,
        src_format: vk::Format,
        src_bytes: &[u8],
    ) -> R<Frame> {
        Ok(self
            .convert_present_inner(width, height, pitch, src_format, src_bytes, false)?
            .0)
    }

    /// PR7 enabler: like [`convert_present`](Self::convert_present) but the converted `B8G8R8A8`
    /// result is *also* copied into a device-local buffer whose memory is **exported as a dma-buf
    /// fd** — a zero-copy hand-off to the NVENC ingest (no host round-trip needed for the encoder).
    /// The returned [`Frame`] is the host-readback copy (for verification/tests); the encoder path
    /// consumes the [`DmaBufExport`]. Errors if the device can't export memory
    /// ([`can_export`](Self::can_export)). Proven on real silicon by `convert_present_export_dmabuf`.
    pub fn convert_present_export(
        &self,
        width: u32,
        height: u32,
        pitch: u32,
        src_format: vk::Format,
        src_bytes: &[u8],
    ) -> R<(Frame, DmaBufExport)> {
        let (frame, export) =
            self.convert_present_inner(width, height, pitch, src_format, src_bytes, true)?;
        let export = export.ok_or("device does not support external-memory fd export")?;
        Ok((frame, export))
    }

    fn convert_present_inner(
        &self,
        width: u32,
        height: u32,
        pitch: u32,
        src_format: vk::Format,
        src_bytes: &[u8],
        export: bool,
    ) -> R<(Frame, Option<DmaBufExport>)> {
        const DST: vk::Format = vk::Format::B8G8R8A8_UNORM;
        let dev = &self.device;
        if width == 0 || height == 0 || pitch < width.saturating_mul(4) {
            return Err("convert_present: bad geometry".into());
        }
        if (pitch as u64).saturating_mul(height as u64) > src_bytes.len() as u64 {
            return Err("convert_present: src_bytes too small for geometry".into());
        }
        if export && self.external_fd.is_none() {
            return Err("device does not support external-memory fd export".into());
        }
        // RAII cleanup for every per-call object (leak fix — see ConvertScratch).
        let mut sc = ConvertScratch::new(dev);

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
        sc.staging = staging;
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
        sc.st_mem = st_mem;
        unsafe {
            dev.bind_buffer_memory(staging, st_mem, 0)?;
            let ptr = dev.map_memory(st_mem, 0, staging_size, vk::MemoryMapFlags::empty())? as *mut u8;
            std::ptr::copy_nonoverlapping(src_bytes.as_ptr(), ptr, staging_size as usize);
            dev.unmap_memory(st_mem);
        }

        // ---- src image (guest format) + dst image (BGRA), both device-local ----
        // Register into `sc` as created so an alloc/bind failure on the second image (or any
        // later `?`) frees the first instead of leaking it (Phase-1 audit leak fix).
        let mk_image = |sc: &mut ConvertScratch, format: vk::Format, dst: bool| -> R<()> {
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
            if dst { sc.dst_img = img } else { sc.src_img = img }
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
            if dst { sc.dst_mem = mem } else { sc.src_mem = mem }
            unsafe { dev.bind_image_memory(img, mem, 0)? };
            Ok(())
        };
        mk_image(&mut sc, src_format, false)?;
        mk_image(&mut sc, DST, true)?;
        let (src_img, dst_img) = (sc.src_img, sc.dst_img);

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
        sc.rb = rb;
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
        sc.rb_mem = rb_mem;
        unsafe { dev.bind_buffer_memory(rb, rb_mem, 0)? };

        // ---- optional device-local, exportable buffer (dma-buf → NVENC, PR7) ----
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
                        .size(rb_size)
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
            sc.export_buffer = ebuf;
            sc.export_mem = emem;
            (Some(ebuf), Some(emem), ereq.size, flag, s)
        } else {
            (None, None, 0, vk::ExternalMemoryHandleTypeFlags::empty(), "")
        };

        // ---- record: staging→src, blit src→dst (format convert), dst→readback ----
        let pool = unsafe {
            dev.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(self.queue_family),
                None,
            )?
        };
        sc.pool = pool;
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
            if let Some(ebuf) = export_buffer {
                dev.cmd_copy_image_to_buffer(
                    cmd,
                    dst_img,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    ebuf,
                    &[region],
                );
            }
            dev.end_command_buffer(cmd)?;
        }

        let fence = unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None)? };
        sc.fence = fence;
        let cmds = [cmd];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        unsafe {
            dev.queue_submit(self.queue, &submit, fence)?;
        }
        self.wait_fence(fence)?;
        let rgba = unsafe {
            let ptr = dev.map_memory(rb_mem, 0, rb_size, vk::MemoryMapFlags::empty())? as *const u8;
            let out = std::slice::from_raw_parts(ptr, rb_size as usize).to_vec();
            dev.unmap_memory(rb_mem);
            out
        };
        // Export the device-local copy's memory as an fd (after the GPU wrote it).
        let export = if let Some(emem) = export_mem {
            let raw = unsafe {
                self.external_fd.as_ref().unwrap().get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default().memory(emem).handle_type(handle_flag),
                )?
            };
            // SAFETY: fresh fd owned by us; OwnedFd closes it. The exported fd keeps its own
            // reference to the allocation, so freeing the VkDeviceMemory below is safe.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            Some(DmaBufExport { fd, size: export_size, handle_type: handle_str })
        } else {
            None
        };
        // Per-call objects are freed by `sc` on drop — on every early-`?` path too. The exported
        // fd (if any) holds its own reference to the allocation, so freeing export_mem on drop
        // after the export is safe (Vulkan spec).
        Ok((Frame { width, height, rgba }, export))
    }

    /// Render a **shader-executed** triangle: a graphics pipeline built from our
    /// precompiled SPIR-V (see [`shaders`]) draws 3 hardcoded, per-vertex-coloured
    /// vertices over a cleared `bg`, on the GPU's shader cores — a bare `draw(3,1,0,0)`
    /// with no vertex buffers. Same submit→fence→readback datapath as [`render_clear`],
    /// but proving real SM execution (interpolated gradient), not just a fixed-function
    /// clear. Returns the read-back frame.
    pub fn render_triangle(&self, width: u32, height: u32, bg: [f32; 4]) -> R<Frame> {
        let (frame, _) =
            self.render_triangle_inner(width, height, bg, &ForwardedDraw::builtin_triangle(), false)?;
        Ok(frame)
    }

    /// Replay a **forwarded** guest draw ([`ForwardedDraw`]) on the physical GPU: compile the
    /// forwarded SPIR-V with the real driver, build the pipeline, clear to `bg`, draw, and read
    /// the `R8G8B8A8` result back. This is the host half of the Phase-1 own-ICD 3D path — the
    /// guest ICD serializes a real app's shaders + draw into the wire and the host executes them
    /// here (no fixed workload). Proven on real silicon by `render_forwarded_matches_builtin`.
    pub fn render_forwarded(
        &self,
        width: u32,
        height: u32,
        bg: [f32; 4],
        draw: &ForwardedDraw,
    ) -> R<Frame> {
        // Fix B (opt-in): reuse per-(w,h) alloc-heavy objects across submits. Isolated path — the
        // default below is untouched, so with the flag off behavior is exactly as before.
        if self.scratch_enabled {
            return self.render_forwarded_cached(width, height, bg, draw);
        }
        let (frame, _) = self.render_triangle_inner(width, height, bg, draw, false)?;
        Ok(frame)
    }

    /// Like [`Self::render_forwarded`] but hands the finished RGBA straight to `present` (called
    /// once, while the readback memory is mapped) instead of returning an owned [`Frame`]. Lets the
    /// device copy the pixels **directly into the guest scanout** — one CPU copy instead of two,
    /// and no per-frame heap allocation (finding #5). On the Fix-B cached path this is the fast
    /// route; with the scratch cache off it falls back to a render-then-present (unchanged 2 copies).
    /// `present` returns success (e.g. whether the guest scanout was fully mapped).
    pub fn render_forwarded_present<F: FnOnce(&[u8]) -> bool>(
        &self,
        width: u32,
        height: u32,
        bg: [f32; 4],
        draw: &ForwardedDraw,
        present: F,
    ) -> R<PresentStats> {
        if self.scratch_enabled {
            return self.render_forwarded_cached_present(width, height, bg, draw, present);
        }
        let (frame, _) = self.render_triangle_inner(width, height, bg, draw, false)?;
        let tp = Instant::now();
        let presented = present(&frame.rgba);
        Ok(PresentStats {
            present_us: tp.elapsed().as_micros() as u64,
            presented,
        })
    }

    /// Fix B: allocation-free forwarded render — reuses the persistent [`SizedScratch`] for
    /// `(width,height)` (built once, then only the command buffer is re-recorded). Requires the
    /// pipeline cache (guaranteed: `scratch_enabled` implies `cache_enabled`), so `pipeline_for`
    /// takes the cached path and never registers into the throwaway guard.
    fn render_forwarded_cached_present<F: FnOnce(&[u8]) -> bool>(
        &self,
        width: u32,
        height: u32,
        bg: [f32; 4],
        draw: &ForwardedDraw,
        present: F,
    ) -> R<PresentStats> {
        const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
        let dev = &self.device;
        let bd = self.breakdown.as_ref();
        let t0 = bd.map(|_| Instant::now());
        let mut throwaway = RenderScratch::new(dev);
        let (render_pass, pipeline) = self.pipeline_for(&mut throwaway, draw, FORMAT)?;

        let mut cache = self.scratch_cache.lock().unwrap_or_else(|e| e.into_inner());
        if !cache.contains_key(&(width, height)) {
            if cache.len() >= MAX_CACHED_SCRATCH {
                for (_, ss) in cache.drain() {
                    unsafe { ss.destroy(dev) };
                }
            }
            let ss = self.build_sized_scratch(width, height, render_pass)?;
            cache.insert((width, height), ss);
        }
        let ss = cache.get(&(width, height)).expect("just inserted above");
        let t_setup = bd.map(|_| Instant::now());

        // Phase-2b: upload the mesh (if any) for this submit. Freed after the fence completes below —
        // the recorded draws reference the buffers until then. A failure here is a clean early return
        // (the scratch is untouched, upload_geometry frees its own partials).
        let geom_gpu = match draw.geometry.as_ref() {
            Some(g) if g.vertex_stride != 0 && !g.vertex_data.is_empty() => {
                Some(self.upload_geometry(g)?)
            }
            _ => None,
        };

        // Single submit thread per device process + fence-wait before return ⇒ no frame N+1 vs. N
        // hazard on the reused objects; reset the pool + fence and re-record for this frame.
        self.record_forwarded_frame(
            ss,
            render_pass,
            pipeline,
            width,
            height,
            bg,
            draw,
            geom_gpu.as_ref(),
            ss.readback,
        )?;
        let t_record = bd.map(|_| Instant::now());
        unsafe {
            dev.reset_fences(&[ss.fence])?;
            let cmds = [ss.cmd];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
            dev.queue_submit(self.queue, &submit, ss.fence)?;
        }
        self.wait_fence(ss.fence)?;
        // The submit fence is signaled ⇒ the draws no longer reference the mesh buffers; free them.
        if let Some(gg) = geom_gpu {
            unsafe { gg.destroy(dev) };
        }
        let t_gpu = bd.map(|_| Instant::now());
        // HOST_CACHED readback: invalidate so the GPU's writes are visible before the (cached, fast)
        // read — reading write-combined/uncached memory was ~72% of the hot path. Coherent memory
        // needs no invalidate. Reads straight from the persistent mapping (no per-frame map/unmap).
        if !ss.rb_coherent {
            unsafe {
                dev.invalidate_mapped_memory_ranges(&[vk::MappedMemoryRange::default()
                    .memory(ss.rb_mem)
                    .offset(0)
                    .size(vk::WHOLE_SIZE)])?;
            }
        }
        // One copy, straight from the persistent readback mapping into the caller's target (the
        // guest scanout) — no intermediate Vec (finding #5: kill the second CPU copy + the
        // per-frame 256 KB heap alloc). Sound because the mapping lives as long as `ss`, and the
        // cache mutex is still held here.
        let slice =
            unsafe { std::slice::from_raw_parts(ss.rb_ptr as *const u8, ss.size as usize) };
        let tp = Instant::now();
        let presented = present(slice);
        let present_dur = tp.elapsed();
        let present_us = present_dur.as_micros() as u64;
        if let Some(bd) = bd {
            let t_copy = Instant::now();
            let d = |a: Option<Instant>, b: Option<Instant>| {
                a.zip(b).map(|(x, y)| (y - x).as_nanos() as u64).unwrap_or(0)
            };
            bd.setup_ns.fetch_add(d(t0, t_setup), Ordering::Relaxed);
            bd.record_ns.fetch_add(d(t_setup, t_record), Ordering::Relaxed);
            bd.gpu_ns.fetch_add(d(t_record, t_gpu), Ordering::Relaxed);
            bd.copy_ns.fetch_add(d(t_gpu, Some(t_copy)), Ordering::Relaxed);
            bd.present_ns
                .fetch_add(present_dur.as_nanos() as u64, Ordering::Relaxed);
            let n = bd.count.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(1000) {
                eprintln!(
                    "breakdown (avg/{n}): setup={}ns record={}ns gpu(submit+wait)={}ns copy={}ns (memcpy={}ns invalidate={}ns)",
                    bd.setup_ns.load(Ordering::Relaxed) / n,
                    bd.record_ns.load(Ordering::Relaxed) / n,
                    bd.gpu_ns.load(Ordering::Relaxed) / n,
                    bd.copy_ns.load(Ordering::Relaxed) / n,
                    bd.present_ns.load(Ordering::Relaxed) / n,
                    (bd.copy_ns.load(Ordering::Relaxed).saturating_sub(bd.present_ns.load(Ordering::Relaxed))) / n,
                );
            }
        }
        Ok(PresentStats { present_us, presented })
    }

    /// Vec-returning wrapper over [`Self::render_forwarded_cached_present`] — adds the copy back
    /// into an owned [`Frame`] for callers/tests that want the pixels (the bench, PPM dumps).
    fn render_forwarded_cached(
        &self,
        width: u32,
        height: u32,
        bg: [f32; 4],
        draw: &ForwardedDraw,
    ) -> R<Frame> {
        let mut rgba = Vec::new();
        self.render_forwarded_cached_present(width, height, bg, draw, |px| {
            rgba = px.to_vec();
            true
        })?;
        Ok(Frame { width, height, rgba })
    }

    /// Phase-2b: upload `g`'s vertex (and optional index) bytes into fresh host-visible+coherent GPU
    /// buffers for one submit. The forwarded bytes are memcpy'd straight in (coherent ⇒ no flush).
    /// On any failure every partial allocation is freed before returning (no leak on the shared
    /// device). The returned [`GeometryGpu`] must be destroyed after the submit fence completes.
    fn upload_geometry(&self, g: &Geometry) -> R<GeometryGpu> {
        let dev = &self.device;
        // Create a host-visible|coherent buffer sized to `data`, memcpy `data` in, return it.
        let mk = |data: &[u8], usage: vk::BufferUsageFlags| -> R<(vk::Buffer, vk::DeviceMemory)> {
            if data.is_empty() {
                return Err("empty geometry buffer".into());
            }
            unsafe {
                let buf = dev.create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(data.len() as u64)
                        .usage(usage)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )?;
                // From here on, free `buf` (and later `mem`) on any early return.
                let cleanup_buf = |e: Box<dyn std::error::Error>| {
                    dev.destroy_buffer(buf, None);
                    e
                };
                let req = dev.get_buffer_memory_requirements(buf);
                let mt = self
                    .find_mem(
                        req.memory_type_bits,
                        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )
                    .map_err(cleanup_buf)?;
                let mem = dev
                    .allocate_memory(
                        &vk::MemoryAllocateInfo::default()
                            .allocation_size(req.size)
                            .memory_type_index(mt),
                        None,
                    )
                    .map_err(|e| cleanup_buf(e.into()))?;
                let cleanup_both = |e: Box<dyn std::error::Error>| {
                    dev.free_memory(mem, None);
                    dev.destroy_buffer(buf, None);
                    e
                };
                dev.bind_buffer_memory(buf, mem, 0)
                    .map_err(|e| cleanup_both(e.into()))?;
                let ptr = dev
                    .map_memory(mem, 0, data.len() as u64, vk::MemoryMapFlags::empty())
                    .map_err(|e| cleanup_both(e.into()))?;
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
                dev.unmap_memory(mem); // coherent → the write is visible to the GPU without a flush
                Ok((buf, mem))
            }
        };
        let (vbo, vbo_mem) = mk(g.vertex_data, vk::BufferUsageFlags::VERTEX_BUFFER)?;
        let ibo = if g.index_data.is_empty() {
            None
        } else {
            match mk(g.index_data, vk::BufferUsageFlags::INDEX_BUFFER) {
                Ok(x) => Some(x),
                Err(e) => {
                    unsafe {
                        dev.destroy_buffer(vbo, None);
                        dev.free_memory(vbo_mem, None);
                    }
                    return Err(e);
                }
            }
        };
        Ok(GeometryGpu { vbo, vbo_mem, ibo })
    }

    /// Record one forwarded-draw frame into `ss`'s command buffer — clear to `bg`, draw, then copy
    /// the result image into `dst` (the persistent readback buffer for the present path, or an
    /// imported guest-scanout buffer for the Fix-D zero-copy path). When `geom` is `Some` (Phase-2b),
    /// binds the mesh's vertex/index buffers and replays [`Geometry::draws`] (multi-draw, per-draw
    /// viewport); otherwise issues the single bufferless `draw(vertex_count)`. Resets the pool first;
    /// one submit thread per process ⇒ no in-flight hazard on the reused command buffer.
    #[allow(clippy::too_many_arguments)]
    fn record_forwarded_frame(
        &self,
        ss: &SizedScratch,
        render_pass: vk::RenderPass,
        pipeline: vk::Pipeline,
        width: u32,
        height: u32,
        bg: [f32; 4],
        draw: &ForwardedDraw,
        geom: Option<&GeometryGpu>,
        dst: vk::Buffer,
    ) -> R<()> {
        let dev = &self.device;
        unsafe {
            dev.reset_command_pool(ss.pool, vk::CommandPoolResetFlags::empty())?;
            dev.begin_command_buffer(
                ss.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            let clears = [vk::ClearValue { color: vk::ClearColorValue { float32: bg } }];
            dev.cmd_begin_render_pass(
                ss.cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(render_pass)
                    .framebuffer(ss.framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: vk::Extent2D { width, height },
                    })
                    .clear_values(&clears),
                vk::SubpassContents::INLINE,
            );
            dev.cmd_bind_pipeline(ss.cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
            // Scissor is full-frame for every draw; the per-draw viewport (below) is what places
            // geometry, and a full-frame scissor never clips it (geometry stays inside its viewport).
            dev.cmd_set_scissor(
                ss.cmd,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D { width, height },
                }],
            );
            let full_viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: width as f32,
                height: height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            match (geom, draw.geometry.as_ref()) {
                // Phase-2b: real mesh — bind the vertex (and optional index) buffer, then replay each
                // forwarded draw with its own viewport.
                (Some(gg), Some(g)) => {
                    dev.cmd_bind_vertex_buffers(ss.cmd, 0, &[gg.vbo], &[0]);
                    if let Some((ibo, _)) = gg.ibo {
                        let it = if g.index_u32 {
                            vk::IndexType::UINT32
                        } else {
                            vk::IndexType::UINT16
                        };
                        dev.cmd_bind_index_buffer(ss.cmd, ibo, 0, it);
                    }
                    let indexed = gg.ibo.is_some();
                    for d in g.draws {
                        let vp = if d.viewport[2] > 0.0 {
                            vk::Viewport {
                                x: d.viewport[0],
                                y: d.viewport[1],
                                width: d.viewport[2],
                                height: d.viewport[3],
                                min_depth: 0.0,
                                max_depth: 1.0,
                            }
                        } else {
                            full_viewport
                        };
                        dev.cmd_set_viewport(ss.cmd, 0, &[vp]);
                        let inst = d.instance_count.max(1);
                        if indexed {
                            dev.cmd_draw_indexed(
                                ss.cmd,
                                d.count,
                                inst,
                                d.first,
                                d.vertex_offset,
                                0,
                            );
                        } else {
                            dev.cmd_draw(ss.cmd, d.count, inst, d.first, 0);
                        }
                    }
                }
                // Phase-1 bufferless path: one shader-synthesized draw over the whole frame.
                _ => {
                    dev.cmd_set_viewport(ss.cmd, 0, &[full_viewport]);
                    dev.cmd_draw(ss.cmd, draw.vertex_count, 1, 0, 0);
                }
            }
            dev.cmd_end_render_pass(ss.cmd);
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
                ss.cmd,
                ss.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst,
                &[region],
            );
            dev.end_command_buffer(ss.cmd)?;
        }
        Ok(())
    }

    /// Fix D: import `[ptr, ptr+size)` (page-aligned host memory — e.g. the guest scanout in
    /// memfd-backed RAM) as a `TRANSFER_DST` buffer the GPU can copy a frame straight into. `size`
    /// is rounded up to the import alignment for the allocation. Fails (→ caller uses the copy path)
    /// if the extension is absent, the pointer is misaligned, or no HOST_VISIBLE type is compatible.
    ///
    /// # Safety
    /// `[ptr, ptr + round_up(size))` must be valid, mapped host memory that stays mapped for as long
    /// as the returned import is cached (see [`Self::forget_all_guest_imports`]).
    unsafe fn import_guest_buffer(&self, ptr: *mut u8, size: u64) -> R<ImportedGuest> {
        let loader = self
            .ext_mem_host
            .as_ref()
            .ok_or("VK_EXT_external_memory_host not supported")?;
        let align = self.host_ptr_align.max(1);
        if (ptr as usize as u64) % align != 0 {
            return Err(format!("guest pointer {ptr:?} not aligned to {align}").into());
        }
        let alloc_size = size.div_ceil(align) * align;
        let dev = &self.device;
        let handle_type = vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT;

        // Which memory types can back this host pointer?
        let mut hpp = vk::MemoryHostPointerPropertiesEXT::default();
        let r = (loader.fp().get_memory_host_pointer_properties_ext)(
            dev.handle(),
            handle_type,
            ptr as *const c_void,
            &mut hpp,
        );
        if r != vk::Result::SUCCESS {
            return Err(format!("vkGetMemoryHostPointerPropertiesEXT: {r:?}").into());
        }
        let type_index = (0..self.mem_props.memory_type_count)
            .find(|&i| {
                (hpp.memory_type_bits & (1 << i)) != 0
                    && self.mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
            })
            .ok_or("no HOST_VISIBLE memory type compatible with the guest pointer")?;

        let mut import = vk::ImportMemoryHostPointerInfoEXT::default()
            .handle_type(handle_type)
            .host_pointer(ptr as *mut c_void);
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(alloc_size)
            .memory_type_index(type_index)
            .push_next(&mut import);
        let mem = dev.allocate_memory(&alloc, None)?;

        let mut ext = vk::ExternalMemoryBufferCreateInfo::default().handle_types(handle_type);
        let buf_ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext);
        let buffer = match dev.create_buffer(&buf_ci, None) {
            Ok(b) => b,
            Err(e) => {
                dev.free_memory(mem, None);
                return Err(e.into());
            }
        };
        if let Err(e) = dev.bind_buffer_memory(buffer, mem, 0) {
            dev.destroy_buffer(buffer, None);
            dev.free_memory(mem, None);
            return Err(e.into());
        }
        Ok(ImportedGuest { mem, buffer })
    }

    /// Fix D: render a forwarded draw and DMA the result **straight into the imported guest scanout**
    /// at `guest_ptr` — no host readback buffer, no CPU copy (the biggest remaining hot-path cost,
    /// ~1 ms/frame at 1080p). Reuses the cached `(w,h)` scratch for everything but the copy
    /// destination, and caches the import keyed by `(guest_ptr, size)` (stable per VM boot). Requires
    /// the scratch cache + `VK_EXT_external_memory_host`; returns Err (→ caller falls back to the
    /// one-copy present) if import fails.
    ///
    /// # Safety
    /// `[guest_ptr, guest_ptr + width*height*4)` must be valid, mapped, page-aligned guest RAM that
    /// stays mapped until [`Self::forget_all_guest_imports`] is called (the device does so on any DMA
    /// remap). One submit thread per process ⇒ no concurrent forget vs. in-flight submit.
    pub unsafe fn render_forwarded_zerocopy(
        &self,
        width: u32,
        height: u32,
        bg: [f32; 4],
        draw: &ForwardedDraw,
        guest_ptr: *mut u8,
    ) -> R<()> {
        const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
        // Zero-copy scanout is the bufferless present fast path (Fix D). A Phase-2b mesh draw would
        // need its vertex buffers uploaded + freed per submit anyway, so it goes through the copy
        // present path instead; reject it here rather than silently drop the geometry and render a
        // bufferless frame (which would be wrong output, not just slow). The device never routes a
        // command-list draw here, so this is defense-in-depth.
        if draw.geometry.is_some() {
            return Err("render_forwarded_zerocopy: geometry draws use the copy present path".into());
        }
        let size = (width as u64) * (height as u64) * 4;
        let key = (guest_ptr as usize, size);

        let mut throwaway = RenderScratch::new(&self.device);
        let (render_pass, pipeline) = self.pipeline_for(&mut throwaway, draw, FORMAT)?;

        // Import (cached) — do it first so a failure returns before we touch the render scratch.
        let dst = {
            let mut imports = self.guest_imports.lock().unwrap_or_else(|e| e.into_inner());
            if !imports.contains_key(&key) {
                if imports.len() >= MAX_CACHED_SCRATCH {
                    for (_, ig) in imports.drain() {
                        ig.destroy(&self.device);
                    }
                }
                let ig = self.import_guest_buffer(guest_ptr, size)?;
                imports.insert(key, ig);
            }
            imports.get(&key).expect("just inserted above").buffer
        }; // the ImportedGuest stays in the map (alive) after the guard drops — `dst` is a handle

        let mut cache = self.scratch_cache.lock().unwrap_or_else(|e| e.into_inner());
        if !cache.contains_key(&(width, height)) {
            if cache.len() >= MAX_CACHED_SCRATCH {
                for (_, ss) in cache.drain() {
                    ss.destroy(&self.device);
                }
            }
            let ss = self.build_sized_scratch(width, height, render_pass)?;
            cache.insert((width, height), ss);
        }
        let ss = cache.get(&(width, height)).expect("just inserted above");

        self.record_forwarded_frame(ss, render_pass, pipeline, width, height, bg, draw, None, dst)?;
        self.device.reset_fences(&[ss.fence])?;
        let cmds = [ss.cmd];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        self.device.queue_submit(self.queue, &submit, ss.fence)?;
        self.wait_fence(ss.fence)?;
        // The GPU wrote guest RAM directly over PCIe (snooped → cache-coherent on x86), so the guest
        // CPU sees it after the fence. No CPU copy, and no invalidate (that was only for reading a
        // HOST_CACHED *host* readback buffer back on the CPU).
        Ok(())
    }

    /// Fix D: drop every cached guest-scanout import. The device calls this whenever it remaps guest
    /// RAM (a DMA_MAP/UNMAP), because the underlying host pointers may become invalid; the next
    /// zero-copy render re-imports lazily. No-op if none are cached.
    pub fn forget_all_guest_imports(&self) {
        let mut imports = self.guest_imports.lock().unwrap_or_else(|e| e.into_inner());
        for (_, ig) in imports.drain() {
            unsafe { ig.destroy(&self.device) };
        }
    }

    /// Fix B: build the persistent per-frame objects for `(width,height)`, sharing `render_pass`
    /// from the pipeline cache. Builds into a RAII guard (so any early `?` frees the partial work),
    /// then disarms it and moves ownership into the returned [`SizedScratch`].
    fn build_sized_scratch(
        &self,
        width: u32,
        height: u32,
        render_pass: vk::RenderPass,
    ) -> R<SizedScratch> {
        const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
        let dev = &self.device;
        let size = width as u64 * height as u64 * 4;
        let mut sc = RenderScratch::new(dev);

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
        sc.image = image;
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
        sc.img_mem = img_mem;
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
        sc.view = view;
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
        sc.framebuffer = framebuffer;
        let readback = unsafe {
            dev.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )?
        };
        sc.readback = readback;
        let rb_req = unsafe { dev.get_buffer_memory_requirements(readback) };
        let (rb_type, rb_coherent) = self.find_readback_mem(rb_req.memory_type_bits)?;
        let rb_mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(rb_req.size)
                    .memory_type_index(rb_type),
                None,
            )?
        };
        sc.rb_mem = rb_mem;
        unsafe { dev.bind_buffer_memory(readback, rb_mem, 0)? };
        let pool = unsafe {
            dev.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(self.queue_family),
                None,
            )?
        };
        sc.pool = pool;
        let cmd = unsafe {
            dev.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0]
        };
        let fence = unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None)? };
        sc.fence = fence;
        // Persistent host-coherent mapping (kept until destroy). Stored as usize (Send+Sync).
        let rb_ptr =
            unsafe { dev.map_memory(rb_mem, 0, size, vk::MemoryMapFlags::empty())? } as usize;

        // Success — transfer ownership out of the RAII guard so Drop won't free these objects.
        sc.disarm();
        Ok(SizedScratch {
            image,
            img_mem,
            view,
            framebuffer,
            readback,
            rb_mem,
            rb_ptr,
            rb_coherent,
            pool,
            cmd,
            fence,
            size,
        })
    }

    /// Render the triangle (as [`render_triangle`](Self::render_triangle)) into a
    /// device-local buffer whose memory is **exported as an fd** for zero-copy hand-off
    /// — a Linux dma-buf where the driver supports it, else an opaque fd. Proves the GPU
    /// output can be shared with another process/consumer without a host round-trip
    /// (ADR-0002/0003: the replay process hands frames to the presenter/encoder). Also
    /// reads the frame back (host-visible copy) so the caller can verify the render.
    /// Errors if the device can't export memory ([`can_export`](Self::can_export)).
    pub fn export_triangle_dmabuf(&self, width: u32, height: u32) -> R<(Frame, DmaBufExport)> {
        let (frame, export) = self.render_triangle_inner(
            width,
            height,
            [0.05, 0.05, 0.08, 1.0],
            &ForwardedDraw::builtin_triangle(),
            true,
        )?;
        let export = export.ok_or("device does not support external-memory fd export")?;
        Ok((frame, export))
    }

    fn render_triangle_inner(
        &self,
        width: u32,
        height: u32,
        bg: [f32; 4],
        draw: &ForwardedDraw,
        export: bool,
    ) -> R<(Frame, Option<DmaBufExport>)> {
        if export && self.external_fd.is_none() {
            return Err("device does not support external-memory fd export".into());
        }
        const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
        let dev = &self.device;
        let size = width as u64 * height as u64 * 4;

        // Everything created below is registered into `sc` and freed on Drop — so any early `?`
        // (e.g. a guest-forwarded pipeline that fails to compile) can't leak on the shared device.
        let mut sc = RenderScratch::new(dev);

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
        sc.image = image;
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
        sc.img_mem = img_mem;
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
        sc.view = view;

        // ---- render pass + graphics pipeline (Fix A: memoized across submits when caching is
        // enabled; the pipeline uses dynamic viewport+scissor so one entry serves every size) ----
        let (render_pass, pipeline) = self.pipeline_for(&mut sc, draw, FORMAT)?;

        // ---- framebuffer (per-frame; binds this frame's view to the render pass) ----
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
        sc.framebuffer = framebuffer;

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
        sc.readback = readback;
        let rb_req = unsafe { dev.get_buffer_memory_requirements(readback) };
        let (rb_type, rb_coherent) = self.find_readback_mem(rb_req.memory_type_bits)?;
        let rb_mem = unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(rb_req.size)
                    .memory_type_index(rb_type),
                None,
            )?
        };
        sc.rb_mem = rb_mem;
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
        if let Some(b) = export_buffer {
            sc.export_buffer = b;
        }
        if let Some(m) = export_mem {
            sc.export_mem = m;
        }

        // ---- record ----
        let pool = unsafe {
            dev.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(self.queue_family),
                None,
            )?
        };
        sc.pool = pool;
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
            // Dynamic viewport+scissor for this frame's size (Fix A: keeps the pipeline itself
            // resolution-independent so it is reused across differently-sized submits).
            dev.cmd_set_viewport(
                cmd,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: width as f32,
                    height: height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            dev.cmd_set_scissor(
                cmd,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D { width, height },
                }],
            );
            dev.cmd_draw(cmd, draw.vertex_count, 1, 0, 0); // no vertex buffer → SM execution
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
        sc.fence = fence;
        let cmds = [cmd];
        let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
        unsafe {
            dev.queue_submit(self.queue, &submit, fence)?;
        }
        self.wait_fence(fence)?;

        // ---- read back (HOST_CACHED: invalidate so the GPU's writes are visible before the
        // cached, fast read; coherent memory needs none — reading uncached memory dominated) ----
        let rgba = unsafe {
            let ptr = dev.map_memory(rb_mem, 0, size, vk::MemoryMapFlags::empty())? as *const u8;
            if !rb_coherent {
                dev.invalidate_mapped_memory_ranges(&[vk::MappedMemoryRange::default()
                    .memory(rb_mem)
                    .offset(0)
                    .size(vk::WHOLE_SIZE)])?;
            }
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
            // The exported fd keeps its own reference to the underlying allocation, so freeing
            // the VkDeviceMemory when `sc` drops does not invalidate it (Vulkan spec).
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            Some(DmaBufExport { fd, size: export_size, handle_type: handle_str })
        } else {
            None
        };

        // All per-render objects are freed by `sc`'s Drop as it goes out of scope here — on this
        // success path and on every early `?` above alike. The export fd (extracted above) keeps
        // its own reference, so freeing export_mem in Drop does not invalidate it.
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
        // Persist compiled pipelines to the shared on-disk cache (if enabled) while the device +
        // cache are still alive.
        self.save_pipeline_cache();
        unsafe {
            let _ = self.device.device_wait_idle();
            // Fix D: free the imported guest-scanout buffers/memory before the device (freeing the
            // VkDeviceMemory does NOT unmap the guest RAM — the import doesn't own it).
            self.forget_all_guest_imports();
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

    /// Fix D: render straight into an imported page-aligned host buffer (standing in for the
    /// memfd-backed guest scanout) and prove the pixels are byte-identical to the CPU-copy path.
    /// Validates the whole `VK_EXT_external_memory_host` import + `cmd_copy_image_to_buffer` →
    /// guest-RAM writeback mechanic on real silicon, without needing a full guest/QEMU stack.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn render_forwarded_zerocopy_matches_builtin() {
        use std::alloc::{alloc_zeroed, dealloc, Layout};
        // The zero-copy path reuses the (w,h) scratch, so the scratch cache must be on.
        std::env::set_var("INFINIGPU_SCRATCH_CACHE", "1");
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        if !gpu.supports_zerocopy_scanout() {
            eprintln!("skipping: no VK_EXT_external_memory_host on {}", gpu.device_name());
            return;
        }
        let (w, h) = (256u32, 256u32);
        let draw = ForwardedDraw::builtin_triangle();
        let bg = [0.02f32, 0.02, 0.03, 1.0];

        // Reference via the normal (copy) path.
        let reference = gpu.render_forwarded(w, h, bg, &draw).expect("reference render");

        // A page-aligned host buffer standing in for guest scanout RAM.
        let size = (w * h * 4) as usize;
        let align = 4096usize;
        let alloc = size.div_ceil(align) * align;
        let layout = Layout::from_size_align(alloc, align).expect("layout");
        // SAFETY: nonzero layout; freed below with the same layout.
        let raw = unsafe { alloc_zeroed(layout) };
        assert!(!raw.is_null(), "alloc");

        // SAFETY: `raw` is page-aligned and `[raw, raw+size)` is valid for the render's lifetime.
        unsafe {
            gpu.render_forwarded_zerocopy(w, h, bg, &draw, raw)
                .expect("zerocopy render");
        }
        let got = unsafe { std::slice::from_raw_parts(raw as *const u8, size) };
        let lit = got
            .chunks_exact(4)
            .filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8)
            .count();
        eprintln!("render_forwarded_zerocopy: lit={lit}/{}, matches builtin", w * h);
        assert_eq!(
            got,
            reference.rgba.as_slice(),
            "zerocopy pixels must be identical to the copy path"
        );
        unsafe { dealloc(raw, layout) };
    }

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

    // PR7 enabler: the converted BGRA is exported as a zero-copy dma-buf fd for NVENC ingest.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn convert_present_export_dmabuf() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        if !gpu.can_export() {
            eprintln!("skipping: device can't export memory fds");
            return;
        }
        let (w, h): (u32, u32) = (8, 8);
        let pitch = w * 4;
        let mut src = vec![0u8; (pitch * h) as usize];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            px.copy_from_slice(&[(i * 3) as u8, (i * 5) as u8, (i * 7) as u8, 0xFF]);
        }
        let (frame, export) = gpu
            .convert_present_export(w, h, pitch, vk::Format::R8G8B8A8_UNORM, &src)
            .expect("convert_present_export");
        // readback still correct (channel swap)
        for i in 0..(w * h) as usize {
            assert_eq!(frame.rgba[i * 4], src[i * 4 + 2]); // B
            assert_eq!(frame.rgba[i * 4 + 2], src[i * 4]); // R
        }
        // exported fd is real and big enough to hold the tight BGRA frame.
        assert!(export.raw_fd() >= 0);
        assert!(export.size() >= (w * h * 4) as u64, "export too small: {}", export.size());
        assert!(matches!(export.handle_type(), "dma-buf" | "opaque-fd"));
        // fd is a live, dup-able handle (proves it's a genuine OS resource, not a stale int).
        let dup = unsafe { libc::dup(export.raw_fd()) };
        assert!(dup >= 0, "exported fd not dup-able");
        unsafe { libc::close(dup) };
    }

    // Phase-1 host half: the FORWARDED-SPIR-V path must produce the same GPU render as the
    // fixed embedded path. Drives `render_forwarded` with the built-in triangle blob (i.e. the
    // SPIR-V arrives as a parameter, not a hardcoded const) and asserts it (a) draws a real
    // triangle — some but not all pixels differ from the background — and (b) is byte-identical
    // to `render_triangle`. Same inputs → deterministic rasterization → identical bytes. Proves
    // the host compiles + runs forwarded SPIR-V, the load-bearing assumption for the guest ICD.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn render_forwarded_matches_builtin() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let (w, h): (u32, u32) = (64, 64);
        let bg = [0.02, 0.02, 0.05, 1.0];
        let builtin = gpu.render_triangle(w, h, bg).expect("render_triangle");
        let forwarded = gpu
            .render_forwarded(w, h, bg, &ForwardedDraw::builtin_triangle())
            .expect("render_forwarded");

        assert_eq!((forwarded.width, forwarded.height), (w, h));
        // A real triangle: some pixels lit (differ from bg), but not the whole frame.
        let bg8 = [
            (bg[0] * 255.0).round() as u8,
            (bg[1] * 255.0).round() as u8,
            (bg[2] * 255.0).round() as u8,
        ];
        let lit = forwarded
            .rgba
            .chunks_exact(4)
            .filter(|px| px[0..3] != bg8)
            .count();
        let total = (w * h) as usize;
        assert!(lit > 0 && lit < total, "expected a triangle, got lit={lit}/{total}");
        // Faithful forwarding: identical to the fixed path, byte-for-byte.
        assert_eq!(forwarded.rgba, builtin.rgba, "forwarded render differs from builtin");
        eprintln!("render_forwarded: lit={lit}/{total}, matches builtin");
    }

    // Stronger than the byte-identity check: prove the forwarded SPIR-V actually EXECUTED correctly
    // (right colours, right interpolation) across resolutions, and that the render is deterministic
    // (which the frame-elision opt depends on). The built-in triangle's three vertex colours are
    // (1,.15,.15)/(.15,1,.15)/(.15,.15,1) — each sums to 1.3 — so any barycentric blend of them sums
    // to 1.3 too: EVERY lit pixel must have R+G+B ≈ 1.3·255 = 331. A flat/solid/wrong shader or broken
    // interpolation breaks this invariant; a real gradient also shows each vertex colour near a corner.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn render_forwarded_colors_are_correct_and_deterministic() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let draw = ForwardedDraw::builtin_triangle();
        let bg = [0.02f32, 0.02, 0.05, 1.0];
        let bg8 = [
            (bg[0] * 255.0).round() as u8,
            (bg[1] * 255.0).round() as u8,
            (bg[2] * 255.0).round() as u8,
        ];
        for &(w, h) in &[(16u32, 16u32), (64, 64), (256, 256), (640, 480), (1920, 1080), (200, 137)] {
            let f = gpu.render_forwarded(w, h, bg, &draw).expect("render");
            assert_eq!((f.width, f.height), (w, h));
            let (mut lit, mut bad_sum) = (0usize, 0usize);
            let (mut max_r, mut max_g, mut max_b) = (0u8, 0u8, 0u8);
            for px in f.rgba.chunks_exact(4) {
                let (r, g, b) = (px[0], px[1], px[2]);
                // Background (allow a 1-LSB driver dither) → not a triangle pixel.
                if (r as i32 - bg8[0] as i32).abs() <= 1
                    && (g as i32 - bg8[1] as i32).abs() <= 1
                    && (b as i32 - bg8[2] as i32).abs() <= 1
                {
                    continue;
                }
                lit += 1;
                let sum = r as u32 + g as u32 + b as u32;
                if !(300..=360).contains(&sum) {
                    bad_sum += 1; // not a valid blend of the three vertex colours
                }
                max_r = max_r.max(r);
                max_g = max_g.max(g);
                max_b = max_b.max(b);
            }
            let total = (w * h) as usize;
            assert!(
                lit > total / 50 && lit < total,
                "not a triangle at {w}x{h}: lit={lit}/{total}"
            );
            assert_eq!(
                bad_sum, 0,
                "at {w}x{h}: {bad_sum} lit pixels aren't a valid vertex-colour blend (R+G+B≉331) — \
                 the fragment shader/interpolation is wrong"
            );
            // A real gradient reaches each vertex colour near its corner (a flat centroid fill would
            // top out at ~110 per channel); this proves interpolation spans all three, not a constant.
            assert!(
                max_r > 170 && max_g > 170 && max_b > 170,
                "at {w}x{h}: interpolation missing a vertex colour (max r={max_r} g={max_g} b={max_b})"
            );
            // Determinism (the frame-elision opt relies on identical input → identical output), through
            // the default cached path.
            let f2 = gpu.render_forwarded(w, h, bg, &draw).expect("render2");
            assert_eq!(f.rgba, f2.rgba, "non-deterministic render at {w}x{h}");
        }
        eprintln!("render_forwarded_colors_are_correct_and_deterministic: OK across 6 resolutions");
    }

    // Regression guard for the Phase-1b adversarial-review finding: a forwarded draw that fails a
    // driver call (here an entry-point name matching no OpEntryPoint → create_graphics_pipelines
    // errors) must return Err cleanly and free its Vulkan objects via RenderScratch's Drop — NOT
    // leak them on the shared VkDevice. A flood of such submits previously leaked up to ~64 MiB
    // each. Here we fire many, assert each errors without panicking, then confirm the device is
    // still healthy (a good render succeeds) — i.e. the error path is clean and repeatable.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn render_forwarded_bad_entry_errors_without_leaking() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let (w, h) = (64u32, 64u32);
        let bg = [0.0, 0.0, 0.0, 1.0];
        let bad = ForwardedDraw {
            vertex_spirv: &shaders::TRIANGLE_SPV,
            vertex_entry: c"no_such_entry", // valid module, but no such OpEntryPoint
            fragment_spirv: &shaders::TRIANGLE_SPV,
            fragment_entry: c"fs_main",
            vertex_count: 3,
            topology: 0,
            geometry: None,
        };
        for i in 0..64 {
            assert!(
                gpu.render_forwarded(w, h, bg, &bad).is_err(),
                "bad entry point must Err (not panic/succeed) on iteration {i}"
            );
        }
        // The shared device is still usable after the error flood — no wedge, no fatal exhaustion.
        let good = gpu
            .render_forwarded(w, h, bg, &ForwardedDraw::builtin_triangle())
            .expect("a good forwarded render still succeeds after the error flood");
        assert_eq!((good.width, good.height), (w, h));
    }

    // ---- Phase-2b: real mesh (vertex buffers, multi-draw, per-viewport, indexed) ---------------

    /// Compile a WGSL shader for one stage to SPIR-V words with naga (a pure-Rust compiler — no
    /// glslang/glslc needed), forcing a single entry point named `main`. Lets these tests author
    /// real VBO-reading shaders offline, so the geometry path is exercised with actual meshes rather
    /// than only the built-in bufferless triangle.
    fn compile_wgsl(src: &str, stage: naga::ShaderStage) -> Vec<u32> {
        let module = naga::front::wgsl::parse_str(src).expect("wgsl parse");
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .expect("wgsl validate");
        let pipe = naga::back::spv::PipelineOptions {
            shader_stage: stage,
            entry_point: "main".to_string(),
        };
        naga::back::spv::write_vec(
            &module,
            &info,
            &naga::back::spv::Options::default(),
            Some(&pipe),
        )
        .expect("spv emit")
    }

    /// Vertex shader reading `pos: vec2 @location(0)` + `color: vec3 @location(1)` from the VBO.
    const MESH_VS: &str = r#"
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec3<f32> };
@vertex
fn main(@location(0) p: vec2<f32>, @location(1) c: vec3<f32>) -> VOut {
    return VOut(vec4<f32>(p, 0.0, 1.0), c);
}
"#;
    /// Fragment shader emitting the interpolated vertex colour.
    const MESH_FS: &str = r#"
@fragment
fn main(@location(0) c: vec3<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(c, 1.0);
}
"#;

    /// Pack `[x, y, r, g, b]` vertices (stride 20B) into a VBO byte blob.
    fn pack_verts(verts: &[[f32; 5]]) -> Vec<u8> {
        let mut b = Vec::with_capacity(verts.len() * 20);
        for v in verts {
            for f in v {
                b.extend_from_slice(&f.to_le_bytes());
            }
        }
        b
    }

    /// The pos(vec2)+color(vec3) attribute layout for [`MESH_VS`], stride 20.
    fn mesh_attrs() -> [VertexAttr; 2] {
        use infinigpu_abi::wire::vk_vformat as V;
        [
            VertexAttr { location: 0, format: V::R32G32_SFLOAT, offset: 0 },
            VertexAttr { location: 1, format: V::R32G32B32_SFLOAT, offset: 8 },
        ]
    }

    /// A forwarded VBO triangle renders **the mesh's own geometry and colours** — not the built-in
    /// triangle. Proves the whole Phase-2b path on real silicon: non-empty vertex-input state, a
    /// bound vertex buffer, per-vertex attributes flowing to the fragment shader. The triangle is
    /// placed at known NDC positions with pure R/G/B vertices, so: its centroid is a blended
    /// interior pixel (R+G+B ≈ 255, the barycentric sum of three unit-sum colours); the region near
    /// the blue vertex is blue-dominant; and the frame corners (outside the triangle) stay
    /// background. A gl_VertexIndex shader ignoring the VBO could not produce this.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_vbo_triangle_renders_mesh_colors() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let vs = compile_wgsl(MESH_VS, naga::ShaderStage::Vertex);
        let fs = compile_wgsl(MESH_FS, naga::ShaderStage::Fragment);
        assert_ne!(vs, fs, "vertex and fragment SPIR-V must be distinct modules");
        // Upward-pointing triangle, centered. Vulkan clip space: +y is DOWN. Pure R/G/B vertices.
        let verts = pack_verts(&[
            [-0.6, 0.5, 1.0, 0.0, 0.0],  // bottom-left  = red
            [0.6, 0.5, 0.0, 1.0, 0.0],   // bottom-right = green
            [0.0, -0.6, 0.0, 0.0, 1.0],  // top-center   = blue
        ]);
        let attrs = mesh_attrs();
        let draws = [DrawCmd { count: 3, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0; 4] }];
        let draw = ForwardedDraw {
            vertex_spirv: &vs,
            vertex_entry: c"main",
            fragment_spirv: &fs,
            fragment_entry: c"main",
            vertex_count: 0,
            topology: 0,
            geometry: Some(Geometry {
                vertex_data: &verts,
                vertex_stride: 20,
                attrs: &attrs,
                index_data: &[],
                index_u32: false,
                draws: &draws,
            }),
        };
        let (w, h) = (256u32, 256u32);
        let bg = [0.0, 0.0, 0.0, 1.0];
        let frame = gpu.render_forwarded(w, h, bg, &draw).expect("mesh render");
        assert_eq!(frame.rgba.len(), (w * h * 4) as usize);

        // NDC -> pixel: px = (ndc.x*0.5+0.5)*W, py = (ndc.y*0.5+0.5)*H.
        let px = |nx: f32| ((nx * 0.5 + 0.5) * w as f32).round().clamp(0.0, (w - 1) as f32) as u32;
        let py = |ny: f32| ((ny * 0.5 + 0.5) * h as f32).round().clamp(0.0, (h - 1) as f32) as u32;

        // Centroid at NDC (0, 0.133): a blended interior pixel. Unit-sum colours ⇒ R+G+B ≈ 255.
        let c = frame.pixel(px(0.0), py(0.133));
        let sum = c[0] as u32 + c[1] as u32 + c[2] as u32;
        assert!(
            (215..=295).contains(&sum),
            "centroid must be a blended interior pixel (R+G+B≈255), got {c:?} sum={sum}"
        );
        assert_eq!(c[3], 255, "alpha opaque");

        // All three pure vertex colours must render *somewhere* — proof the per-vertex colour
        // attribute flows from the VBO through to the fragment output. (Orientation-agnostic: which
        // screen corner each vertex lands in depends on clip-space handedness, but a shader ignoring
        // the VBO could never produce a distinct red AND green AND blue region.)
        let (mut red, mut green, mut blue) = (0u32, 0u32, 0u32);
        for p in frame.rgba.chunks_exact(4) {
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            if r > 150 && r - g > 60 && r - b > 60 {
                red += 1;
            }
            if g > 150 && g - r > 60 && g - b > 60 {
                green += 1;
            }
            if b > 150 && b - r > 60 && b - g > 60 {
                blue += 1;
            }
        }
        assert!(
            red > 40 && green > 40 && blue > 40,
            "each vertex colour must render a region (red={red} green={green} blue={blue})"
        );

        // The four corners are outside the triangle → background.
        for (cx, cy) in [(2, 2), (w - 3, 2), (2, h - 3), (w - 3, h - 3)] {
            let p = frame.pixel(cx, cy);
            assert!(
                p[0] < 20 && p[1] < 20 && p[2] < 20,
                "corner ({cx},{cy}) must be background, got {p:?}"
            );
        }

        // Determinism: a second render is byte-identical (cached pipeline + re-uploaded VBO).
        let f2 = gpu.render_forwarded(w, h, bg, &draw).expect("mesh render 2");
        assert_eq!(frame.rgba, f2.rgba, "mesh render must be deterministic");
        eprintln!("forwarded_vbo_triangle_renders_mesh_colors: OK (centroid sum={sum})");
    }

    /// Multi-draw + per-draw viewport + indexed draw, all in one submit. Draw #1 fills the LEFT half
    /// (its own viewport) with a red indexed quad; draw #2 fills the RIGHT half with a green indexed
    /// quad — from a single shared vertex+index buffer. Proves: `cmd_draw_indexed`, a bound index
    /// buffer, more than one draw per submit, and that each draw's viewport places it independently.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_multidraw_viewports_indexed() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let vs = compile_wgsl(MESH_VS, naga::ShaderStage::Vertex);
        let fs = compile_wgsl(MESH_FS, naga::ShaderStage::Fragment);
        // A fullscreen quad in NDC (covers the whole of whatever viewport it's drawn into). Verts
        // 0..3 red, 4..7 green — the vertex_offset selects which colour set a draw uses.
        let quad = |cr: f32, cg: f32, cb: f32| {
            [
                [-1.0, -1.0, cr, cg, cb],
                [1.0, -1.0, cr, cg, cb],
                [1.0, 1.0, cr, cg, cb],
                [-1.0, 1.0, cr, cg, cb],
            ]
        };
        let mut v = Vec::new();
        v.extend_from_slice(&quad(1.0, 0.0, 0.0)); // 0..3 red
        v.extend_from_slice(&quad(0.0, 1.0, 0.0)); // 4..7 green
        let verts = pack_verts(&v);
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let index_data: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
        let attrs = mesh_attrs();
        let (w, h) = (256u32, 128u32);
        let hw = w as f32 / 2.0;
        let draws = [
            // Left half: red quad (vertices 0..3).
            DrawCmd { count: 6, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0, 0.0, hw, h as f32] },
            // Right half: green quad (same 6 indices + vertex_offset 4 → vertices 4..7).
            DrawCmd { count: 6, instance_count: 1, first: 0, vertex_offset: 4, viewport: [hw, 0.0, hw, h as f32] },
        ];
        let draw = ForwardedDraw {
            vertex_spirv: &vs,
            vertex_entry: c"main",
            fragment_spirv: &fs,
            fragment_entry: c"main",
            vertex_count: 0,
            topology: 0,
            geometry: Some(Geometry {
                vertex_data: &verts,
                vertex_stride: 20,
                attrs: &attrs,
                index_data: &index_data,
                index_u32: false,
                draws: &draws,
            }),
        };
        let bg = [0.0, 0.0, 0.0, 1.0];
        let frame = gpu.render_forwarded(w, h, bg, &draw).expect("multidraw render");

        let left = frame.pixel(w / 4, h / 2);
        let right = frame.pixel(3 * w / 4, h / 2);
        assert!(
            left[0] > 200 && left[1] < 40 && left[2] < 40,
            "left viewport must be red, got {left:?}"
        );
        assert!(
            right[1] > 200 && right[0] < 40 && right[2] < 40,
            "right viewport must be green, got {right:?}"
        );
        eprintln!("forwarded_multidraw_viewports_indexed: OK (left={left:?} right={right:?})");
    }
}
