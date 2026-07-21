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

/// Depth-test state for a forwarded mesh (Phase-2d). Applies to the whole draw list. When present
/// with `test` or `write` set, the render pass gains a depth attachment and the pipeline a
/// depth-stencil state — hidden-surface removal for real 3D scenes. `None`/all-off ⇒ no depth
/// attachment (the 2D / painter's-order path, unchanged).
#[derive(Clone, Copy)]
pub struct DepthState {
    /// Enable the depth test (compare each fragment against the depth buffer).
    pub test: bool,
    /// Write passing fragments' depth back to the buffer.
    pub write: bool,
    /// Wire compare-op (`infinigpu_abi::wire::depth_compare`); mapped by [`map_compare`].
    pub compare: u32,
}

impl DepthState {
    /// Whether a depth attachment is needed (a pure no-op depth state needs none).
    fn attachment_needed(&self) -> bool {
        self.test || self.write
    }
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
    /// Depth-test state (Phase-2d); `None` ⇒ no depth buffer (2D / painter's order).
    pub depth: Option<DepthState>,
    /// Push-constant bytes (Phase-2c) — a transform block (e.g. an MVP `mat4`) the shaders read via
    /// `var<push_constant>` / `layout(push_constant)`. Empty ⇒ an empty pipeline layout (no
    /// transform, raw NDC). Applied to the VERTEX|FRAGMENT stages at offset 0 before the draws.
    pub push_constants: &'a [u8],
    /// Sampled textures (Phase-2c) bound into descriptor set 0. Texture `i` binds a sampled image at
    /// `tex_binding + 2i` and a sampler at `tex_binding + 2i + 1`, matching consecutive WGSL
    /// `texture_2d`/`sampler` pairs — the layout a real material shader (albedo/normal/roughness/…) uses.
    /// Empty ⇒ untextured. Capped at [`MAX_TEXTURES`].
    pub textures: &'a [Texture<'a>],
    /// Base binding of texture 0's sampled image (its sampler is at `tex_binding + 1`; texture `i` at
    /// `tex_binding + 2i` / `+ 2i + 1`). `0` ⇒ image@0 / sampler@1 (the pre-UBO default). Non-zero lets a
    /// UBO and the textures share set 0 at distinct bindings. Ignored when `textures` is empty.
    pub tex_binding: u32,
    /// One uniform buffer (Phase-2c) bound at descriptor set 0 binding `uniform.binding`
    /// (VERTEX|FRAGMENT), for a shader's `var<uniform>` block (e.g. per-frame matrices). `None` ⇒ no
    /// UBO. Composes with `texture` in the same set at a distinct binding.
    pub uniform: Option<UniformBlock<'a>>,
    /// One READ-ONLY storage buffer (Phase-2c SSBO) bound at descriptor set 0 binding `storage.binding`
    /// (VERTEX|FRAGMENT), for a shader's `var<storage>` block (a DXVK structured/raw SRV, a skinning
    /// palette, per-instance data). `None` ⇒ no SSBO. Composes with the UBO + textures in the same set
    /// at a distinct binding. Never written back (a shader write is discarded — read-only scope).
    pub storage: Option<StorageBlock<'a>>,
    /// Static fixed-function rasterization + blend state (Phase-2d-A5) as an
    /// [`infinigpu_abi::wire::raster_flags`] bitfield: cull mode, front-face winding, alpha-blend
    /// enable. `0` ⇒ cull NONE / front-face CCW / blend off — the state `build_pipeline` hardcoded
    /// before this field, so a bufferless or older draw is unchanged. Baked into the pipeline (keyed
    /// in [`PipelineKey`]), so distinct states get distinct cached pipelines.
    pub raster_flags: u32,
}

/// Max sampled textures a single forwarded draw may bind into descriptor set 0 (Phase-2c multi-texture).
/// A real material shader binds a handful (albedo/normal/metallic/roughness/ao/emissive); 8 covers the
/// common case. The device decode enforces the same cap fail-closed.
pub const MAX_TEXTURES: usize = 8;

/// Max bytes a single forwarded storage buffer (SSBO) may carry. Deliberately far larger than the UBO's
/// 64 KiB cap — Vulkan guarantees `maxStorageBufferRange` ≥ 128 MiB, and real SSBO content (skinning
/// palettes, per-instance arrays, DXVK structured buffers) routinely exceeds 64 KiB. 16 MiB comfortably
/// covers that while staying under the whole-payload ceiling (`MAX_CMD_BYTES`). Enforced identically at
/// the device decode, this host reject, and the guest clamp.
pub const MAX_SSBO_BYTES: usize = 16 * 1024 * 1024;

/// A forwarded uniform buffer (Phase-2c): a declared descriptor-set-0 binding + the raw bytes the host
/// uploads into a `UNIFORM_BUFFER` for the shader's `var<uniform>` block to read (VERTEX|FRAGMENT).
#[derive(Clone, Copy)]
pub struct UniformBlock<'a> {
    /// Descriptor-set-0 binding this UBO occupies (must differ from the texture's image/sampler bindings).
    pub binding: u32,
    /// The uniform bytes (host uploads verbatim; std140 layout is the shader author's concern).
    pub bytes: &'a [u8],
}

/// A forwarded READ-ONLY storage buffer (Phase-2c SSBO): a declared descriptor-set-0 binding + the raw
/// bytes the host uploads into a `STORAGE_BUFFER` for the shader's `var<storage>` block to read
/// (VERTEX|FRAGMENT). std430 layout is the shader author's concern (opaque bytes host-side); never
/// written back.
#[derive(Clone, Copy)]
pub struct StorageBlock<'a> {
    /// Descriptor-set-0 binding this SSBO occupies (must differ from the UBO + texture bindings).
    pub binding: u32,
    /// The storage bytes (host uploads verbatim).
    pub bytes: &'a [u8],
}

/// Sampler configuration for a forwarded [`Texture`].
#[derive(Clone, Copy)]
pub struct SamplerCfg {
    /// Linear min+mag filtering; `false` ⇒ nearest.
    pub linear: bool,
    /// Repeat/wrap addressing; `false` ⇒ clamp-to-edge.
    pub repeat: bool,
}

/// A sampled texture forwarded from the guest: tightly-packed `R8G8B8A8_UNORM` pixels + a sampler
/// config. The host uploads the pixels to a device-local image, transitions it to shader-read, and
/// binds it (plus a sampler) through a descriptor set for the fragment shader to `textureSample`.
pub struct Texture<'a> {
    pub width: u32,
    pub height: u32,
    /// `width*height*4` RGBA8 bytes.
    pub rgba: &'a [u8],
    pub sampler: SamplerCfg,
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

/// The depth attachment format used for forwarded meshes (Phase-2d). `D32_SFLOAT` is required to be
/// supported as a depth attachment by every Vulkan implementation, so no format-support query is
/// needed.
const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;

/// Wire `depth_compare` u32 → `VkCompareOp`. Unknown values fall back to `LESS_OR_EQUAL` (the
/// conventional default for a `[0,1]`, 1.0-cleared depth buffer): a fail-safe that still does
/// sensible hidden-surface removal rather than erroring.
fn map_compare(c: u32) -> vk::CompareOp {
    use infinigpu_abi::wire::depth_compare as C;
    match c {
        C::NEVER => vk::CompareOp::NEVER,
        C::LESS => vk::CompareOp::LESS,
        C::EQUAL => vk::CompareOp::EQUAL,
        C::LESS_OR_EQUAL => vk::CompareOp::LESS_OR_EQUAL,
        C::GREATER => vk::CompareOp::GREATER,
        C::NOT_EQUAL => vk::CompareOp::NOT_EQUAL,
        C::GREATER_OR_EQUAL => vk::CompareOp::GREATER_OR_EQUAL,
        C::ALWAYS => vk::CompareOp::ALWAYS,
        _ => vk::CompareOp::LESS_OR_EQUAL,
    }
}

/// Map a [`wire::raster_flags`](infinigpu_abi::wire::raster_flags) cull sub-field to `VkCullModeFlags`.
/// Unknown values fall back to `NONE` (render everything) — fail-safe: a bad flag never hides geometry.
fn map_cull_mode(rf: u32) -> vk::CullModeFlags {
    use infinigpu_abi::wire::cull_mode as C;
    match infinigpu_abi::wire::raster_flags::cull(rf) {
        C::FRONT => vk::CullModeFlags::FRONT,
        C::BACK => vk::CullModeFlags::BACK,
        C::FRONT_AND_BACK => vk::CullModeFlags::FRONT_AND_BACK,
        _ => vk::CullModeFlags::NONE,
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
    /// Keyed by `(width, height, with_depth)` — a depth-testing mesh (Phase-2d) needs a scratch whose
    /// framebuffer has a depth attachment, distinct from the color-only scratch at the same size.
    scratch_cache: Mutex<HashMap<(u32, u32, bool), SizedScratch>>,
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
    /// Phase-2c: descriptor-set-0 layouts keyed by [`DescriptorSig`] (which resources at which bindings).
    /// Built lazily per unique signature and reused (content-independent). Bounded to avoid an
    /// adversarial guest growing it unboundedly; all entries destroyed on drop.
    desc_set_layouts: Mutex<HashMap<DescriptorSig, vk::DescriptorSetLayout>>,
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
    depth_view: vk::ImageView,
    depth_image: vk::Image,
    depth_mem: vk::DeviceMemory,
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
            depth_view: vk::ImageView::null(),
            depth_image: vk::Image::null(),
            depth_mem: vk::DeviceMemory::null(),
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
        self.depth_view = vk::ImageView::null();
        self.depth_image = vk::Image::null();
        self.depth_mem = vk::DeviceMemory::null();
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
            if self.depth_view != vk::ImageView::null() {
                d.destroy_image_view(self.depth_view, None);
            }
            if self.depth_image != vk::Image::null() {
                d.destroy_image(self.depth_image, None);
            }
            if self.depth_mem != vk::DeviceMemory::null() {
                d.free_memory(self.depth_mem, None);
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
    /// Phase-2d depth-stencil state packed as `attachment | test<<1 | write<<2 | compare<<4`; `0`
    /// (no depth attachment) for the built-in / 2D paths, so their key is unchanged.
    depth_key: u32,
    /// Phase-2c push-constant block size (bytes); `0` ⇒ an empty pipeline layout. The layout (hence
    /// the pipeline) differs by this, so it is part of the key.
    pc_size: u32,
    /// Phase-2c: the descriptor-set-0 signature (which resources at which bindings). The pipeline layout
    /// (hence the pipeline) includes the descriptor-set layout built from this, so it is part of the key.
    /// `None` ⇒ no descriptor set (empty layout, the pre-2c paths).
    desc_sig: Option<DescriptorSig>,
    /// Phase-2d-A5 static rasterization+blend state (cull/front-face/blend) baked into the pipeline,
    /// as an [`infinigpu_abi::wire::raster_flags`] bitfield. `0` ⇒ cull NONE / CCW / blend off, so the
    /// pre-A5 paths key exactly as before. Distinct states ⇒ distinct cached pipelines.
    raster_flags: u32,
}

/// The descriptor-set-0 signature a draw needs (Phase-2c): which resources bind where. The host builds
/// a `VkDescriptorSetLayout` from this — a `UNIFORM_BUFFER` at `ubo_binding` (VERTEX|FRAGMENT), and/or
/// `tex_count` `SAMPLED_IMAGE` + `SAMPLER` pairs at `tex_binding + 2i` / `+ 2i + 1` (FRAGMENT). The full
/// layout is fixed by `(tex_binding, tex_count)`, so those two fields key it. Same signature ⇒ same
/// layout ⇒ same pipeline, so it is cached and part of the [`PipelineKey`].
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
struct DescriptorSig {
    ubo_binding: Option<u32>,
    /// Base binding of texture 0's sampled image; `None` ⇒ no textures. Present iff `tex_count > 0`.
    tex_binding: Option<u32>,
    /// Number of sampled textures (each an image+sampler pair). `0` ⇒ none.
    tex_count: u32,
    /// Binding of the read-only storage buffer (SSBO); `None` ⇒ no SSBO. Present iff the draw has one.
    ssbo_binding: Option<u32>,
}

/// The descriptor-set signature a draw needs, or `None` if it binds no descriptor resources (Phase-2c).
/// Textures bind image+sampler pairs from `tex_binding` (texture `i` at `+2i`/`+2i+1`); a UBO at its
/// `binding`; an SSBO at its `binding`.
fn desc_sig_of(draw: &ForwardedDraw) -> Option<DescriptorSig> {
    let g = draw.geometry.as_ref()?;
    let ubo_binding = g.uniform.as_ref().map(|u| u.binding);
    let ssbo_binding = g.storage.as_ref().map(|s| s.binding);
    let tex_count = g.textures.len() as u32;
    let tex_binding = (tex_count > 0).then_some(g.tex_binding);
    if ubo_binding.is_none() && tex_binding.is_none() && ssbo_binding.is_none() {
        None
    } else {
        Some(DescriptorSig { ubo_binding, tex_binding, tex_count, ssbo_binding })
    }
}

/// Pack a `ForwardedDraw`'s depth state into the [`PipelineKey`] `depth_key` field. Returns
/// `(with_depth_attachment, depth_key)`. `0` when no depth attachment is needed.
fn depth_key_of(draw: &ForwardedDraw) -> (bool, u32) {
    match draw.geometry.as_ref().and_then(|g| g.depth) {
        Some(d) if d.test || d.write => {
            let key = 1 | ((d.test as u32) << 1) | ((d.write as u32) << 2) | ((d.compare & 0x7) << 4);
            (true, key)
        }
        _ => (false, 0),
    }
}

/// Static rasterization+blend state of a draw (Phase-2d-A5), as an
/// [`infinigpu_abi::wire::raster_flags`] bitfield; `0` (cull NONE / CCW / blend off) for a bufferless
/// draw or one that didn't request any (the pre-A5 default). Baked into the pipeline, so it is part of
/// the [`PipelineKey`].
fn raster_flags_of(draw: &ForwardedDraw) -> u32 {
    draw.geometry.as_ref().map(|g| g.raster_flags).unwrap_or(0)
}

/// Byte size of a draw's push-constant block (Phase-2c); `0` when there are none. The pipeline layout
/// carries a matching push-constant range, so this is part of the [`PipelineKey`].
fn push_const_size_of(draw: &ForwardedDraw) -> u32 {
    draw.geometry
        .as_ref()
        .map(|g| g.push_constants.len() as u32)
        .unwrap_or(0)
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
    /// Keyed by `(color format raw, with_depth)` — the two render-pass variants (Phase-2d added the
    /// depth variant).
    render_passes: HashMap<(i32, bool), vk::RenderPass>,
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
    /// Phase-2d depth attachment (image + memory + view), present iff this scratch was built for a
    /// depth-testing render pass; all null otherwise. The framebuffer above then has depth at
    /// attachment 1, and the record path adds a depth clear value.
    has_depth: bool,
    depth_image: vk::Image,
    depth_mem: vk::DeviceMemory,
    depth_view: vk::ImageView,
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

/// Phase-2c: a sampled texture uploaded to the GPU for **one** submit — a host-visible staging buffer
/// (filled with the guest's RGBA8), a device-local sampled image, its view, a sampler, and a
/// descriptor pool + set (binding 0 = image, binding 1 = sampler). The staging→image copy + layout
/// transitions are recorded into the frame's command buffer (before the render pass); everything is
/// freed after the submit fence. Per submit (texture content varies); a by-id cache is a follow-up.
/// The descriptor pool + set no longer live here — they live in [`FrameDescriptors`] so a UBO and a
/// texture can share one set (Phase-2c UBO composition).
struct TextureGpu {
    staging: vk::Buffer,
    staging_mem: vk::DeviceMemory,
    image: vk::Image,
    image_mem: vk::DeviceMemory,
    view: vk::ImageView,
    sampler: vk::Sampler,
    width: u32,
    height: u32,
}

impl TextureGpu {
    /// # Safety
    /// `dev` must be the owning device and no submit referencing these objects may still be in flight.
    unsafe fn destroy(&self, dev: &ash::Device) {
        dev.destroy_sampler(self.sampler, None);
        dev.destroy_image_view(self.view, None);
        dev.destroy_image(self.image, None);
        dev.free_memory(self.image_mem, None);
        dev.destroy_buffer(self.staging, None);
        dev.free_memory(self.staging_mem, None);
    }
}

/// Phase-2c: the guest's uniform buffer uploaded to a host-visible+coherent `UNIFORM_BUFFER` for **one**
/// submit — the forwarded bytes memcpy straight in (no staging/copy; per submit, a by-id cache is a
/// follow-up). Freed after the submit fence.
struct UniformGpu {
    buffer: vk::Buffer,
    mem: vk::DeviceMemory,
}

impl UniformGpu {
    /// # Safety
    /// `dev` must be the owning device and no submit referencing the buffer may still be in flight.
    unsafe fn destroy(&self, dev: &ash::Device) {
        dev.destroy_buffer(self.buffer, None);
        dev.free_memory(self.mem, None);
    }
}

/// Phase-2c SSBO: the guest's storage buffer uploaded to a host-visible+coherent `STORAGE_BUFFER` for
/// **one** submit — the forwarded bytes memcpy straight in (read-only, no writeback). Freed after the
/// submit fence. Same shape as [`UniformGpu`]; distinct type so the descriptor write can't confuse them.
struct StorageGpu {
    buffer: vk::Buffer,
    mem: vk::DeviceMemory,
}

impl StorageGpu {
    /// # Safety
    /// `dev` must be the owning device and no submit referencing the buffer may still be in flight.
    unsafe fn destroy(&self, dev: &ash::Device) {
        dev.destroy_buffer(self.buffer, None);
        dev.free_memory(self.mem, None);
    }
}

/// Phase-2c: descriptor set 0 for **one** submit — a pool, the single set allocated from it, and the
/// resources written into that set (a UBO, an SSBO, and/or textures at their declared bindings). Composing
/// them into one set is what lets a real app use per-frame uniforms AND a storage buffer AND a texture at
/// once. Destroyed after the submit fence (the pool frees the set; the buffer/image objects are freed too).
struct FrameDescriptors {
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    ubo: Option<UniformGpu>,
    ssbo: Option<StorageGpu>,
    tex: Vec<TextureGpu>,
}

impl FrameDescriptors {
    /// # Safety
    /// `dev` must be the owning device and no submit referencing these objects may still be in flight.
    unsafe fn destroy(&self, dev: &ash::Device) {
        if self.pool != vk::DescriptorPool::null() {
            dev.destroy_descriptor_pool(self.pool, None); // frees the set
        }
        if let Some(u) = &self.ubo {
            u.destroy(dev);
        }
        if let Some(s) = &self.ssbo {
            s.destroy(dev);
        }
        for t in &self.tex {
            t.destroy(dev);
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
        if self.has_depth {
            dev.destroy_image_view(self.depth_view, None);
            dev.destroy_image(self.depth_image, None);
            dev.free_memory(self.depth_mem, None);
        }
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
            desc_set_layouts: Mutex::new(HashMap::new()),
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
    fn build_render_pass(&self, format: vk::Format, with_depth: bool) -> R<vk::RenderPass> {
        let color = vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        // Phase-2d: an optional depth attachment (index 1). CLEAR at load, no store (transient — the
        // readback only wants colour). Its own external dependency orders early/late fragment-test
        // writes after any prior frame's use of the reused image.
        let depth = vk::AttachmentDescription::default()
            .format(DEPTH_FORMAT)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::DONT_CARE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);
        let color_ref = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let depth_ref = vk::AttachmentReference::default()
            .attachment(1)
            .layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);

        let color_dep = vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
        // Order this frame's depth clear/writes after any earlier frame that read the (reused) depth
        // image — matches the color image's UNDEFINED→CLEAR reuse discipline.
        let depth_dep = vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS)
            .dst_stage_mask(vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS)
            .src_access_mask(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE);

        let create = |attachs: &[vk::AttachmentDescription],
                      subpass: &[vk::SubpassDescription],
                      deps: &[vk::SubpassDependency]|
         -> R<vk::RenderPass> {
            Ok(unsafe {
                self.device.create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(attachs)
                        .subpasses(subpass)
                        .dependencies(deps),
                    None,
                )?
            })
        };

        if with_depth {
            let attachs = [color, depth];
            let subpass = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color_ref)
                .depth_stencil_attachment(&depth_ref)];
            let deps = [color_dep, depth_dep];
            create(&attachs, &subpass, &deps)
        } else {
            let attachs = [color];
            let subpass = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color_ref)];
            let deps = [color_dep];
            create(&attachs, &subpass, &deps)
        }
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
        // Phase-2c: a push-constant range (VERTEX|FRAGMENT, offset 0) when the mesh carries a
        // transform block, and the texture descriptor-set layout (set 0) when it carries a texture,
        // else an empty layout (the built-in / no-transform path, unchanged).
        let pc_size = push_const_size_of(draw);
        let pc_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(pc_size)];
        let set_layouts: Vec<vk::DescriptorSetLayout> = match desc_sig_of(draw) {
            Some(sig) => vec![self.desc_set_layout(sig)?],
            None => Vec::new(),
        };
        let mut layout_ci = vk::PipelineLayoutCreateInfo::default();
        if pc_size > 0 {
            layout_ci = layout_ci.push_constant_ranges(&pc_ranges);
        }
        if !set_layouts.is_empty() {
            layout_ci = layout_ci.set_layouts(&set_layouts);
        }
        let layout = unsafe { dev.create_pipeline_layout(&layout_ci, None)? };
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
        // Phase-2d-A5: static rasterization + blend state from the mesh's `raster_flags` (cull mode,
        // front-face winding, alpha-blend enable). `0` reproduces the pre-A5 hardcoded default
        // (cull NONE / CCW / blend off), so bufferless / older draws are unchanged.
        let rf = raster_flags_of(draw);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(map_cull_mode(rf))
            .front_face(if rf & infinigpu_abi::wire::raster_flags::FRONT_FACE_CW != 0 {
                vk::FrontFace::CLOCKWISE
            } else {
                vk::FrontFace::COUNTER_CLOCKWISE
            })
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        // Standard src-alpha-over blending when requested; the source alpha the shader outputs drives
        // the composite (colour = src.rgb·src.a + dst.rgb·(1−src.a)). Off ⇒ opaque overwrite (default).
        let blend_on = rf & infinigpu_abi::wire::raster_flags::BLEND != 0;
        let blend_attach = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(blend_on)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attach);
        // Phase-2d depth-stencil state: present iff the mesh requested a depth attachment (test or
        // write). The pipeline's depth config MUST agree with the render pass's attachment presence.
        let depth_state = draw
            .geometry
            .as_ref()
            .and_then(|g| g.depth)
            .filter(|d| d.attachment_needed())
            .map(|d| {
                vk::PipelineDepthStencilStateCreateInfo::default()
                    .depth_test_enable(d.test)
                    .depth_write_enable(d.write)
                    .depth_compare_op(map_compare(d.compare))
                    .depth_bounds_test_enable(false)
                    .stencil_test_enable(false)
            });
        let mut pipeline_ci = vk::GraphicsPipelineCreateInfo::default()
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
        if let Some(ds) = depth_state.as_ref() {
            pipeline_ci = pipeline_ci.depth_stencil_state(ds);
        }
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
    ) -> R<(vk::RenderPass, vk::Pipeline, vk::PipelineLayout)> {
        let dev = &self.device;
        let topology = map_topology(draw.topology);
        let (with_depth, depth_key) = depth_key_of(draw);

        if !self.cache_enabled {
            let rp = self.build_render_pass(format, with_depth)?;
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
            return Ok((rp, pipe, layout));
        }

        let vs_hash = hash_spirv(draw.vertex_spirv);
        let fs_hash = hash_spirv(draw.fragment_spirv);
        let key = PipelineKey {
            vs_hash,
            fs_hash,
            topology: topology.as_raw(),
            format: format.as_raw(),
            vinput_hash: hash_vinput(draw),
            depth_key,
            pc_size: push_const_size_of(draw),
            desc_sig: desc_sig_of(draw),
            raster_flags: raster_flags_of(draw),
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
        let rp_key = (format.as_raw(), with_depth);
        let rp = match cache.render_passes.get(&rp_key) {
            Some(&rp) => rp,
            None => {
                let rp = self.build_render_pass(format, with_depth)?;
                cache.render_passes.insert(rp_key, rp);
                rp
            }
        };
        if let Some(cp) = cache.pipelines.get(&key).copied() {
            cache.hits += 1;
            return Ok((rp, cp.pipeline, cp.layout));
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
                Ok((rp, pipe, layout))
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
        let (render_pass, pipeline, layout) = self.pipeline_for(&mut throwaway, draw, FORMAT)?;
        // Phase-2d: a depth-testing mesh needs a scratch whose framebuffer has a depth attachment
        // matching the render pass. Key the scratch by depth so the two variants never collide.
        let (with_depth, _) = depth_key_of(draw);
        let sk = (width, height, with_depth);

        let mut cache = self.scratch_cache.lock().unwrap_or_else(|e| e.into_inner());
        if !cache.contains_key(&sk) {
            if cache.len() >= MAX_CACHED_SCRATCH {
                for (_, ss) in cache.drain() {
                    unsafe { ss.destroy(dev) };
                }
            }
            let ss = self.build_sized_scratch(width, height, render_pass, with_depth)?;
            cache.insert(sk, ss);
        }
        let ss = cache.get(&sk).expect("just inserted above");
        let t_setup = bd.map(|_| Instant::now());

        // Phase-2b/2c: upload the mesh + texture (if any) for this submit. Freed after the fence
        // completes below — the recorded draws reference them until then. A failure here is a clean
        // early return (the scratch is untouched; each upload frees its own partials).
        let geom_gpu = match draw.geometry.as_ref() {
            Some(g) if g.vertex_stride != 0 && !g.vertex_data.is_empty() => {
                Some(self.upload_geometry(g)?)
            }
            _ => None,
        };
        // Phase-2c: build descriptor set 0 (a UBO and/or a texture, composed into one set) for this
        // submit. A failure frees the already-uploaded mesh before bailing (no leak).
        let descs = match desc_sig_of(draw) {
            Some(sig) => {
                let g = draw.geometry.as_ref().expect("desc_sig_of ⇒ geometry present");
                let built = self
                    .desc_set_layout(sig)
                    .and_then(|sl| self.build_frame_descriptors(g.uniform.as_ref(), g.storage.as_ref(), g.textures, g.tex_binding, sl));
                match built {
                    Ok(d) => Some(d),
                    Err(e) => {
                        if let Some(gg) = geom_gpu {
                            unsafe { gg.destroy(dev) };
                        }
                        return Err(e);
                    }
                }
            }
            None => None,
        };

        // Single submit thread per device process + fence-wait before return ⇒ no frame N+1 vs. N
        // hazard on the reused objects; reset the pool + fence and re-record for this frame. The
        // record/submit/wait steps are fallible (device-lost / OOM); on any failure the per-submit
        // mesh + descriptors (their own VkBuffer/VkDeviceMemory/pool/image) must be freed before we
        // bail — they have no Drop impl (manual `destroy`), so an early `?` return would otherwise leak
        // them on the long-lived tenant-shared device. Fold the fallible steps into one result so a
        // single cleanup arm covers all three exits.
        let mut t_record = None;
        let submit_result: R<()> = (|| {
            self.record_forwarded_frame(
                ss,
                render_pass,
                pipeline,
                layout,
                width,
                height,
                bg,
                draw,
                geom_gpu.as_ref(),
                descs.as_ref(),
                ss.readback,
            )?;
            t_record = bd.map(|_| Instant::now());
            unsafe {
                dev.reset_fences(&[ss.fence])?;
                let cmds = [ss.cmd];
                let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
                dev.queue_submit(self.queue, &submit, ss.fence)?;
            }
            self.wait_fence(ss.fence)?;
            Ok(())
        })();
        // Whether the submit succeeded or failed, the fence is no longer pending on our objects (on
        // success it is signaled; on failure the submit never referenced them or the wait returned) ⇒
        // free the per-submit mesh + descriptors exactly once, then propagate any error.
        if let Some(gg) = geom_gpu {
            unsafe { gg.destroy(dev) };
        }
        if let Some(d) = descs {
            unsafe { d.destroy(dev) };
        }
        submit_result?;
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

    /// Phase-2c: the descriptor-set-0 layout for a [`DescriptorSig`], built dynamically from the present
    /// resources at their declared bindings (UBO@`ubo_binding` VERTEX|FRAGMENT; SAMPLED_IMAGE@`image_binding`
    /// + SAMPLER@`image_binding+1` FRAGMENT) and cached by signature. `DescriptorSig{None, Some(0)}` rebuilds
    /// the exact old fixed texture layout (image@0/sampler@1), so texture-only pipelines are unchanged.
    fn desc_set_layout(&self, sig: DescriptorSig) -> R<vk::DescriptorSetLayout> {
        let mut guard = self.desc_set_layouts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sl) = guard.get(&sig) {
            return Ok(*sl);
        }
        // Bound the cache: a well-behaved app uses a handful of signatures; an adversarial guest sending
        // endless unique (ubo_binding, image_binding) pairs must not grow this without limit. Evict-all
        // past the cap (mirrors the pipeline-cache posture) — layouts rebuild on demand.
        const MAX_DESC_SET_LAYOUTS: usize = 64;
        if guard.len() >= MAX_DESC_SET_LAYOUTS {
            for (_, sl) in guard.drain() {
                unsafe { self.device.destroy_descriptor_set_layout(sl, None) };
            }
        }
        let mut bindings: Vec<vk::DescriptorSetLayoutBinding> = Vec::new();
        if let Some(b) = sig.ubo_binding {
            bindings.push(
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT),
            );
        }
        if let Some(b) = sig.ssbo_binding {
            bindings.push(
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT),
            );
        }
        if let Some(base) = sig.tex_binding {
            // Texture i: sampled image @ base+2i, sampler @ base+2i+1 (FRAGMENT).
            for i in 0..sig.tex_count {
                let b = base + 2 * i;
                bindings.push(
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(b)
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::FRAGMENT),
                );
                bindings.push(
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(b + 1)
                        .descriptor_type(vk::DescriptorType::SAMPLER)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::FRAGMENT),
                );
            }
        }
        let sl = unsafe {
            self.device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )?
        };
        guard.insert(sig, sl);
        Ok(sl)
    }

    /// Phase-2c: upload `tex`'s RGBA8 pixels into a per-submit [`TextureGpu`] — a filled host-visible
    /// staging buffer, a device-local sampled image (still UNDEFINED; the record path copies + layout-
    /// transitions it), a view, and a sampler from `tex.sampler`. Writes the image@`image_binding` +
    /// sampler@`image_binding + 1` descriptors into the caller-provided `set` (which may also hold a
    /// UBO). Frees every partial allocation on any error. Destroyed after the submit fence.
    fn upload_texture_into(
        &self,
        tex: &Texture,
        set: vk::DescriptorSet,
        image_binding: u32,
    ) -> R<TextureGpu> {
        let dev = &self.device;
        let (w, h) = (tex.width, tex.height);
        let expected = (w as usize).checked_mul(h as usize).and_then(|p| p.checked_mul(4));
        if w == 0 || h == 0 || w > 16384 || h > 16384 || expected != Some(tex.rgba.len()) {
            return Err(format!("bad texture {w}x{h} ({} bytes)", tex.rgba.len()).into());
        }
        // Track partial allocations so any early return frees exactly what was built (no leak on the
        // long-lived shared device). The pool/set are the caller's (build_frame_descriptors) — not ours.
        let mut staging = vk::Buffer::null();
        let mut staging_mem = vk::DeviceMemory::null();
        let mut image = vk::Image::null();
        let mut image_mem = vk::DeviceMemory::null();
        let mut view = vk::ImageView::null();
        let mut sampler = vk::Sampler::null();
        let mut build = || -> R<TextureGpu> {
            unsafe {
                // Staging buffer (host-visible|coherent), filled with the pixels.
                staging = dev.create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(tex.rgba.len() as u64)
                        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )?;
                let sreq = dev.get_buffer_memory_requirements(staging);
                staging_mem = dev.allocate_memory(
                    &vk::MemoryAllocateInfo::default().allocation_size(sreq.size).memory_type_index(
                        self.find_mem(
                            sreq.memory_type_bits,
                            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                        )?,
                    ),
                    None,
                )?;
                dev.bind_buffer_memory(staging, staging_mem, 0)?;
                let ptr = dev.map_memory(staging_mem, 0, tex.rgba.len() as u64, vk::MemoryMapFlags::empty())?;
                std::ptr::copy_nonoverlapping(tex.rgba.as_ptr(), ptr as *mut u8, tex.rgba.len());
                dev.unmap_memory(staging_mem);

                // Device-local sampled image.
                image = dev.create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .extent(vk::Extent3D { width: w, height: h, depth: 1 })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )?;
                let ireq = dev.get_image_memory_requirements(image);
                image_mem = dev.allocate_memory(
                    &vk::MemoryAllocateInfo::default().allocation_size(ireq.size).memory_type_index(
                        self.find_mem(ireq.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?,
                    ),
                    None,
                )?;
                dev.bind_image_memory(image, image_mem, 0)?;
                view = dev.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .subresource_range(color_range()),
                    None,
                )?;
                let filter = if tex.sampler.linear { vk::Filter::LINEAR } else { vk::Filter::NEAREST };
                let addr = if tex.sampler.repeat {
                    vk::SamplerAddressMode::REPEAT
                } else {
                    vk::SamplerAddressMode::CLAMP_TO_EDGE
                };
                sampler = dev.create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(filter)
                        .min_filter(filter)
                        .address_mode_u(addr)
                        .address_mode_v(addr)
                        .address_mode_w(addr),
                    None,
                )?;

                // Write image@image_binding + sampler@image_binding+1 into the shared set.
                let img_info = [vk::DescriptorImageInfo::default()
                    .image_view(view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                let smp_info = [vk::DescriptorImageInfo::default().sampler(sampler)];
                let writes = [
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(image_binding)
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .image_info(&img_info),
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(image_binding + 1)
                        .descriptor_type(vk::DescriptorType::SAMPLER)
                        .image_info(&smp_info),
                ];
                dev.update_descriptor_sets(&writes, &[]);
                Ok(TextureGpu { staging, staging_mem, image, image_mem, view, sampler, width: w, height: h })
            }
        };
        build().map_err(|e| {
            // Free whatever was built (skip nulls), in dependency-safe order.
            unsafe {
                if sampler != vk::Sampler::null() {
                    dev.destroy_sampler(sampler, None);
                }
                if view != vk::ImageView::null() {
                    dev.destroy_image_view(view, None);
                }
                if image != vk::Image::null() {
                    dev.destroy_image(image, None);
                }
                if image_mem != vk::DeviceMemory::null() {
                    dev.free_memory(image_mem, None);
                }
                if staging != vk::Buffer::null() {
                    dev.destroy_buffer(staging, None);
                }
                if staging_mem != vk::DeviceMemory::null() {
                    dev.free_memory(staging_mem, None);
                }
            }
            e
        })
    }

    /// Phase-2c: build descriptor set 0 for one submit from the present resources — a UBO and/or a
    /// texture, composed into a SINGLE `VkDescriptorSet` at their declared bindings (what lets a real
    /// app use per-frame uniforms AND a texture at once). Creates a pool sized for the present types,
    /// allocates one set from `set_layout` (built for the matching [`DescriptorSig`]), uploads+writes
    /// the UBO (host-visible|coherent `UNIFORM_BUFFER` at `uniform.binding`) and/or the texture
    /// (image@`image_binding`, sampler@`image_binding + 1`). Frees partials on any error.
    fn build_frame_descriptors(
        &self,
        uniform: Option<&UniformBlock>,
        storage: Option<&StorageBlock>,
        textures: &[Texture],
        tex_base: u32,
        set_layout: vk::DescriptorSetLayout,
    ) -> R<FrameDescriptors> {
        let dev = &self.device;
        // Reject a degenerate/oversized UBO before allocating anything (mirrors the device decode cap).
        if let Some(u) = uniform {
            if u.bytes.is_empty() || u.bytes.len() > 65536 {
                return Err(format!("bad uniform block ({} bytes)", u.bytes.len()).into());
            }
        }
        // Same for the SSBO, but with the far-larger storage cap (mirrors the device decode).
        if let Some(s) = storage {
            if s.bytes.is_empty() || s.bytes.len() > MAX_SSBO_BYTES {
                return Err(format!("bad storage block ({} bytes)", s.bytes.len()).into());
            }
        }
        if textures.len() > MAX_TEXTURES {
            return Err(format!("too many textures ({} > {MAX_TEXTURES})", textures.len()).into());
        }
        let mut pool = vk::DescriptorPool::null();
        let mut ubo_buf = vk::Buffer::null();
        let mut ubo_mem = vk::DeviceMemory::null();
        let mut ssbo_buf = vk::Buffer::null();
        let mut ssbo_mem = vk::DeviceMemory::null();
        let mut build = || -> R<FrameDescriptors> {
            unsafe {
                let mut sizes: Vec<vk::DescriptorPoolSize> = Vec::new();
                if uniform.is_some() {
                    sizes.push(
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::UNIFORM_BUFFER)
                            .descriptor_count(1),
                    );
                }
                if storage.is_some() {
                    sizes.push(
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::STORAGE_BUFFER)
                            .descriptor_count(1),
                    );
                }
                if !textures.is_empty() {
                    sizes.push(
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::SAMPLED_IMAGE)
                            .descriptor_count(textures.len() as u32),
                    );
                    sizes.push(
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::SAMPLER)
                            .descriptor_count(textures.len() as u32),
                    );
                }
                pool = dev.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&sizes),
                    None,
                )?;
                let layouts = [set_layout];
                let set = dev.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default().descriptor_pool(pool).set_layouts(&layouts),
                )?[0];

                // UBO first (nothing after it can fail before the texture, which self-cleans on error).
                let ubo = if let Some(u) = uniform {
                    ubo_buf = dev.create_buffer(
                        &vk::BufferCreateInfo::default()
                            .size(u.bytes.len() as u64)
                            .usage(vk::BufferUsageFlags::UNIFORM_BUFFER)
                            .sharing_mode(vk::SharingMode::EXCLUSIVE),
                        None,
                    )?;
                    let req = dev.get_buffer_memory_requirements(ubo_buf);
                    ubo_mem = dev.allocate_memory(
                        &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(
                            self.find_mem(
                                req.memory_type_bits,
                                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                            )?,
                        ),
                        None,
                    )?;
                    dev.bind_buffer_memory(ubo_buf, ubo_mem, 0)?;
                    let ptr = dev.map_memory(ubo_mem, 0, u.bytes.len() as u64, vk::MemoryMapFlags::empty())?;
                    std::ptr::copy_nonoverlapping(u.bytes.as_ptr(), ptr as *mut u8, u.bytes.len());
                    dev.unmap_memory(ubo_mem);
                    let binfo = [vk::DescriptorBufferInfo::default()
                        .buffer(ubo_buf)
                        .offset(0)
                        .range(u.bytes.len() as u64)];
                    let writes = [vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(u.binding)
                        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                        .buffer_info(&binfo)];
                    dev.update_descriptor_sets(&writes, &[]);
                    Some(UniformGpu { buffer: ubo_buf, mem: ubo_mem })
                } else {
                    None
                };

                // SSBO next — identical to the UBO block but STORAGE_BUFFER, read-only, bound at offset 0
                // (sidesteps minStorageBufferOffsetAlignment). No barrier: host-coherent + read-only.
                let ssbo = if let Some(s) = storage {
                    ssbo_buf = dev.create_buffer(
                        &vk::BufferCreateInfo::default()
                            .size(s.bytes.len() as u64)
                            .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
                            .sharing_mode(vk::SharingMode::EXCLUSIVE),
                        None,
                    )?;
                    let req = dev.get_buffer_memory_requirements(ssbo_buf);
                    ssbo_mem = dev.allocate_memory(
                        &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(
                            self.find_mem(
                                req.memory_type_bits,
                                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                            )?,
                        ),
                        None,
                    )?;
                    dev.bind_buffer_memory(ssbo_buf, ssbo_mem, 0)?;
                    let ptr = dev.map_memory(ssbo_mem, 0, s.bytes.len() as u64, vk::MemoryMapFlags::empty())?;
                    std::ptr::copy_nonoverlapping(s.bytes.as_ptr(), ptr as *mut u8, s.bytes.len());
                    dev.unmap_memory(ssbo_mem);
                    let binfo = [vk::DescriptorBufferInfo::default()
                        .buffer(ssbo_buf)
                        .offset(0)
                        .range(s.bytes.len() as u64)];
                    let writes = [vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(s.binding)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(&binfo)];
                    dev.update_descriptor_sets(&writes, &[]);
                    Some(StorageGpu { buffer: ssbo_buf, mem: ssbo_mem })
                } else {
                    None
                };

                // Textures last — upload_texture_into writes each into the shared set at its binding and
                // self-cleans on error. Already-built TextureGpus are collected so an error mid-way frees
                // them (via the outer map_err path, which destroys the pool; the built textures are freed
                // here explicitly since they live only in this Vec until the FrameDescriptors is returned).
                let mut tgpu: Vec<TextureGpu> = Vec::with_capacity(textures.len());
                for (i, t) in textures.iter().enumerate() {
                    let binding = tex_base + 2 * i as u32;
                    match self.upload_texture_into(t, set, binding) {
                        Ok(g) => tgpu.push(g),
                        Err(e) => {
                            for g in &tgpu {
                                g.destroy(dev);
                            }
                            return Err(e);
                        }
                    }
                }

                Ok(FrameDescriptors { pool, set, ubo, ssbo, tex: tgpu })
            }
        };
        build().map_err(|e| {
            unsafe {
                if ubo_buf != vk::Buffer::null() {
                    dev.destroy_buffer(ubo_buf, None);
                }
                if ubo_mem != vk::DeviceMemory::null() {
                    dev.free_memory(ubo_mem, None);
                }
                if ssbo_buf != vk::Buffer::null() {
                    dev.destroy_buffer(ssbo_buf, None);
                }
                if ssbo_mem != vk::DeviceMemory::null() {
                    dev.free_memory(ssbo_mem, None);
                }
                if pool != vk::DescriptorPool::null() {
                    dev.destroy_descriptor_pool(pool, None); // frees the set
                }
            }
            e
        })
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
        layout: vk::PipelineLayout,
        width: u32,
        height: u32,
        bg: [f32; 4],
        draw: &ForwardedDraw,
        geom: Option<&GeometryGpu>,
        descs: Option<&FrameDescriptors>,
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
            // Phase-2c: upload the texture BEFORE the render pass — transition UNDEFINED→TRANSFER_DST,
            // copy staging→image, transition TRANSFER_DST→SHADER_READ_ONLY so the fragment shader can
            // sample it. Same command buffer as the render (one submit), so no extra sync needed. (The
            // UBO needs no barrier/copy — it is host-visible|coherent, written at descriptor-build time.)
            for t in descs.map(|d| d.tex.as_slice()).unwrap_or(&[]) {
                let to_dst = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .image(t.image)
                    .subresource_range(color_range());
                dev.cmd_pipeline_barrier(
                    ss.cmd,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_dst],
                );
                let region = vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(0)
                            .base_array_layer(0)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D { width: t.width, height: t.height, depth: 1 });
                dev.cmd_copy_buffer_to_image(
                    ss.cmd,
                    t.staging,
                    t.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
                let to_read = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
                    .image(t.image)
                    .subresource_range(color_range());
                dev.cmd_pipeline_barrier(
                    ss.cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_read],
                );
            }
            // Clear values are indexed by attachment: color at 0, and (Phase-2d) depth at 1 cleared
            // to the far plane (1.0) so LESS/LESS_OR_EQUAL admits the first fragment at every pixel.
            let color_clear = vk::ClearValue { color: vk::ClearColorValue { float32: bg } };
            let depth_clear = vk::ClearValue {
                depth_stencil: vk::ClearDepthStencilValue { depth: 1.0, stencil: 0 },
            };
            let clears_depth = [color_clear, depth_clear];
            let clears_color = [color_clear];
            let clears: &[vk::ClearValue] = if ss.has_depth { &clears_depth } else { &clears_color };
            dev.cmd_begin_render_pass(
                ss.cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(render_pass)
                    .framebuffer(ss.framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: vk::Extent2D { width, height },
                    })
                    .clear_values(clears),
                vk::SubpassContents::INLINE,
            );
            dev.cmd_bind_pipeline(ss.cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
            // Phase-2c: bind descriptor set 0 (UBO and/or texture, composed) so the shaders read them.
            if let Some(d) = descs {
                dev.cmd_bind_descriptor_sets(
                    ss.cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    layout,
                    0,
                    &[d.set],
                    &[],
                );
            }
            // Phase-2c: push the transform block (if any) before the draws. The layout's push-const
            // range (built in build_pipeline) matches these bytes at offset 0, VERTEX|FRAGMENT.
            let pc = draw.geometry.as_ref().map(|g| g.push_constants).unwrap_or(&[]);
            if !pc.is_empty() {
                dev.cmd_push_constants(
                    ss.cmd,
                    layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    pc,
                );
            }
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
                // A command list with explicit draws: either a real MESH (a vertex/index buffer was
                // uploaded — `geom` is Some) or a BUFFERLESS draw that pulls its vertices from a UBO by
                // gl_VertexIndex (vkcube — `geom` is None, but the draw list + UBO are present). Bind
                // the vertex/index buffer only for a mesh; either way replay each forwarded draw with
                // its own count + viewport (NOT the top-level `vertex_count`, which is 0 for a cmdlist).
                (_, Some(g)) if !g.draws.is_empty() => {
                    let indexed = match geom {
                        Some(gg) => {
                            dev.cmd_bind_vertex_buffers(ss.cmd, 0, &[gg.vbo], &[0]);
                            if let Some((ibo, _)) = gg.ibo {
                                let it = if g.index_u32 {
                                    vk::IndexType::UINT32
                                } else {
                                    vk::IndexType::UINT16
                                };
                                dev.cmd_bind_index_buffer(ss.cmd, ibo, 0, it);
                            }
                            gg.ibo.is_some()
                        }
                        None => false,
                    };
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
        let (render_pass, pipeline, layout) = self.pipeline_for(&mut throwaway, draw, FORMAT)?;

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

        // Zero-copy is the bufferless path (rejected geometry above), so never depth (key: false).
        let sk = (width, height, false);
        let mut cache = self.scratch_cache.lock().unwrap_or_else(|e| e.into_inner());
        if !cache.contains_key(&sk) {
            if cache.len() >= MAX_CACHED_SCRATCH {
                for (_, ss) in cache.drain() {
                    ss.destroy(&self.device);
                }
            }
            let ss = self.build_sized_scratch(width, height, render_pass, false)?;
            cache.insert(sk, ss);
        }
        let ss = cache.get(&sk).expect("just inserted above");

        self.record_forwarded_frame(ss, render_pass, pipeline, layout, width, height, bg, draw, None, None, dst)?;
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
        with_depth: bool,
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

        // Phase-2d: a matching depth attachment when the render pass has one. Device-local D32,
        // registered in the guard so an early `?` frees it. `views[1]` feeds the framebuffer.
        let (depth_image, depth_mem, depth_view) = if with_depth {
            let dimg = unsafe {
                dev.create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(DEPTH_FORMAT)
                        .extent(vk::Extent3D { width, height, depth: 1 })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )?
            };
            sc.depth_image = dimg;
            let dreq = unsafe { dev.get_image_memory_requirements(dimg) };
            let dmem = unsafe {
                dev.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(dreq.size)
                        .memory_type_index(
                            self.find_mem(dreq.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?,
                        ),
                    None,
                )?
            };
            sc.depth_mem = dmem;
            unsafe { dev.bind_image_memory(dimg, dmem, 0)? };
            let dview = unsafe {
                dev.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(dimg)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(DEPTH_FORMAT)
                        .subresource_range(depth_range()),
                    None,
                )?
            };
            sc.depth_view = dview;
            (dimg, dmem, dview)
        } else {
            (vk::Image::null(), vk::DeviceMemory::null(), vk::ImageView::null())
        };

        let views_depth = [view, depth_view];
        let views_color = [view];
        let views: &[vk::ImageView] = if with_depth { &views_depth } else { &views_color };
        let framebuffer = unsafe {
            dev.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(render_pass)
                    .attachments(views)
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
            has_depth: with_depth,
            depth_image,
            depth_mem,
            depth_view,
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
        let (render_pass, pipeline, _layout) = self.pipeline_for(&mut sc, draw, FORMAT)?;

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
            // Phase-2c: free every cached descriptor-set layout built for a DescriptorSig.
            let mut layouts = self.desc_set_layouts.lock().unwrap_or_else(|e| e.into_inner());
            for (_, sl) in layouts.drain() {
                self.device.destroy_descriptor_set_layout(sl, None);
            }
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

/// Subresource range for a depth attachment view (Phase-2d) — the DEPTH aspect of `DEPTH_FORMAT`.
fn depth_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::DEPTH)
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
            // IMMEDIATES = naga's name for push constants (`var<immediate>`); harmless for shaders
            // that don't use them, required for the Phase-2c transform test.
            naga::valid::Capabilities::IMMEDIATES,
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
                depth: None,
                push_constants: &[],
                textures: &[],
                tex_binding: 0,
                uniform: None,
                storage: None,
                raster_flags: 0,
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
                depth: None,
                push_constants: &[],
                textures: &[],
                tex_binding: 0,
                uniform: None,
                storage: None,
                raster_flags: 0,
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

    /// Phase-2d: depth testing does real hidden-surface removal. Two triangles occupy the SAME screen
    /// region at different depths — a NEAR red one (z=0.3) then a FAR green one (z=0.7), drawn in that
    /// order. Without a depth buffer the last draw wins (painter's order → green). With the depth test
    /// (`LESS` + write) the nearer red one wins despite being drawn first. Rendering both ways and
    /// asserting the centre pixel FLIPS red↔green proves the depth attachment, clear-to-far, per-pixel
    /// compare, and depth write all work — not just that a depth buffer is present.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_depth_test_resolves_occlusion() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        // Vertex shader reads a vec3 position (x, y, z) so each triangle carries its own NDC depth.
        let vs = compile_wgsl(
            "struct VOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec3<f32> };\n\
             @vertex fn main(@location(0) p: vec3<f32>, @location(1) c: vec3<f32>) -> VOut {\n\
               return VOut(vec4<f32>(p, 1.0), c);\n}",
            naga::ShaderStage::Vertex,
        );
        let fs = compile_wgsl(MESH_FS, naga::ShaderStage::Fragment);
        // stride = 6 f32 = 24 B: pos(vec3)@0, color(vec3)@12.
        use infinigpu_abi::wire::vk_vformat as V;
        let attrs = [
            VertexAttr { location: 0, format: V::R32G32B32_SFLOAT, offset: 0 },
            VertexAttr { location: 1, format: V::R32G32B32_SFLOAT, offset: 12 },
        ];
        // Same centered triangle twice, differing only in z and colour. Near = red (z=0.3),
        // far = green (z=0.7). One shared vertex buffer; two draws select their half via vertex_offset.
        let tri = |z: f32, c: [f32; 3]| {
            [
                [-0.7f32, 0.6, z, c[0], c[1], c[2]],
                [0.7, 0.6, z, c[0], c[1], c[2]],
                [0.0, -0.7, z, c[0], c[1], c[2]],
            ]
        };
        let mut vv = Vec::new();
        vv.extend_from_slice(&tri(0.3, [1.0, 0.0, 0.0])); // 0..2 near red
        vv.extend_from_slice(&tri(0.7, [0.0, 1.0, 0.0])); // 3..5 far green
        let verts: Vec<u8> = vv.iter().flat_map(|v| v.iter().flat_map(|f| f.to_le_bytes())).collect();
        // Draw NEAR (red) first, then FAR (green) — so painter's order (no depth) yields green.
        let draws = [
            DrawCmd { count: 3, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0; 4] },
            DrawCmd { count: 3, instance_count: 1, first: 3, vertex_offset: 0, viewport: [0.0; 4] },
        ];
        let (w, h) = (128u32, 128u32);
        let bg = [0.0, 0.0, 0.0, 1.0];

        let center = |depth: Option<DepthState>| -> [u8; 4] {
            let draw = ForwardedDraw {
                vertex_spirv: &vs,
                vertex_entry: c"main",
                fragment_spirv: &fs,
                fragment_entry: c"main",
                vertex_count: 0,
                topology: 0,
                geometry: Some(Geometry {
                    vertex_data: &verts,
                    vertex_stride: 24,
                    attrs: &attrs,
                    index_data: &[],
                    index_u32: false,
                    draws: &draws,
                    depth,
                    push_constants: &[],
                    textures: &[],
                    tex_binding: 0,
                    uniform: None,
                    storage: None,
                    raster_flags: 0,
                }),
            };
            gpu.render_forwarded(w, h, bg, &draw).expect("depth render").pixel(w / 2, h / 2)
        };

        // No depth: the last draw (far green) paints over the near red → green wins.
        let no_depth = center(None);
        assert!(
            no_depth[1] > 150 && no_depth[1] > no_depth[0] + 60,
            "without depth the last-drawn far triangle must win (green), got {no_depth:?}"
        );
        // Depth LESS + write: the near red triangle wins despite being drawn first.
        let with_depth = center(Some(DepthState {
            test: true,
            write: true,
            compare: infinigpu_abi::wire::depth_compare::LESS,
        }));
        assert!(
            with_depth[0] > 150 && with_depth[0] > with_depth[1] + 60,
            "with depth the nearer triangle must win (red), got {with_depth:?}"
        );
        eprintln!("forwarded_depth_test_resolves_occlusion: OK (no_depth={no_depth:?} with_depth={with_depth:?})");
    }

    /// Phase-2c: a push-constant transform (an MVP `mat4`) actually moves geometry. A small triangle
    /// sits at the NDC origin; the vertex shader multiplies its position by a push-constant matrix.
    /// With the IDENTITY matrix it renders at the centre; with a translation `(+0.5, +0.5)` it moves
    /// to the lower-right quadrant. Asserting the lit region FOLLOWS the matrix (centre lit ↔ quadrant
    /// bg, and vice-versa) proves the pipeline-layout push-const range, `cmd_push_constants`, and the
    /// column-major `mat4` layout all work — the gate that lets a real app use a camera/model transform
    /// instead of raw NDC.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_push_constant_transform_moves_geometry() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        // Vertex shader transforms pos(vec2) by a push-constant mat4 (naga: var<immediate>).
        let vs = compile_wgsl(
            "struct PC { mvp: mat4x4<f32> };\n\
             var<immediate> pc: PC;\n\
             struct VOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec3<f32> };\n\
             @vertex fn main(@location(0) p: vec2<f32>, @location(1) c: vec3<f32>) -> VOut {\n\
               return VOut(pc.mvp * vec4<f32>(p, 0.0, 1.0), c);\n}",
            naga::ShaderStage::Vertex,
        );
        let fs = compile_wgsl(MESH_FS, naga::ShaderStage::Fragment);
        // Small triangle around the origin (spans ~±0.25), pure white so any lit pixel is easy to find.
        let verts = pack_verts(&[
            [-0.25, 0.25, 1.0, 1.0, 1.0],
            [0.25, 0.25, 1.0, 1.0, 1.0],
            [0.0, -0.25, 1.0, 1.0, 1.0],
        ]);
        let attrs = mesh_attrs();
        let draws = [DrawCmd { count: 3, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0; 4] }];
        // Column-major mat4: columns are contiguous. Translation puts (tx,ty,tz) in the 4th column.
        let mat = |tx: f32, ty: f32| -> [u8; 64] {
            let m: [f32; 16] = [
                1.0, 0.0, 0.0, 0.0, // col 0
                0.0, 1.0, 0.0, 0.0, // col 1
                0.0, 0.0, 1.0, 0.0, // col 2
                tx, ty, 0.0, 1.0, // col 3 (translation)
            ];
            let mut b = [0u8; 64];
            for (i, f) in m.iter().enumerate() {
                b[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
            }
            b
        };
        let (w, h) = (128u32, 128u32);
        let bg = [0.0, 0.0, 0.0, 1.0];
        // Centroid (mean x, mean y) of the lit pixels + the lit count — orientation-agnostic (x is
        // unambiguous; the y-axis handedness of the readback is not, so we assert x precisely and only
        // that y moved by roughly the expected magnitude).
        let centroid = |f: &Frame| -> (f32, f32, u32) {
            let (mut sx, mut sy, mut n) = (0u64, 0u64, 0u32);
            for y in 0..h {
                for x in 0..w {
                    let p = f.pixel(x, y);
                    if p[0] as u16 + p[1] as u16 + p[2] as u16 > 200 {
                        sx += x as u64;
                        sy += y as u64;
                        n += 1;
                    }
                }
            }
            assert!(n > 30, "transform render must produce a visible triangle (lit={n})");
            (sx as f32 / n as f32, sy as f32 / n as f32, n)
        };
        let render = |m: &[u8]| {
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
                    depth: None,
                    push_constants: m,
                    textures: &[],
                    tex_binding: 0,
                    uniform: None,
                    storage: None,
                    raster_flags: 0,
                }),
            };
            gpu.render_forwarded(w, h, bg, &draw).expect("transform render")
        };

        // Identity: the triangle sits at the centre of the frame.
        let (ix, iy, _) = centroid(&render(&mat(0.0, 0.0)));
        assert!(
            (ix - w as f32 / 2.0).abs() < 8.0 && (iy - h as f32 / 2.0).abs() < 8.0,
            "identity transform must render at the centre, got ({ix:.1},{iy:.1})"
        );
        // Translate (+0.5,+0.5): NDC x=0.5 → pixel 0.75·w. The centroid must shift right by ~0.25·w,
        // and y must move by a comparable magnitude (direction depends on readback handedness).
        let (tx, ty, _) = centroid(&render(&mat(0.5, 0.5)));
        let dx = tx - ix;
        let dy = (ty - iy).abs();
        assert!(
            (dx - w as f32 * 0.25).abs() < 12.0,
            "translation must shift the triangle right by ~0.25·w ({} px), got dx={dx:.1}",
            w as f32 * 0.25
        );
        assert!(
            (dy - h as f32 * 0.25).abs() < 12.0,
            "translation must shift the triangle vertically by ~0.25·h, got dy={dy:.1}"
        );
        eprintln!(
            "forwarded_push_constant_transform_moves_geometry: OK (identity@({ix:.0},{iy:.0}) → translate shifted dx={dx:.0} dy={dy:.0})"
        );
    }

    /// Phase-2c: a **sampled texture** actually renders. A fullscreen quad with UVs samples a 2×2
    /// four-colour texture (red / green / blue / white) through a descriptor set (image@0, sampler@1)
    /// with nearest filtering, so each screen quadrant shows one texel. Asserting all four distinct
    /// texel colours appear proves the whole path: staging upload, the UNDEFINED→TRANSFER_DST→
    /// SHADER_READ_ONLY layout transitions, the sampler, the descriptor-set layout/pool/set + bind,
    /// and `textureSample` reading the interpolated UV attribute. This is the gate for textured apps.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_texture_samples_onto_a_quad() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let vs = compile_wgsl(
            "struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };\n\
             @vertex fn main(@location(0) p: vec2<f32>, @location(1) uv: vec2<f32>) -> VOut {\n\
               return VOut(vec4<f32>(p, 0.0, 1.0), uv);\n}",
            naga::ShaderStage::Vertex,
        );
        let fs = compile_wgsl(
            "@group(0) @binding(0) var tex: texture_2d<f32>;\n\
             @group(0) @binding(1) var samp: sampler;\n\
             @fragment fn main(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {\n\
               return textureSample(tex, samp, uv);\n}",
            naga::ShaderStage::Fragment,
        );
        // Fullscreen quad: pos(vec2)@0 + uv(vec2)@8, stride 16.
        use infinigpu_abi::wire::vk_vformat as V;
        let attrs = [
            VertexAttr { location: 0, format: V::R32G32_SFLOAT, offset: 0 },
            VertexAttr { location: 1, format: V::R32G32_SFLOAT, offset: 8 },
        ];
        let quad: [[f32; 4]; 4] = [
            [-1.0, -1.0, 0.0, 0.0],
            [1.0, -1.0, 1.0, 0.0],
            [1.0, 1.0, 1.0, 1.0],
            [-1.0, 1.0, 0.0, 1.0],
        ];
        let verts: Vec<u8> = quad.iter().flat_map(|v| v.iter().flat_map(|f| f.to_le_bytes())).collect();
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let index_data: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
        // 2×2 RGBA8: red, green / blue, white.
        let tex_px: [u8; 16] = [
            255, 0, 0, 255, 0, 255, 0, 255, // row 0: red, green
            0, 0, 255, 255, 255, 255, 255, 255, // row 1: blue, white
        ];
        let draws = [DrawCmd { count: 6, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0; 4] }];
        let draw = ForwardedDraw {
            vertex_spirv: &vs,
            vertex_entry: c"main",
            fragment_spirv: &fs,
            fragment_entry: c"main",
            vertex_count: 0,
            topology: 0,
            geometry: Some(Geometry {
                vertex_data: &verts,
                vertex_stride: 16,
                attrs: &attrs,
                index_data: &index_data,
                index_u32: false,
                draws: &draws,
                depth: None,
                push_constants: &[],
                textures: &[Texture {
                    width: 2,
                    height: 2,
                    rgba: &tex_px,
                    sampler: SamplerCfg { linear: false, repeat: false },
                }],
                tex_binding: 0,
                uniform: None,
                storage: None,
                raster_flags: 0,
            }),
        };
        let (w, h) = (128u32, 128u32);
        let bg = [0.0, 0.0, 0.0, 1.0];
        let frame = gpu.render_forwarded(w, h, bg, &draw).expect("textured render");

        // All four texel colours must appear (each texel maps to a screen quadrant under nearest).
        let (mut red, mut green, mut blue, mut white) = (0u32, 0u32, 0u32, 0u32);
        for p in frame.rgba.chunks_exact(4) {
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            red += (r > 180 && g < 60 && b < 60) as u32;
            green += (g > 180 && r < 60 && b < 60) as u32;
            blue += (b > 180 && r < 60 && g < 60) as u32;
            white += (r > 180 && g > 180 && b > 180) as u32;
        }
        let quad_px = (w * h / 4) as u32;
        for (name, n) in [("red", red), ("green", green), ("blue", blue), ("white", white)] {
            assert!(
                n > quad_px / 2,
                "texel colour {name} must fill ~a quadrant ({n}/{quad_px}); texture not sampled?"
            );
        }
        eprintln!("forwarded_texture_samples_onto_a_quad: OK (r={red} g={green} b={blue} w={white})");
    }

    /// Phase-2c UBO: a `var<uniform>` block reaches the VERTEX stage — the shader offsets each vertex
    /// by the UBO's xy, so a +0.5 NDC x-offset moves the triangle right by ~0.25·w vs a zero offset.
    /// Proves the host builds a UNIFORM_BUFFER descriptor (VERTEX|FRAGMENT), uploads the bytes, and
    /// binds it — the piece a real game needs for per-frame matrices.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_uniform_only_offsets_geometry() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let vs = compile_wgsl(
            "@group(0) @binding(0) var<uniform> u: vec4<f32>;\n\
             struct VOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec3<f32> };\n\
             @vertex fn main(@location(0) p: vec2<f32>, @location(1) c: vec3<f32>) -> VOut {\n\
               return VOut(vec4<f32>(p + u.xy, 0.0, 1.0), c);\n}",
            naga::ShaderStage::Vertex,
        );
        let fs = compile_wgsl(
            "@fragment fn main(@location(0) c: vec3<f32>) -> @location(0) vec4<f32> {\n\
               return vec4<f32>(c, 1.0);\n}",
            naga::ShaderStage::Fragment,
        );
        use infinigpu_abi::wire::vk_vformat as V;
        let attrs = [
            VertexAttr { location: 0, format: V::R32G32_SFLOAT, offset: 0 },
            VertexAttr { location: 1, format: V::R32G32B32_SFLOAT, offset: 8 },
        ];
        // A small red triangle (pos vec2 + color vec3), stride 20.
        let tri: [[f32; 5]; 3] =
            [[0.0, -0.3, 1.0, 0.0, 0.0], [-0.3, 0.3, 1.0, 0.0, 0.0], [0.3, 0.3, 1.0, 0.0, 0.0]];
        let verts: Vec<u8> = tri.iter().flat_map(|v| v.iter().flat_map(|f| f.to_le_bytes())).collect();
        let draws = [DrawCmd { count: 3, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0; 4] }];
        let (w, h) = (128u32, 128u32);
        let bg = [0.0, 0.0, 0.0, 1.0];

        let centroid_x = |off: [f32; 4]| -> f32 {
            let ubo: Vec<u8> = off.iter().flat_map(|f| f.to_le_bytes()).collect();
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
                    depth: None,
                    push_constants: &[],
                    textures: &[],
                    tex_binding: 0,
                    uniform: Some(UniformBlock { binding: 0, bytes: &ubo }),
                    storage: None,
                    raster_flags: 0,
                }),
            };
            let frame = gpu.render_forwarded(w, h, bg, &draw).expect("uniform render");
            let (mut sum, mut n) = (0f64, 0f64);
            for (i, p) in frame.rgba.chunks_exact(4).enumerate() {
                if p[0] > 128 {
                    sum += (i as u32 % w) as f64;
                    n += 1.0;
                }
            }
            assert!(n > 50.0, "the triangle should light many pixels ({n})");
            (sum / n) as f32
        };
        let cx0 = centroid_x([0.0, 0.0, 0.0, 0.0]);
        let cx_right = centroid_x([0.5, 0.0, 0.0, 0.0]);
        let shift = cx_right - cx0;
        let expected = 0.25 * w as f32; // +0.5 NDC = a quarter of the width in pixels
        assert!(
            (shift - expected).abs() < w as f32 * 0.06,
            "UBO x-offset must move the triangle right ~{expected}px; got {shift}px \
             (cx0={cx0}, cxR={cx_right}) — the UBO didn't reach the vertex stage"
        );
        eprintln!("forwarded_uniform_only_offsets_geometry: OK (shift={shift}px ~ {expected}px)");
    }

    /// Phase-2c SSBO: a read-only `var<storage>` block reaches the VERTEX stage — the shader offsets each
    /// vertex by the SSBO's xy, so a +0.5 NDC x-offset moves the triangle right by ~0.25·w vs a zero
    /// offset. Proves the host builds a STORAGE_BUFFER descriptor (VERTEX|FRAGMENT), uploads the bytes,
    /// and binds it — the piece a DXVK structured/raw SRV (or a skinning palette) needs.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_storage_only_offsets_geometry() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let vs = compile_wgsl(
            "@group(0) @binding(0) var<storage, read> s: vec4<f32>;\n\
             struct VOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec3<f32> };\n\
             @vertex fn main(@location(0) p: vec2<f32>, @location(1) c: vec3<f32>) -> VOut {\n\
               return VOut(vec4<f32>(p + s.xy, 0.0, 1.0), c);\n}",
            naga::ShaderStage::Vertex,
        );
        let fs = compile_wgsl(
            "@fragment fn main(@location(0) c: vec3<f32>) -> @location(0) vec4<f32> {\n\
               return vec4<f32>(c, 1.0);\n}",
            naga::ShaderStage::Fragment,
        );
        use infinigpu_abi::wire::vk_vformat as V;
        let attrs = [
            VertexAttr { location: 0, format: V::R32G32_SFLOAT, offset: 0 },
            VertexAttr { location: 1, format: V::R32G32B32_SFLOAT, offset: 8 },
        ];
        let tri: [[f32; 5]; 3] =
            [[0.0, -0.3, 1.0, 0.0, 0.0], [-0.3, 0.3, 1.0, 0.0, 0.0], [0.3, 0.3, 1.0, 0.0, 0.0]];
        let verts: Vec<u8> = tri.iter().flat_map(|v| v.iter().flat_map(|f| f.to_le_bytes())).collect();
        let draws = [DrawCmd { count: 3, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0; 4] }];
        let (w, h) = (128u32, 128u32);
        let bg = [0.0, 0.0, 0.0, 1.0];

        let centroid_x = |off: [f32; 4]| -> f32 {
            let ssbo: Vec<u8> = off.iter().flat_map(|f| f.to_le_bytes()).collect();
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
                    depth: None,
                    push_constants: &[],
                    textures: &[],
                    tex_binding: 0,
                    uniform: None,
                    storage: Some(StorageBlock { binding: 0, bytes: &ssbo }),
                    raster_flags: 0,
                }),
            };
            let frame = gpu.render_forwarded(w, h, bg, &draw).expect("storage render");
            let (mut sum, mut n) = (0f64, 0f64);
            for (i, p) in frame.rgba.chunks_exact(4).enumerate() {
                if p[0] > 128 {
                    sum += (i as u32 % w) as f64;
                    n += 1.0;
                }
            }
            assert!(n > 50.0, "the triangle should light many pixels ({n})");
            (sum / n) as f32
        };
        let cx0 = centroid_x([0.0, 0.0, 0.0, 0.0]);
        let cx_right = centroid_x([0.5, 0.0, 0.0, 0.0]);
        let shift = cx_right - cx0;
        let expected = 0.25 * w as f32;
        assert!(
            (shift - expected).abs() < w as f32 * 0.06,
            "SSBO x-offset must move the triangle right ~{expected}px; got {shift}px \
             (cx0={cx0}, cxR={cx_right}) — the SSBO didn't reach the vertex stage"
        );
        eprintln!("forwarded_storage_only_offsets_geometry: OK (shift={shift}px ~ {expected}px)");
    }

    /// Phase-2c composition: a UBO **and** an SSBO — two DIFFERENT buffer descriptor types — in ONE set at
    /// distinct bindings. The VS offsets x by the UBO (binding 0) and y by the SSBO (binding 1); asserting
    /// BOTH shifts proves the host's dynamic descriptor-set-0 layout mixes a UNIFORM_BUFFER and a
    /// STORAGE_BUFFER correctly (what a real DXVK draw with both a constant buffer and a structured SRV needs).
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_uniform_and_storage_compose() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let vs = compile_wgsl(
            "@group(0) @binding(0) var<uniform> u: vec4<f32>;\n\
             @group(0) @binding(1) var<storage, read> s: vec4<f32>;\n\
             struct VOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec3<f32> };\n\
             @vertex fn main(@location(0) p: vec2<f32>, @location(1) c: vec3<f32>) -> VOut {\n\
               return VOut(vec4<f32>(p.x + u.x, p.y + s.y, 0.0, 1.0), c);\n}",
            naga::ShaderStage::Vertex,
        );
        let fs = compile_wgsl(
            "@fragment fn main(@location(0) c: vec3<f32>) -> @location(0) vec4<f32> {\n\
               return vec4<f32>(c, 1.0);\n}",
            naga::ShaderStage::Fragment,
        );
        use infinigpu_abi::wire::vk_vformat as V;
        let attrs = [
            VertexAttr { location: 0, format: V::R32G32_SFLOAT, offset: 0 },
            VertexAttr { location: 1, format: V::R32G32B32_SFLOAT, offset: 8 },
        ];
        let tri: [[f32; 5]; 3] =
            [[0.0, -0.3, 1.0, 0.0, 0.0], [-0.3, 0.3, 1.0, 0.0, 0.0], [0.3, 0.3, 1.0, 0.0, 0.0]];
        let verts: Vec<u8> = tri.iter().flat_map(|v| v.iter().flat_map(|f| f.to_le_bytes())).collect();
        let draws = [DrawCmd { count: 3, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0; 4] }];
        let (w, h) = (128u32, 128u32);
        let bg = [0.0, 0.0, 0.0, 1.0];

        // Returns the (x, y) centroid of the lit triangle for a given UBO x-offset + SSBO y-offset.
        let centroid = |ux: f32, sy: f32| -> (f32, f32) {
            let ubo: Vec<u8> = [ux, 0.0, 0.0, 0.0].iter().flat_map(|f| f.to_le_bytes()).collect();
            let ssbo: Vec<u8> = [0.0, sy, 0.0, 0.0].iter().flat_map(|f| f.to_le_bytes()).collect();
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
                    depth: None,
                    push_constants: &[],
                    textures: &[],
                    tex_binding: 0,
                    uniform: Some(UniformBlock { binding: 0, bytes: &ubo }),
                    storage: Some(StorageBlock { binding: 1, bytes: &ssbo }),
                    raster_flags: 0,
                }),
            };
            let frame = gpu.render_forwarded(w, h, bg, &draw).expect("compose render");
            let (mut sx, mut sy2, mut n) = (0f64, 0f64, 0f64);
            for (i, p) in frame.rgba.chunks_exact(4).enumerate() {
                if p[0] > 128 {
                    sx += (i as u32 % w) as f64;
                    sy2 += (i as u32 / w) as f64;
                    n += 1.0;
                }
            }
            assert!(n > 50.0, "the triangle should light many pixels ({n})");
            ((sx / n) as f32, (sy2 / n) as f32)
        };
        let (cx0, cy0) = centroid(0.0, 0.0);
        let (cx_r, cy_v) = centroid(0.5, 0.5); // UBO shifts +x (right); SSBO shifts y (the host y-flips NDC)
        let dx = cx_r - cx0;
        let dy = cy_v - cy0;
        let expected = 0.25 * w as f32;
        // UBO drives x → a definite rightward shift (same convention as the UBO-only test).
        assert!(
            (dx - expected).abs() < w as f32 * 0.06,
            "UBO x-offset must shift the triangle right ~{expected}px; got {dx}px — UBO half of the set failed"
        );
        // SSBO drives y → a shift of the right MAGNITUDE (direction depends on the host's y-flip, which is
        // orthogonal to this test — what matters is the SSBO's bytes reached the vertex stage).
        assert!(
            (dy.abs() - expected).abs() < h as f32 * 0.06,
            "SSBO y-offset must shift the triangle ~{expected}px vertically; got {dy}px — SSBO half of the set failed"
        );
        eprintln!("forwarded_uniform_and_storage_compose: OK (dx={dx}px dy={dy}px ~ {expected}px)");
    }

    /// Phase-2c composition: a UBO **and** a texture in ONE descriptor set at distinct bindings — the
    /// gate for real textured, transformed apps. The VS offsets by a UBO (binding 0); the FS samples a
    /// texture (binding 1) + sampler (binding 2). Asserts BOTH work at once: all four texel colours
    /// appear (texture composed) AND a +0.5 x-offset shifts the textured quad right (UBO reached VERTEX).
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_uniform_and_texture_compose() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let vs = compile_wgsl(
            "@group(0) @binding(0) var<uniform> u: vec4<f32>;\n\
             struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };\n\
             @vertex fn main(@location(0) p: vec2<f32>, @location(1) uv: vec2<f32>) -> VOut {\n\
               return VOut(vec4<f32>(p + u.xy, 0.0, 1.0), uv);\n}",
            naga::ShaderStage::Vertex,
        );
        let fs = compile_wgsl(
            "@group(0) @binding(1) var tex: texture_2d<f32>;\n\
             @group(0) @binding(2) var samp: sampler;\n\
             @fragment fn main(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {\n\
               return textureSample(tex, samp, uv);\n}",
            naga::ShaderStage::Fragment,
        );
        use infinigpu_abi::wire::vk_vformat as V;
        let attrs = [
            VertexAttr { location: 0, format: V::R32G32_SFLOAT, offset: 0 },
            VertexAttr { location: 1, format: V::R32G32_SFLOAT, offset: 8 },
        ];
        let quad: [[f32; 4]; 4] =
            [[-1.0, -1.0, 0.0, 0.0], [1.0, -1.0, 1.0, 0.0], [1.0, 1.0, 1.0, 1.0], [-1.0, 1.0, 0.0, 1.0]];
        let verts: Vec<u8> = quad.iter().flat_map(|v| v.iter().flat_map(|f| f.to_le_bytes())).collect();
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let index_data: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
        let tex_px: [u8; 16] = [
            255, 0, 0, 255, 0, 255, 0, 255, // red, green
            0, 0, 255, 255, 255, 255, 255, 255, // blue, white
        ];
        let draws = [DrawCmd { count: 6, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0; 4] }];
        let (w, h) = (128u32, 128u32);
        let bg = [0.0, 0.0, 0.0, 1.0];

        let render = |off: [f32; 4]| -> Frame {
            let ubo: Vec<u8> = off.iter().flat_map(|f| f.to_le_bytes()).collect();
            let draw = ForwardedDraw {
                vertex_spirv: &vs,
                vertex_entry: c"main",
                fragment_spirv: &fs,
                fragment_entry: c"main",
                vertex_count: 0,
                topology: 0,
                geometry: Some(Geometry {
                    vertex_data: &verts,
                    vertex_stride: 16,
                    attrs: &attrs,
                    index_data: &index_data,
                    index_u32: false,
                    draws: &draws,
                    depth: None,
                    push_constants: &[],
                    textures: &[Texture {
                        width: 2,
                        height: 2,
                        rgba: &tex_px,
                        sampler: SamplerCfg { linear: false, repeat: false },
                    }],
                    tex_binding: 1, // image@1, sampler@2 — composes with the UBO@0
                    uniform: Some(UniformBlock { binding: 0, bytes: &ubo }),
                    storage: None,
                    raster_flags: 0,
                }),
            };
            gpu.render_forwarded(w, h, bg, &draw).expect("composed render")
        };
        let centroid_x = |f: &Frame| -> f32 {
            let (mut sum, mut n) = (0f64, 0f64);
            for (i, p) in f.rgba.chunks_exact(4).enumerate() {
                // Any non-background (textured) pixel.
                if p[0] as u16 + p[1] as u16 + p[2] as u16 > 40 {
                    sum += (i as u32 % w) as f64;
                    n += 1.0;
                }
            }
            (sum / n.max(1.0)) as f32
        };

        let ctrl = render([0.0, 0.0, 0.0, 0.0]);
        // Texture composed with the UBO: all four texel colours appear in the un-offset render.
        let (mut red, mut green, mut blue, mut white) = (0u32, 0u32, 0u32, 0u32);
        for p in ctrl.rgba.chunks_exact(4) {
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            red += (r > 180 && g < 60 && b < 60) as u32;
            green += (g > 180 && r < 60 && b < 60) as u32;
            blue += (b > 180 && r < 60 && g < 60) as u32;
            white += (r > 180 && g > 180 && b > 180) as u32;
        }
        let quad_px = (w * h / 4) as u32;
        for (name, n) in [("red", red), ("green", green), ("blue", blue), ("white", white)] {
            assert!(
                n > quad_px / 2,
                "texel {name} must fill ~a quadrant ({n}/{quad_px}) — the texture did not compose with the UBO"
            );
        }
        // UBO reached the VERTEX stage while a texture was bound: the textured region shifts right.
        let shifted = render([0.5, 0.0, 0.0, 0.0]);
        let shift = centroid_x(&shifted) - centroid_x(&ctrl);
        assert!(
            shift > w as f32 * 0.10,
            "UBO x-offset must shift the textured quad right; got {shift}px — the UBO didn't reach the \
             vertex stage while a texture was bound in the same set"
        );
        eprintln!(
            "forwarded_uniform_and_texture_compose: OK (shift={shift}px, colours r={red} g={green} b={blue} w={white})"
        );
    }

    /// Phase-2d-A5 cull: `raster_flags` cull mode + front-face actually reach the GPU pipeline. The
    /// SAME triangle is rendered three ways — cull NONE (control), cull BACK with CCW-front, cull BACK
    /// with CW-front. Whatever the triangle's winding, exactly ONE of the two BACK variants treats it
    /// as a back face and culls it (renders empty) while the other renders it like the control. Without
    /// this, solid closed meshes over-draw their hidden back faces. Winding-agnostic by construction.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_back_face_culling_removes_geometry() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let vs = compile_wgsl(
            "@vertex fn main(@location(0) p: vec2<f32>) -> @builtin(position) vec4<f32> {\n\
               return vec4<f32>(p, 0.0, 1.0);\n}",
            naga::ShaderStage::Vertex,
        );
        let fs = compile_wgsl(
            "@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(1.0, 0.0, 0.0, 1.0); }",
            naga::ShaderStage::Fragment,
        );
        use infinigpu_abi::wire::{cull_mode, raster_flags, vk_vformat as V};
        let attrs = [VertexAttr { location: 0, format: V::R32G32_SFLOAT, offset: 0 }];
        let tri: [[f32; 2]; 3] = [[0.0, -0.6], [-0.6, 0.6], [0.6, 0.6]];
        let verts: Vec<u8> = tri.iter().flat_map(|v| v.iter().flat_map(|f| f.to_le_bytes())).collect();
        let draws = [DrawCmd { count: 3, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0; 4] }];
        let (w, h) = (128u32, 128u32);
        let bg = [0.0, 0.0, 0.0, 1.0];

        let lit = |rf: u32| -> u32 {
            let draw = ForwardedDraw {
                vertex_spirv: &vs,
                vertex_entry: c"main",
                fragment_spirv: &fs,
                fragment_entry: c"main",
                vertex_count: 0,
                topology: 0,
                geometry: Some(Geometry {
                    vertex_data: &verts,
                    vertex_stride: 8,
                    attrs: &attrs,
                    index_data: &[],
                    index_u32: false,
                    draws: &draws,
                    depth: None,
                    push_constants: &[],
                    textures: &[],
                    tex_binding: 0,
                    uniform: None,
                    storage: None,
                    raster_flags: rf,
                }),
            };
            let frame = gpu.render_forwarded(w, h, bg, &draw).expect("cull render");
            frame.rgba.chunks_exact(4).filter(|p| p[0] > 128).count() as u32
        };
        let ctrl = lit(0); // cull NONE
        let back_ccw = lit(raster_flags::pack(cull_mode::BACK, false, false));
        let back_cw = lit(raster_flags::pack(cull_mode::BACK, true, false));
        assert!(ctrl > 500, "control (cull NONE) must render the triangle ({ctrl} px)");
        // Exactly one BACK variant culls (≈0), the other renders (≈ctrl). front_face selects which.
        let culled = back_ccw.min(back_cw);
        let kept = back_ccw.max(back_cw);
        assert!(culled < ctrl / 20, "one winding must be culled to ~empty (got {culled} px)");
        assert!(kept > ctrl / 2, "the other winding must still render (got {kept} px)");
        eprintln!(
            "forwarded_back_face_culling_removes_geometry: OK (none={ctrl}, back+ccw={back_ccw}, back+cw={back_cw})"
        );
    }

    /// Phase-2d-A5 blend: the `BLEND` `raster_flags` bit enables standard src-alpha-over compositing.
    /// A triangle whose fragment alpha is 0.5 is drawn over a solid red background. Blend OFF ⇒ the
    /// triangle's colour opaquely overwrites red; blend ON ⇒ the covered pixels are a 50/50 composite
    /// of the triangle colour and the red background. Without this, transparent surfaces render opaque.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_alpha_blend_composites_over_background() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let vs = compile_wgsl(
            "@vertex fn main(@location(0) p: vec2<f32>) -> @builtin(position) vec4<f32> {\n\
               return vec4<f32>(p, 0.0, 1.0);\n}",
            naga::ShaderStage::Vertex,
        );
        // Fragment: blue at half alpha.
        let fs = compile_wgsl(
            "@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(0.0, 0.0, 1.0, 0.5); }",
            naga::ShaderStage::Fragment,
        );
        use infinigpu_abi::wire::{raster_flags, vk_vformat as V};
        let attrs = [VertexAttr { location: 0, format: V::R32G32_SFLOAT, offset: 0 }];
        // A big centred triangle so the centre pixel is definitely covered.
        let tri: [[f32; 2]; 3] = [[0.0, -0.8], [-0.8, 0.8], [0.8, 0.8]];
        let verts: Vec<u8> = tri.iter().flat_map(|v| v.iter().flat_map(|f| f.to_le_bytes())).collect();
        let draws = [DrawCmd { count: 3, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0; 4] }];
        let (w, h) = (64u32, 64u32);
        let bg = [1.0, 0.0, 0.0, 1.0]; // red background

        let centre = |rf: u32| -> [u8; 4] {
            let draw = ForwardedDraw {
                vertex_spirv: &vs,
                vertex_entry: c"main",
                fragment_spirv: &fs,
                fragment_entry: c"main",
                vertex_count: 0,
                topology: 0,
                geometry: Some(Geometry {
                    vertex_data: &verts,
                    vertex_stride: 8,
                    attrs: &attrs,
                    index_data: &[],
                    index_u32: false,
                    draws: &draws,
                    depth: None,
                    push_constants: &[],
                    textures: &[],
                    tex_binding: 0,
                    uniform: None,
                    storage: None,
                    raster_flags: rf,
                }),
            };
            let frame = gpu.render_forwarded(w, h, bg, &draw).expect("blend render");
            let idx = ((h / 2) * w + w / 2) as usize * 4;
            [frame.rgba[idx], frame.rgba[idx + 1], frame.rgba[idx + 2], frame.rgba[idx + 3]]
        };
        let opaque = centre(0); // blend off
        let blended = centre(raster_flags::BLEND); // blend on
        // Blend off: opaque blue overwrite (R≈0, B≈255).
        assert!(opaque[0] < 40 && opaque[2] > 200, "blend-off must be opaque blue, got {opaque:?}");
        // Blend on: ~50/50 of blue over red ⇒ roughly (128, 0, 128). Both channels mid-range, and
        // distinctly different from the opaque overwrite.
        assert!(
            blended[0] > 90 && blended[0] < 170 && blended[2] > 90 && blended[2] < 170,
            "blend-on must composite blue over red (~half red, half blue), got {blended:?}"
        );
        assert!(blended[0] > opaque[0] + 60, "blend must retain background red the opaque path drops");
        eprintln!("forwarded_alpha_blend_composites_over_background: OK (opaque={opaque:?}, blended={blended:?})");
    }

    /// Phase-2c multi-texture: TWO sampled textures bound into ONE descriptor set at distinct bindings
    /// (image@0/sampler@1 and image@2/sampler@3) both sample correctly — the gate for real material
    /// shaders (albedo + normal + …). The FS picks texture 0 on the left half of the quad and texture 1
    /// on the right; a red + a blue 1×1 texture must show red-left / blue-right. If the host bound only
    /// one, or crossed the bindings, the halves would be wrong.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_two_textures_sample_at_distinct_bindings() {
        let gpu = match HostGpu::open() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let vs = compile_wgsl(
            "struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };\n\
             @vertex fn main(@location(0) p: vec2<f32>, @location(1) uv: vec2<f32>) -> VOut {\n\
               return VOut(vec4<f32>(p, 0.0, 1.0), uv);\n}",
            naga::ShaderStage::Vertex,
        );
        let fs = compile_wgsl(
            "@group(0) @binding(0) var t0: texture_2d<f32>;\n\
             @group(0) @binding(1) var s0: sampler;\n\
             @group(0) @binding(2) var t1: texture_2d<f32>;\n\
             @group(0) @binding(3) var s1: sampler;\n\
             @fragment fn main(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {\n\
               if (uv.x < 0.5) { return textureSample(t0, s0, uv); }\n\
               return textureSample(t1, s1, uv);\n}",
            naga::ShaderStage::Fragment,
        );
        use infinigpu_abi::wire::vk_vformat as V;
        let attrs = [
            VertexAttr { location: 0, format: V::R32G32_SFLOAT, offset: 0 },
            VertexAttr { location: 1, format: V::R32G32_SFLOAT, offset: 8 },
        ];
        let quad: [[f32; 4]; 4] =
            [[-1.0, -1.0, 0.0, 0.0], [1.0, -1.0, 1.0, 0.0], [1.0, 1.0, 1.0, 1.0], [-1.0, 1.0, 0.0, 1.0]];
        let verts: Vec<u8> = quad.iter().flat_map(|v| v.iter().flat_map(|f| f.to_le_bytes())).collect();
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let index_data: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
        let red: [u8; 4] = [255, 0, 0, 255];
        let blue: [u8; 4] = [0, 0, 255, 255];
        let draws = [DrawCmd { count: 6, instance_count: 1, first: 0, vertex_offset: 0, viewport: [0.0; 4] }];
        let (w, h) = (64u32, 64u32);
        let bg = [0.0, 0.0, 0.0, 1.0];
        let nearest = SamplerCfg { linear: false, repeat: false };
        let textures = [
            Texture { width: 1, height: 1, rgba: &red, sampler: nearest },
            Texture { width: 1, height: 1, rgba: &blue, sampler: nearest },
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
                vertex_stride: 16,
                attrs: &attrs,
                index_data: &index_data,
                index_u32: false,
                draws: &draws,
                depth: None,
                push_constants: &[],
                textures: &textures,
                tex_binding: 0, // t0 @0/s0 @1, t1 @2/s1 @3
                uniform: None,
                storage: None,
                raster_flags: 0,
            }),
        };
        let frame = gpu.render_forwarded(w, h, bg, &draw).expect("two-texture render");
        // Sample a pixel in the left quarter (texture 0 = red) and the right quarter (texture 1 = blue).
        let px = |x: u32, y: u32| {
            let i = (y * w + x) as usize * 4;
            [frame.rgba[i], frame.rgba[i + 1], frame.rgba[i + 2]]
        };
        let left = px(w / 4, h / 2);
        let right = px(3 * w / 4, h / 2);
        assert!(left[0] > 200 && left[2] < 60, "left half must sample texture 0 (red), got {left:?}");
        assert!(right[2] > 200 && right[0] < 60, "right half must sample texture 1 (blue), got {right:?}");
        eprintln!("forwarded_two_textures_sample_at_distinct_bindings: OK (left={left:?}, right={right:?})");
    }
}
