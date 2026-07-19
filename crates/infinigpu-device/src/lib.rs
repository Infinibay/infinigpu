//! # infinigpu-device
//!
//! The host device seam (ADR-0001): a **vfio-user PCI device server** that QEMU
//! attaches to a guest (`-device vfio-user-pci,socket=…`). It implements
//! [`vfio_user::ServerBackend`] — config space + BAR0 control registers on the
//! trapped socket path, zero-copy guest-RAM DMA via `mmap`'d memfds ([`dma`]), and
//! MSI-X completion (hand-rolled capability; per-vector eventfds).
//!
//! Verified against `vfio_user` v0.1.3: there is **no ioeventfd doorbell**
//! (`GET_REGION_IO_FDS` is rejected), so submissions use `POLL_SUBMIT` (the host
//! polls the shared index page; a trapped doorbell write only wakes an idle poller
//! — modelled here by an immediate MSI-X raise for the loopback test). The whole
//! seam is exercised by `tests/loopback.rs` with the in-process `Client`, so it is
//! validated **before** QEMU exists.

mod config;
pub mod dispatch;
pub mod dma;
pub mod drain;
pub mod mailbox;
pub mod profile;
pub mod resource;

use dma::DmaTable;
use infinigpu_abi::regs;
use infinigpu_replay::{ForwardedDraw, Frame, HostGpu, PresentStats};
use infinigpu_sched::{BrokerConfig, GpuBroker, VmConfig, VmTicket};
use log::info;
use std::fs::File;
use std::io::{self, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Estimated VRAM footprint (MB) a display VM reserves at GPU-attach (scanout +
/// working set). A real encoder/3D VM would report more; Phase-0 uses a flat guess.
pub const VRAM_ESTIMATE_MB: u64 = 256;

/// Number of independent command rings (contexts) this device serves (ADR-0004/0006
/// multi-ring). Each has its own base + retired register + MSI-X vector (`ctx+1`).
/// `MAX_CONTEXTS` (63) is the ABI ceiling; MSI-X vector 0 is device/control.
pub const NUM_CONTEXTS: usize = 8;

/// One physical GPU context shared by every VM's backend. `render_clear` takes
/// `&self`; the [`GpuBroker`] run-lock guarantees only one submission runs at a time
/// (cooperative multiplexing, never MPS — ADR-0002/0007), so the inner mutex only
/// guards lazy open. Sharing one context is the Phase-1 first-cut simplification of
/// ADR-0003's per-VM replay *process*.
pub struct SharedGpu {
    gpu: Mutex<Option<HostGpu>>,
}

impl SharedGpu {
    pub fn new() -> Arc<Self> {
        Arc::new(SharedGpu {
            gpu: Mutex::new(None),
        })
    }

    /// Lazily open the GPU, then render a cleared frame. `None` on GPU-open/render
    /// failure (logged). Call only from inside a broker `run()` closure.
    pub fn render_clear(&self, width: u32, height: u32, rgba: [f32; 4]) -> Option<Frame> {
        let mut g = self.gpu.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            match HostGpu::open() {
                Ok(x) => {
                    info!("replay GPU: {} ({:?})", x.device_name(), x.driver_id());
                    *g = Some(x);
                }
                Err(e) => {
                    log::error!("cannot open replay GPU: {e}");
                    return None;
                }
            }
        }
        match g.as_ref().unwrap().render_clear(width, height, rgba) {
            Ok(f) => Some(f),
            Err(e) => {
                log::error!("render failed: {e}");
                None
            }
        }
    }

    /// Lazily open the GPU, then replay a **shader-executed** triangle workload (a real graphics
    /// pipeline + SM execution on the physical GPU — the Phase-0 3D remoting proof, `vk_op::TRIANGLE`).
    /// `None` on GPU-open/render failure (logged). Call only from inside a broker `run()` closure.
    pub fn render_triangle(&self, width: u32, height: u32, bg: [f32; 4]) -> Option<Frame> {
        let mut g = self.gpu.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            match HostGpu::open() {
                Ok(x) => {
                    info!("replay GPU: {} ({:?})", x.device_name(), x.driver_id());
                    *g = Some(x);
                }
                Err(e) => {
                    log::error!("cannot open replay GPU: {e}");
                    return None;
                }
            }
        }
        match g.as_ref().unwrap().render_triangle(width, height, bg) {
            Ok(f) => Some(f),
            Err(e) => {
                log::error!("triangle render failed: {e}");
                None
            }
        }
    }

    /// Lazily open the GPU, then replay a **forwarded** guest draw (`vk_op::FORWARDED`): the host
    /// compiles the guest ICD's forwarded SPIR-V with the real driver and executes the draw on the
    /// physical GPU. `None` on GPU-open/render failure (logged). Call only from inside a broker
    /// `run()` closure.
    pub fn render_forwarded(
        &self,
        width: u32,
        height: u32,
        bg: [f32; 4],
        draw: &ForwardedDraw,
    ) -> Option<Frame> {
        let mut g = self.gpu.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            match HostGpu::open() {
                Ok(x) => {
                    info!("replay GPU: {} ({:?})", x.device_name(), x.driver_id());
                    *g = Some(x);
                }
                Err(e) => {
                    log::error!("cannot open replay GPU: {e}");
                    return None;
                }
            }
        }
        match g.as_ref().unwrap().render_forwarded(width, height, bg, draw) {
            Ok(f) => Some(f),
            Err(e) => {
                log::error!("forwarded render failed: {e}");
                None
            }
        }
    }

    /// Like [`Self::render_forwarded`] but hands the finished pixels to `present` (the guest-scanout
    /// dma-write) **directly from the readback mapping** — one CPU copy instead of two, and no
    /// per-frame `Frame` heap allocation (perf finding #5). Falls back to render-then-present when
    /// the scratch cache is off (identical to the old path). `None` on GPU-open/render failure.
    pub fn render_forwarded_present<F: FnOnce(&[u8]) -> bool>(
        &self,
        width: u32,
        height: u32,
        bg: [f32; 4],
        draw: &ForwardedDraw,
        present: F,
    ) -> Option<PresentStats> {
        let mut g = self.gpu.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            match HostGpu::open() {
                Ok(x) => {
                    info!("replay GPU: {} ({:?})", x.device_name(), x.driver_id());
                    *g = Some(x);
                }
                Err(e) => {
                    log::error!("cannot open replay GPU: {e}");
                    return None;
                }
            }
        }
        match g
            .as_ref()
            .unwrap()
            .render_forwarded_present(width, height, bg, draw, present)
        {
            Ok(st) => Some(st),
            Err(e) => {
                log::error!("forwarded render failed: {e}");
                None
            }
        }
    }

    /// Lazily open the GPU and return its HAL capabilities (ADR-0008) — vendor, driver,
    /// render/timestamp/external-memory/priority flags — or `None` if no GPU.
    pub fn caps(&self) -> Option<infinigpu_hal::GpuCaps> {
        use infinigpu_hal::GpuBackend;
        let mut g = self.gpu.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            match HostGpu::open() {
                Ok(x) => *g = Some(x),
                Err(_) => return None,
            }
        }
        g.as_ref().map(|x| x.caps())
    }

    /// Lazily open the GPU and return its device name (e.g. "NVIDIA RTX A5000"), or
    /// `None` if no GPU is available.
    pub fn device_name(&self) -> Option<String> {
        let mut g = self.gpu.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            match HostGpu::open() {
                Ok(x) => *g = Some(x),
                Err(e) => {
                    log::error!("cannot open replay GPU: {e}");
                    return None;
                }
            }
        }
        g.as_ref().map(|x| x.device_name().to_string())
    }

    /// Phase-2 instrumentation passthrough: pipeline-cache `(hits, misses, cached)` if the GPU is
    /// open. `None` before the first render (GPU not yet lazily opened).
    pub fn cache_stats(&self) -> Option<(u64, u64, usize)> {
        let g = self.gpu.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().map(|x| x.cache_stats())
    }
}

impl Default for SharedGpu {
    fn default() -> Self {
        SharedGpu {
            gpu: Mutex::new(None),
        }
    }
}

/// Host-decoded, owning form of a `vk_op::FORWARDED` wire draw (see [`decode_forwarded`]). Owns the
/// SPIR-V words + entry names so a borrowing [`ForwardedDraw`] can be built inside the GPU run
/// closure (which must be `'static`). Public so the off-VM interop test can assert the guest C
/// encoder's bytes round-trip through this decoder.
pub struct OwnedForwardedDraw {
    pub vertex_spirv: Vec<u32>,
    pub fragment_spirv: Vec<u32>,
    pub vertex_entry: std::ffi::CString,
    pub fragment_entry: std::ffi::CString,
    pub vertex_count: u32,
    pub topology: u32,
}

impl OwnedForwardedDraw {
    fn as_draw(&self) -> ForwardedDraw<'_> {
        ForwardedDraw {
            vertex_spirv: &self.vertex_spirv,
            vertex_entry: &self.vertex_entry,
            fragment_spirv: &self.fragment_spirv,
            fragment_entry: &self.fragment_entry,
            vertex_count: self.vertex_count,
            topology: self.topology,
        }
    }
}

/// Decode the [`ForwardedDrawTail`](infinigpu_abi::wire::ForwardedDrawTail) + trailing SPIR-V/entry
/// blobs that follow the fixed [`VulkanWorkload`] in a `vk_op::FORWARDED` SUBMIT_CMD payload. Fully
/// bounds-checked against hostile guest input: every declared length must fit the remaining bytes
/// without overflow, each SPIR-V blob must be a non-empty multiple of 4 (Vulkan words) within
/// `max_bytes`, and each entry name must be a NUL-terminated string in its declared span. Returns
/// `None` on ANY violation, so the caller drops the submit and still retires the fence (fail-closed).
/// Public so the off-VM guest-conformance interop test can drive it with the C encoder's output.
pub fn decode_forwarded(payload: &[u8], max_bytes: usize) -> Option<OwnedForwardedDraw> {
    use infinigpu_abi::wire::ForwardedDrawTail;
    use zerocopy::FromBytes;

    // The tail sits right after the fixed VulkanWorkload header.
    let tail_bytes = payload.get(size_of::<infinigpu_abi::wire::VulkanWorkload>()..)?;
    let (tail, rest) = ForwardedDrawTail::read_from_prefix(tail_bytes).ok()?;

    let vlen = tail.vertex_spirv_len as usize;
    let flen = tail.fragment_spirv_len as usize;
    let velen = tail.vertex_entry_len as usize;
    let felen = tail.fragment_entry_len as usize;

    // SPIR-V: non-empty u32-word streams, bounded. Entry names: non-empty, reasonably short.
    if vlen == 0
        || flen == 0
        || !vlen.is_multiple_of(4)
        || !flen.is_multiple_of(4)
        || vlen > max_bytes
        || flen > max_bytes
        || velen == 0
        || felen == 0
        || velen > 256
        || felen > 256
    {
        return None;
    }
    // The four blobs must fit `rest` exactly-or-less, with no length overflow.
    let total = vlen.checked_add(flen)?.checked_add(velen)?.checked_add(felen)?;
    if total > rest.len() {
        return None;
    }
    let (vspirv_b, r) = rest.split_at(vlen);
    let (fspirv_b, r) = r.split_at(flen);
    let (ventry_b, r) = r.split_at(velen);
    let (fentry_b, _) = r.split_at(felen);

    // Copy SPIR-V into aligned u32 vectors (native byte order — host and guest share endianness).
    let to_words = |b: &[u8]| -> Vec<u32> {
        b.chunks_exact(4)
            .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    };
    let cstr = |b: &[u8]| -> Option<std::ffi::CString> {
        let nul = b.iter().position(|&c| c == 0)?;
        std::ffi::CString::new(&b[..nul]).ok()
    };

    let vertex_spirv = to_words(vspirv_b);
    let fragment_spirv = to_words(fspirv_b);

    // Cheap sanity gate: both blobs must start with the SPIR-V magic word. This rejects trivially
    // garbage input before it reaches the driver's shader compiler. It does NOT make the SPIR-V
    // safe — an adversarial-but-well-formed module can still hang/crash the compiler, and because
    // SharedGpu runs one VkDevice across tenants (ADR-0003 per-VM jailed replay process is the
    // deferred real isolation), that would wedge co-tenants. Full validation (spirv-val) / process
    // isolation is tracked for a later phase; this is defense-in-depth, not a security boundary.
    const SPIRV_MAGIC: u32 = 0x0723_0203;
    if vertex_spirv.first() != Some(&SPIRV_MAGIC) || fragment_spirv.first() != Some(&SPIRV_MAGIC) {
        return None;
    }

    Some(OwnedForwardedDraw {
        vertex_spirv,
        fragment_spirv,
        vertex_entry: cstr(ventry_b)?,
        fragment_entry: cstr(fentry_b)?,
        vertex_count: tail.vertex_count,
        topology: tail.topology,
    })
}
use vfio_bindings::bindings::vfio::{
    vfio_region_info, VFIO_IRQ_INFO_EVENTFD, VFIO_IRQ_SET_ACTION_TRIGGER,
    VFIO_IRQ_SET_DATA_EVENTFD, VFIO_PCI_BAR0_REGION_INDEX, VFIO_PCI_BAR1_REGION_INDEX,
    VFIO_PCI_CONFIG_REGION_INDEX, VFIO_PCI_MSIX_IRQ_INDEX, VFIO_PCI_NUM_IRQS, VFIO_PCI_NUM_REGIONS,
    VFIO_REGION_INFO_FLAG_READ, VFIO_REGION_INFO_FLAG_WRITE,
};
use vfio_user::{DmaMapFlags, DmaUnmapFlags, IrqInfo, Server, ServerBackend, ServerRegion};

/// Test/debug scaffolding registers in a reserved BAR0 sub-range. These let the
/// loopback test drive DMA through the seam; they are **not** part of the guest ABI
/// and will be removed once the real ring decoder lands.
pub mod dbg {
    /// Program the guest IOVA the debug DMA register operates on (lo/hi halves).
    pub const DMA_ADDR_LO: u64 = 0x0F00;
    pub const DMA_ADDR_HI: u64 = 0x0F04;
    /// Read = load a `u32` from guest RAM at the programmed IOVA (via the DMA table);
    /// write = store a `u32` there. Proves bidirectional zero-copy DMA.
    pub const DMA_DATA: u64 = 0x0F08;
}

/// A persistent per-VM BGRA scanout surface for the accelerated 2D damage path.
///
/// The full-frame path (`DISPLAY_SCANOUT`) rebuilds the whole BGRA frame from a full
/// framebuffer DMA-read every present. The damage path (`DISPLAY_SCANOUT_DAMAGE`) instead
/// keeps this surface across presents and patches **only the dirty sub-rectangle** into it,
/// then streams the whole surface — so the encoder still sees a complete frame (its P-frames
/// diff it) while the device DMA-reads + repacks only `dw*dh*4` bytes instead of the entire
/// `pitch*height`. Reset to `None` on any resize/first-frame so a stale surface can never
/// leak partial-frame corruption (the guest re-sends a full frame on modeset).
struct ScanoutBuffer {
    w: usize,
    h: usize,
    /// Tightly-packed BGRA, `w*h*4` bytes (what the encoder ingests).
    bgra: Vec<u8>,
}

/// Largest cursor sprite the device will DMA-read + forward (matches the guest's advertised
/// `mode_config.cursor_width/height`). A `pitch*height` above this is rejected fail-closed.
const CURSOR_MAX_DIM: usize = 256;
const CURSOR_MAX_BYTES: usize = CURSOR_MAX_DIM * CURSOR_MAX_DIM * 4;
/// Minimum spacing between cursor **shape** DMA-reads: coalesces a hostile DEFINE flood (the
/// de-dup-alternation attack) to at most ~this rate so a guest can't peg the callback thread.
const CURSOR_SHAPE_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);
/// Minimum spacing between forwarded cursor **MOVE**s (position coalescing) — WARP moves bypass it.
const CURSOR_MOVE_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// Per-VM cursor-plane state for the accelerated cursor path. De-dups unchanged shapes/positions,
/// rate-limits shape reads (DoS), and remembers whether the guest must be re-solicited after a
/// device (re)admission (the guest only emits `CURSOR_UPDATE` on a cursor *change*, so a re-adopted
/// device would otherwise have no shape to prime a viewer with).
#[derive(Default)]
struct CursorState {
    /// Last forwarded shape key `(shape_ref, width, height, pitch)` — skip an identical re-DEFINE.
    last_shape: Option<(u64, u16, u16, u32)>,
    /// Last forwarded position — suppress redundant MOVEs.
    last_pos: Option<(i32, i32)>,
    /// Last forwarded VISIBLE state.
    last_visible: bool,
    /// Last shape DMA-read time (rate limit).
    last_shape_read: Option<Instant>,
    /// Last forwarded MOVE time (rate limit).
    last_move_sent: Option<Instant>,
}

/// Whether a guest cursor-shape DEFINE is safe to DMA-read + forward: non-empty, within the
/// 256×256 sprite bound, the only accepted pixel format ([`format::B8G8R8A8`]), and a `pitch` that
/// covers a full row without overflowing the byte cap. Fail-closed — any hostile value (zero dims,
/// oversized, wrong format, too-small or overflowing pitch) returns `false` and the cursor is
/// dropped, never DMA-read.
fn cursor_shape_ok(width: u16, height: u16, pitch: u32, format: u32) -> bool {
    let (w, h, pitch) = (width as usize, height as usize, pitch as usize);
    w != 0
        && h != 0
        && w <= CURSOR_MAX_DIM
        && h <= CURSOR_MAX_DIM
        && format == infinigpu_abi::wire::format::B8G8R8A8
        && pitch >= w * 4
        && pitch.checked_mul(h).map_or(false, |t| t <= CURSOR_MAX_BYTES)
}

/// Clamp a guest-supplied damage rect into the `[0,w]×[0,h]` frame. Saturating `min`
/// math guarantees the result satisfies `dx+dw <= w` and `dy+dh <= h` for **any** `u32`
/// input (including `u32::MAX`), so a hostile guest can never drive an out-of-bounds
/// framebuffer index. Returns `(dx, dy, dw, dh)` in pixels.
fn clamp_damage(w: usize, h: usize, dx: u32, dy: u32, dw: u32, dh: u32) -> (usize, usize, usize, usize) {
    let dx = (dx as usize).min(w);
    let dy = (dy as usize).min(h);
    let dw = (dw as usize).min(w - dx);
    let dh = (dh as usize).min(h - dy);
    (dx, dy, dw, dh)
}

/// The infinigpu vfio-user device backend.
pub struct InfinigpuBackend {
    config: Vec<u8>,
    dma: DmaTable,
    /// Per-vector MSI-X trigger eventfds (index 0 = device/control, 1.. per-context).
    msix: Vec<Option<File>>,
    global_ctrl: u32,
    global_status: u32,
    irq_mask: u32,
    dbg_dma_addr: u64,
    /// Per-context command-ring bases (guest physical), programmed via the per-context
    /// `CMD_RING_BASE` block. Index = context/ring id (`0..NUM_CONTEXTS`).
    ring_base: Vec<u64>,
    /// Per-context highest retired seqno (completion sync via trapped read). Ring 0 is
    /// also mirrored at the fixed `CMD_RING0_RETIRED` register for the Phase-0 driver.
    ring_retired: Vec<u64>,
    /// Per-context `RingIndices`-page IOVA (`CMD_RING_INDEX`). Non-zero switches ring `ctx` to the
    /// PR4 real drainer (`drain_ctx`); zero keeps the Phase-0 single-descriptor path.
    ring_index: Vec<u64>,
    /// Per-context descriptor-ring capacity in entries (`CMD_RING_SIZE`; power of two). Only used on
    /// the real-drainer path.
    ring_cap: Vec<u32>,
    /// Per-VM host-side blob resource table (PR4). Populated by the `RESOURCE_*` ring messages;
    /// the resource-backed present (`RESOURCE_FLUSH`) resolves a blob's backing through `dma`.
    resources: resource::ResourceTable,
    /// The GPU broker (ADR-0007): admission + fair-share scheduling. Shared across
    /// every VM's backend so they cooperatively share one physical GPU.
    broker: Arc<GpuBroker>,
    /// The shared physical GPU context (one per host, serialized by the broker).
    shared_gpu: Arc<SharedGpu>,
    /// This VM's scheduling policy (weight, VRAM cap, priority tier).
    vm_config: VmConfig,
    /// Admission ticket — `Some` once the broker admits this VM at GPU-attach; holds
    /// its VRAM reservation + concurrency slot until drop (reap).
    ticket: Option<VmTicket>,
    /// True once admission has been attempted and denied (so we stop retrying/log once).
    admission_denied: bool,
    /// Monotonic count of scanout presents (DRM/KMS page-flips) served.
    present_count: u64,
    /// If set (env `INFINIGPU_PRESENT_DIR`), each presented framebuffer is written
    /// here as a PPM so the guest's real console/desktop can be viewed host-side.
    present_dir: Option<PathBuf>,
    /// If set (env `INFINIGPU_PIXEL_PORT`), each presented framebuffer is H.264-encoded
    /// on NVENC and streamed over WebSocket (infiniPixel) — watch the live guest desktop
    /// in a browser. The WebSocket server binds eagerly; the encoder is created on the
    /// first present, sized to the guest framebuffer.
    pixel: Option<infinigpu_pixel::PixelStreamer>,
    /// Persistent BGRA surface for the accelerated 2D damage path (`ScanoutBuffer`).
    /// `None` until the first present, and re-set to `None`/rebuilt on any resize so the
    /// damage path never patches into a stale-sized buffer.
    last_scanout: Option<ScanoutBuffer>,
    /// Guest-framebuffer bytes DMA-read for the most recent present — the whole
    /// `pitch*height` for a full-frame present, `~dw*dh*4` for a damage present. Surfaced
    /// in the periodic infiniPixel stats line so the damage-path win is observable.
    bytes_read_last: usize,
    /// Per-VM cursor-plane state (client-side cursor overlay path). `None` until the first
    /// `CURSOR_UPDATE`; reset on device reset.
    cursor: CursorState,
    /// Set on (re)admission: the guest must be re-solicited for its current cursor because the
    /// device's `CursorState`/Hub cache was lost (the guest only emits on a cursor *change*).
    cursor_needs_resolicit: bool,
    /// Whether this device advertises `caps::CURSOR_PLANE` (env `INFINIGPU_CURSOR_PLANE`).
    cursor_plane_enabled: bool,
    /// Monotonic count of 3D (`encoding::VULKAN_VENUSLIKE`) submissions seen on the ring. The
    /// Venus command decoder (`crates/infinigpu-replay/src/venus/`) is gated on the Phase-0
    /// go/no-go spike (`docs/spikes/venus-nvidia-a5000.md`); until it lands this counts submits so
    /// a guest render node can prove the 3D datapath reaches the device (fence retires) without the
    /// generic "unsupported encoding" warn. Reset on device reset.
    vulkan_submits: u64,
    /// The framebuffer dims the broker VRAM reservation was last trued-up for (PR8). Admission
    /// reserves a baseline at attach (before the guest picks a mode); the first present at each new
    /// size revises the reservation to cover the real per-VM scanout footprint. `None` until the
    /// first present; reset on device reset.
    scanout_vram_dims: Option<(u32, u32)>,
    /// Opt-in per-submit latency profiler (env `INFINIGPU_PROFILE`); `None` in production so the
    /// hot path is unaffected. See [`profile`].
    profiler: Option<profile::SubmitProfiler>,
}

impl Default for InfinigpuBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a [`BrokerConfig`] from **real** GPU capacity via NVML, falling back to the
/// default (fixed) capacity when NVML is unavailable (no NVIDIA driver / CI). This makes
/// admission count the GPU's actual total VRAM instead of a hardcoded guess — the
/// measured-capacity half of ADR-0003/0007. Per-VM VRAM attribution (each jailed replay
/// process → its pid's VRAM via `infinigpu_nvml::NvmlProbe::process_vram`) layers on top
/// once the replay runs as a separate process.
pub fn broker_config_from_nvml() -> BrokerConfig {
    let mut cfg = BrokerConfig::default();
    // PR8: a host on an older driver/SKU whose NVENC block caps concurrent sessions sets this so
    // an over-cap streaming VM is denied-with-reason instead of getting a silent black stream.
    // Unset (default `None`) = unlimited, matching drivers ≥550.
    if let Some(n) = std::env::var("INFINIGPU_MAX_ENC_SESSIONS").ok().and_then(|s| s.parse().ok()) {
        cfg.max_enc_sessions = Some(n);
        info!("broker: NVENC session cap set to {n} (env INFINIGPU_MAX_ENC_SESSIONS)");
    }
    match infinigpu_nvml::NvmlProbe::open().and_then(|p| p.snapshot(0)) {
        Ok(s) => {
            cfg.total_vram_mb = s.total_mb;
            info!(
                "broker capacity from NVML: {} — {} MB total, {} MB free, {} enc-sessions",
                s.name,
                s.total_mb,
                s.free_mb,
                s.encoder_sessions.map(|n| n.to_string()).unwrap_or_else(|| "n/a".into()),
            );
        }
        Err(e) => info!(
            "NVML unavailable ({e}); broker uses default capacity {} MB",
            cfg.total_vram_mb
        ),
    }
    cfg
}

impl InfinigpuBackend {
    /// Standalone single-VM backend with its own broker (real NVML capacity when
    /// available) and GPU context. Used by the single-socket `infinigpu-device` binary
    /// and the in-process demos/tests.
    pub fn new() -> Self {
        let broker = GpuBroker::with_real_clock(broker_config_from_nvml());
        Self::with_broker(broker, SharedGpu::new(), VmConfig::new("vm", 1, 4096))
    }

    /// Multi-VM backend: shares a `broker` + `shared_gpu` with other VMs' backends, so
    /// the broker arbitrates one physical GPU across them. `vm_config` carries this
    /// VM's weight / VRAM cap / priority.
    pub fn with_broker(
        broker: Arc<GpuBroker>,
        shared_gpu: Arc<SharedGpu>,
        vm_config: VmConfig,
    ) -> Self {
        let present_dir = std::env::var_os("INFINIGPU_PRESENT_DIR").map(PathBuf::from);
        if let Some(dir) = &present_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        let pixel_port: Option<u16> = std::env::var("INFINIGPU_PIXEL_PORT")
            .ok()
            .and_then(|s| s.parse().ok());
        // Bind the infiniPixel WebSocket server eagerly (encoder stays lazy, sized to
        // the first frame) so a viewer can connect *before* the first present and catch
        // the whole stream from frame 0.
        let pixel = pixel_port.and_then(|port| match infinigpu_pixel::PixelStreamer::new(30, 8000, port) {
            Ok(s) => {
                info!("infiniPixel: serving on ws://0.0.0.0:{port} (open client/infinipixel.html?port={port})");
                Some(s)
            }
            Err(e) => {
                log::error!("infiniPixel: cannot bind :{port}: {e}");
                None
            }
        });
        let profiler = profile::SubmitProfiler::from_env(&vm_config.vm_id);
        InfinigpuBackend {
            config: config::build(),
            dma: DmaTable::new(),
            msix: (0..config::MSIX_VECTORS).map(|_| None).collect(),
            global_ctrl: 0,
            global_status: regs::global_status::READY,
            irq_mask: 0,
            dbg_dma_addr: 0,
            ring_base: vec![0; NUM_CONTEXTS],
            ring_retired: vec![0; NUM_CONTEXTS],
            ring_index: vec![0; NUM_CONTEXTS],
            ring_cap: vec![0; NUM_CONTEXTS],
            resources: resource::ResourceTable::new(),
            broker,
            shared_gpu,
            vm_config,
            ticket: None,
            admission_denied: false,
            present_count: 0,
            present_dir,
            pixel,
            last_scanout: None,
            bytes_read_last: 0,
            cursor: CursorState::default(),
            cursor_needs_resolicit: false,
            cursor_plane_enabled: std::env::var_os("INFINIGPU_CURSOR_PLANE").is_some(),
            vulkan_submits: 0,
            scanout_vram_dims: None,
            profiler,
        }
    }

    /// Admission control (ADR-0007): ask the broker to admit this VM at GPU-attach.
    /// Idempotent. Returns whether the VM currently holds a ticket. Fail-closed: on
    /// denial, no ticket is granted and GPU submissions are refused.
    fn ensure_admitted(&mut self) -> bool {
        if self.ticket.is_some() {
            return true;
        }
        if self.admission_denied {
            return false;
        }
        // PR8: a streaming VM (infiniPixel encoder bound) also claims a scarce NVENC session, so a
        // host at its encode-session cap denies the (n+1)-th VM with a reason instead of a black
        // stream. A control-plane-only backend (no pixel port) reserves VRAM only.
        let req = if self.pixel.is_some() {
            infinigpu_sched::AdmitRequest::streaming(VRAM_ESTIMATE_MB)
        } else {
            infinigpu_sched::AdmitRequest::vram(VRAM_ESTIMATE_MB)
        };
        match self.broker.admit(self.vm_config.clone(), req) {
            Ok(t) => {
                self.ticket = Some(t);
                true
            }
            Err(e) => {
                log::warn!(
                    "admission DENIED for vm={}: {e} — GPU submissions refused (fail-closed)",
                    self.vm_config.vm_id
                );
                self.global_status |= regs::global_status::FATAL;
                self.admission_denied = true;
                false
            }
        }
    }

    /// PR8: true up this VM's broker VRAM reservation to the real per-VM scanout footprint once the
    /// guest negotiates its framebuffer size. Admission reserved only a baseline (`VRAM_ESTIMATE_MB`)
    /// at attach — before any mode was set; a per-VM host `ScanoutTarget` adds ~3× `w*h*4`. Runs at
    /// most once per distinct size (cheap no-op every other present). **Degrade, never black:** if
    /// the grow is refused (host near capacity) we keep the baseline reservation and keep streaming.
    fn account_scanout_vram(&mut self, w: u32, h: u32) {
        if self.scanout_vram_dims == Some((w, h)) {
            return;
        }
        self.scanout_vram_dims = Some((w, h));
        let want = VRAM_ESTIMATE_MB + infinigpu_sched::scanout_vram_estimate_mb(w, h, 3);
        if let Some(t) = &self.ticket {
            match t.adjust_vram(want) {
                Ok(mb) => log::debug!("PR8: VRAM reservation trued up to {mb} MB for {w}x{h}"),
                Err(e) => log::warn!(
                    "PR8: VRAM true-up to {want} MB for {w}x{h} refused ({e}); keeping baseline \
                     {VRAM_ESTIMATE_MB} MB (degrade, not black)"
                ),
            }
        }
    }

    /// Raise MSI-X `vector` by writing to its eventfd (QEMU's irqfd delivers it).
    fn raise(&mut self, vector: usize) {
        if let Some(Some(f)) = self.msix.get_mut(vector) {
            let _ = f.write_all(&1u64.to_le_bytes());
        }
    }

    fn reset_state(&mut self) {
        self.global_ctrl = 0;
        self.global_status = regs::global_status::NEEDS_RESET;
        // Reap: dropping the ticket releases this VM's VRAM reservation + concurrency
        // slot back to the broker so the capacity is immediately re-admittable.
        self.ticket = None;
        self.admission_denied = false;
        self.ring_base.iter_mut().for_each(|b| *b = 0);
        self.ring_retired.iter_mut().for_each(|r| *r = 0);
        self.ring_index.iter_mut().for_each(|i| *i = 0);
        self.ring_cap.iter_mut().for_each(|c| *c = 0);
        // Blob resources are per-generation; the guest re-creates them after a reset.
        self.resources.clear();
        // Drop the persistent damage surface: after a reset the guest re-negotiates and
        // sends a fresh full-frame present, so a retained buffer would only risk staleness.
        self.last_scanout = None;
        // Cursor state is lost across a reset; the guest re-emits its cursor on the next change,
        // and we flag a re-solicit so a viewer isn't left with a stale/empty overlay meanwhile.
        self.cursor = CursorState::default();
        self.cursor_needs_resolicit = true;
        // 3D submit counter is per-attach observability; a fresh generation starts from zero.
        self.vulkan_submits = 0;
        // The VRAM true-up re-runs on the next present after a reset (fresh mode negotiation).
        self.scanout_vram_dims = None;
        if let Some(p) = &self.pixel {
            p.reset_planes();
        }
        // DMA maps persist across device reset (the client re-sends DMA_UNMAP/MAP).
    }

    /// If `off` falls in the per-context config block region, return `(ctx, field)`
    /// where `field` is the byte offset within context `ctx`'s block.
    fn ctx_block(off: u64) -> Option<(usize, u64)> {
        let base = regs::ctrl::CMD_RING_CFG;
        let span = NUM_CONTEXTS as u64 * regs::CMD_RING_STRIDE;
        if (base..base + span).contains(&off) {
            let rel = off - base;
            Some(((rel / regs::CMD_RING_STRIDE) as usize, rel % regs::CMD_RING_STRIDE))
        } else {
            None
        }
    }

    fn bar0_read_u32(&mut self, off: u64) -> u32 {
        use regs::ctrl::*;
        // Per-context config block: base + retired readback for ring `ctx`.
        if let Some((ctx, field)) = Self::ctx_block(off) {
            return match field {
                CMD_RING_BASE_LO => (self.ring_base[ctx] & 0xFFFF_FFFF) as u32,
                CMD_RING_BASE_HI => (self.ring_base[ctx] >> 32) as u32,
                CMD_RING_SIZE => self.ring_cap[ctx],
                CMD_RING_RETIRED_LO => (self.ring_retired[ctx] & 0xFFFF_FFFF) as u32,
                CMD_RING_RETIRED_HI => (self.ring_retired[ctx] >> 32) as u32,
                CMD_RING_INDEX_LO => (self.ring_index[ctx] & 0xFFFF_FFFF) as u32,
                CMD_RING_INDEX_HI => (self.ring_index[ctx] >> 32) as u32,
                _ => 0,
            };
        }
        match off {
            DEV_MAGIC => infinigpu_abi::DEV_MAGIC,
            ABI_VERSION => infinigpu_abi::abi_version(),
            // Advertise the cursor plane only when explicitly enabled (rollout gate); otherwise
            // the guest keeps its software cursor (today's path) and never emits CURSOR_UPDATE.
            DEV_CAPS if self.cursor_plane_enabled => regs::PHASE2_DEV_CAPS,
            DEV_CAPS => regs::PHASE1_DEV_CAPS,
            NUM_CONTEXTS => crate::NUM_CONTEXTS as u32,
            MAX_RING_ENTRIES => 256,
            BAR2_APERTURE_MB => 0, // no blob aperture yet
            GLOBAL_CTRL => self.global_ctrl,
            GLOBAL_STATUS => self.global_status,
            IRQ_MASK => self.irq_mask,
            // Ring 0 retired mirrored at the fixed register for the Phase-0 guest driver.
            CMD_RING0_RETIRED_LO => (self.ring_retired[0] & 0xFFFF_FFFF) as u32,
            CMD_RING0_RETIRED_HI => (self.ring_retired[0] >> 32) as u32,
            dbg::DMA_DATA => self.dma.read_u32(self.dbg_dma_addr).unwrap_or(0xFFFF_FFFF),
            _ => 0,
        }
    }

    fn bar0_write_u32(&mut self, off: u64, val: u32) {
        use regs::ctrl::*;

        // Doorbell page: a trapped write. In the real device this wakes the poller;
        // here we run the submit engine for the addressed ring inline (which raises the
        // ring's MSI-X vector on completion).
        if (regs::doorbell::PAGE..regs::doorbell::PAGE + 0x1000).contains(&off) {
            if off == regs::doorbell::CTRL {
                self.raise(0);
            } else if off >= regs::doorbell::CMD_BASE {
                let ctx = ((off - regs::doorbell::CMD_BASE) / 4) as usize;
                if ctx < crate::NUM_CONTEXTS {
                    self.process_ring(ctx);
                    // Signal ring `ctx`'s completion vector (models the poller waking on
                    // the doorbell and retiring submitted work; guests may instead poll
                    // this ring's retired register).
                    self.raise(ctx + 1);
                }
            }
            return;
        }

        // Per-context config block: program ring `ctx`'s base address.
        if let Some((ctx, field)) = Self::ctx_block(off) {
            match field {
                CMD_RING_BASE_LO => {
                    self.ring_base[ctx] = (self.ring_base[ctx] & !0xFFFF_FFFF) | u64::from(val)
                }
                CMD_RING_BASE_HI => {
                    self.ring_base[ctx] = (self.ring_base[ctx] & 0xFFFF_FFFF) | (u64::from(val) << 32)
                }
                CMD_RING_SIZE => self.ring_cap[ctx] = val,
                CMD_RING_INDEX_LO => {
                    self.ring_index[ctx] = (self.ring_index[ctx] & !0xFFFF_FFFF) | u64::from(val)
                }
                CMD_RING_INDEX_HI => {
                    self.ring_index[ctx] = (self.ring_index[ctx] & 0xFFFF_FFFF) | (u64::from(val) << 32)
                }
                _ => {}
            }
            return;
        }

        match off {
            GLOBAL_CTRL => {
                self.global_ctrl = val;
                // GPU-attach: the guest enabling the device is the admission trigger.
                if val & regs::global_ctrl::DEVICE_ENABLE != 0 {
                    self.ensure_admitted();
                }
            }
            DEVICE_RESET => {
                if val & 1 != 0 {
                    self.reset_state();
                }
            }
            IRQ_MASK => self.irq_mask = val,
            dbg::DMA_ADDR_LO => {
                self.dbg_dma_addr = (self.dbg_dma_addr & !0xFFFF_FFFF) | u64::from(val)
            }
            dbg::DMA_ADDR_HI => {
                self.dbg_dma_addr = (self.dbg_dma_addr & 0xFFFF_FFFF) | (u64::from(val) << 32)
            }
            dbg::DMA_DATA => {
                self.dma.write_u32(self.dbg_dma_addr, val);
            }
            _ => {}
        }
    }

    fn read_bar0(&mut self, offset: u64, data: &mut [u8]) {
        let mut done = 0;
        while done < data.len() {
            let cur = offset + done as u64;
            let aligned = cur & !3;
            let within = (cur - aligned) as usize;
            let word = self.bar0_read_u32(aligned).to_le_bytes();
            let n = core::cmp::min(4 - within, data.len() - done);
            data[done..done + n].copy_from_slice(&word[within..within + n]);
            done += n;
        }
    }

    fn write_bar0(&mut self, offset: u64, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let cur = offset + done as u64;
            let aligned = cur & !3;
            let within = (cur - aligned) as usize;
            let n = core::cmp::min(4 - within, data.len() - done);
            // read-modify-write so sub-word writes are safe (doorbells read as 0).
            let mut word = self.bar0_read_u32(aligned).to_le_bytes();
            word[within..within + n].copy_from_slice(&data[done..done + n]);
            self.bar0_write_u32(aligned, u32::from_le_bytes(word));
            done += n;
        }
    }

    /// Phase-0 submit engine for command ring `ctx`: decode the SUBMIT_CMD at that
    /// ring's base from guest RAM (via the DMA table) and execute it. For `DISPLAY_CLEAR`
    /// this renders on the GPU and DMA-writes the result to the guest scanout address.
    /// The production path uses the polled multi-descriptor ring + real Vulkan payloads;
    /// this drives the whole pipeline end-to-end for bring-up. Completion sets ring
    /// `ctx`'s retired seqno and raises MSI-X vector `ctx+1`.
    fn process_ring(&mut self, ctx: usize) {
        use infinigpu_abi::wire::{
            encoding, msg_type, ClearPresent, CursorUpdate, Descriptor, ScanoutPresent,
            ScanoutPresentDamaged, SubmitCmd, VulkanWorkload,
        };
        use zerocopy::FromBytes;

        // PR4: when the guest has programmed a RingIndices page for this ctx, drive it as a real
        // SPSC descriptor ring (bounded two-phase drain + RESOURCE_* dispatch). Otherwise fall
        // through to the Phase-0 single-descriptor path below.
        if self.ring_index[ctx] != 0 {
            self.drain_ctx(ctx);
            return;
        }

        let base = self.ring_base[ctx];
        log::debug!("process_ring ctx={ctx} base={base:#x}");
        if base == 0 {
            return;
        }
        let mut db = [0u8; core::mem::size_of::<Descriptor>()];
        if !self.dma.read(base, &mut db) {
            log::error!("ring {ctx} base {base:#x} not mapped");
            return;
        }
        let desc = Descriptor::read_from_bytes(&db).unwrap();
        log::debug!(
            "  desc msg_type={:#x} data_offset={}",
            desc.msg_type,
            desc.data_offset
        );
        // Cursor-plane sideband (a control message, not a command submission): the CursorUpdate
        // body sits at `base + data_offset`. Decode + forward, then retire this ring's seqno — a
        // malformed cursor is dropped but never stalls the ring.
        if desc.msg_type == msg_type::CURSOR_UPDATE {
            let payload_addr = base + desc.data_offset as u64;
            let mut cb = [0u8; core::mem::size_of::<CursorUpdate>()];
            let n = if desc.len == 0 {
                cb.len()
            } else {
                (desc.len as usize).min(cb.len())
            };
            if self.dma.read(payload_addr, &mut cb[..n]) {
                let cu = CursorUpdate::read_from_bytes(&cb).unwrap();
                self.handle_cursor_update(&cu);
            }
            self.ring_retired[ctx] = desc.seqno;
            return;
        }
        if desc.msg_type != msg_type::SUBMIT_CMD {
            return;
        }
        let mut sb = [0u8; core::mem::size_of::<SubmitCmd>()];
        if !self.dma.read(base + db.len() as u64, &mut sb) {
            return;
        }
        let sc = SubmitCmd::read_from_bytes(&sb).unwrap();
        let payload_addr = base + desc.data_offset as u64;
        match sc.encoding {
            encoding::DISPLAY_CLEAR => {
                let mut pb = [0u8; core::mem::size_of::<ClearPresent>()];
                if !self.dma.read(payload_addr, &mut pb) {
                    return;
                }
                let cp = ClearPresent::read_from_bytes(&pb).unwrap();
                self.render_clear_present(&cp, sc.seqno, ctx);
            }
            encoding::DISPLAY_SCANOUT => {
                let mut pb = [0u8; core::mem::size_of::<ScanoutPresent>()];
                if !self.dma.read(payload_addr, &mut pb) {
                    return;
                }
                let sp = ScanoutPresent::read_from_bytes(&pb).unwrap();
                self.present_scanout(&sp, sc.seqno, ctx);
            }
            encoding::DISPLAY_SCANOUT_DAMAGE => {
                // Bounded, zero-filled read (ADR-0004 forward-compat): honor `payload_len`
                // so a future guest that appends trailing fields still parses here (we read
                // the prefix we understand, zero-fill the rest), and an unset `payload_len`
                // falls back to the full struct size. Never reads past the struct.
                let mut pb = [0u8; core::mem::size_of::<ScanoutPresentDamaged>()];
                let n = if sc.payload_len == 0 {
                    pb.len()
                } else {
                    (sc.payload_len as usize).min(pb.len())
                };
                if !self.dma.read(payload_addr, &mut pb[..n]) {
                    return;
                }
                let sp = ScanoutPresentDamaged::read_from_bytes(&pb).unwrap();
                self.present_scanout_damaged(&sp, sc.seqno, ctx);
            }
            encoding::VULKAN_VENUSLIKE => {
                // Read the workload header from guest RAM (bounded to what we understand; a fuller
                // ICD's trailing vkCmd* stream past the header is ignored in Phase-0).
                let mut pb = [0u8; core::mem::size_of::<VulkanWorkload>()];
                let n = if sc.payload_len == 0 {
                    pb.len()
                } else {
                    (sc.payload_len as usize).min(pb.len())
                };
                if self.dma.read(payload_addr, &mut pb[..n]) {
                    self.submit_vulkan(&sc, &pb, ctx);
                }
            }
            other => log::warn!("unsupported encoding {:#x}", other),
        }
    }

    /// PR4 real-ring drainer for context `ctx` (`docs/adr/2D-ACCEL-IMPLEMENTATION.md`). Runs the
    /// loom-verified SPSC [`infinigpu_ring::Ring`] over the guest-shared, DMA-resident index page +
    /// descriptor array (addressed by `CMD_RING_INDEX` / `CMD_RING_BASE` / `CMD_RING_SIZE`), in the
    /// two-phase bounded shape (`drain.rs`): **phase 1** pops a batch (≤ capacity) under the ring
    /// view then drops it; **phase 2** executes each descriptor under `&mut self`; **phase 3**
    /// retires the highest seqno on the shared page (+ the host-side register mirror) so the guest's
    /// fences resolve. Fail-closed throughout: bad geometry, an unmapped page, or a malformed
    /// descriptor is logged + skipped — never a stall, never an out-of-bounds read.
    fn drain_ctx(&mut self, ctx: usize) {
        use infinigpu_abi::wire::{Descriptor, RingIndices};

        let index_iova = self.ring_index[ctx];
        let desc_iova = self.ring_base[ctx];
        let cap = self.ring_cap[ctx] as usize;
        if desc_iova == 0 || cap == 0 || !cap.is_power_of_two() {
            log::error!("drain ctx={ctx}: bad ring geometry (base={desc_iova:#x} cap={cap})");
            return;
        }
        // Resolve the shared ring memory to host pointers (fail-closed bounds check). `host_ptr`
        // returns raw pointers, not a borrow of `self`, so descriptor execution can take `&mut self`
        // afterwards; the pointers stay valid because a ring drain never unmaps DMA.
        let idx_bytes = core::mem::size_of::<RingIndices>();
        let desc_bytes = cap * core::mem::size_of::<Descriptor>();
        let (index_ptr, desc_ptr) = unsafe {
            match (self.dma.host_ptr(index_iova, idx_bytes), self.dma.host_ptr(desc_iova, desc_bytes)) {
                (Some(i), Some(d)) => (i as *const u8, d as *const u8),
                _ => {
                    log::error!("drain ctx={ctx}: ring memory not fully mapped");
                    return;
                }
            }
        };

        // Admission gate (fail-closed), once per drain: a VM the broker didn't admit runs nothing.
        if !self.ensure_admitted() {
            return;
        }

        // Phase 1: pop a bounded batch, then drop the ring view (releases the shared-page pointers).
        let drained = {
            let ring = match unsafe { drain::ring_over_shared(index_ptr, desc_ptr, cap) } {
                Ok(r) => r,
                Err(e) => {
                    log::error!("drain ctx={ctx}: ring view rejected: {e:?}");
                    return;
                }
            };
            drain::pop_batch(&ring, cap)
        };

        // Phase 2: execute each descriptor under &mut self (DMA reads, resource-table mutation,
        // present). Bounded by the batch size (≤ capacity).
        for desc in &drained.descriptors {
            self.execute_descriptor(ctx, desc);
        }

        // Phase 3: publish the highest retired seqno on the shared index page + mirror it in the
        // host-side register (the doorbell handler raises the completion vector).
        if drained.highest_seqno > 0 {
            let _ = unsafe {
                drain::retire_over_shared(index_ptr, desc_ptr, cap, drained.highest_seqno)
            };
            self.ring_retired[ctx] = drained.highest_seqno;
        }
    }

    /// Phase 2 of the drain: execute one descriptor. DMA-reads its payload (at
    /// `ring_base + data_offset`, or an absolute guest address when [`desc_flags::PAYLOAD_ABS`]
    /// is set — see below; both bounded to 64 MiB) and dispatches the `RESOURCE_*` op into the
    /// per-VM [`resource::ResourceTable`] (`dispatch::execute_resource`); a validated `RESOURCE_FLUSH`
    /// routes to the resource-backed present. Every malformed/rejected input is logged + dropped.
    fn execute_descriptor(&mut self, ctx: usize, desc: &infinigpu_abi::wire::Descriptor) {
        use crate::dispatch::{execute_resource, Executed};
        use infinigpu_abi::wire::desc_flags;

        const MAX_PAYLOAD: usize = 64 * 1024 * 1024;
        let len = desc.len as usize;
        if len > MAX_PAYLOAD {
            log::error!("drain ctx={ctx}: descriptor payload {len} exceeds cap — dropped");
            return;
        }
        // Out-of-line payload: a large SUBMIT_CMD body (e.g. a forwarded draw carrying the guest
        // app's SPIR-V) that doesn't fit the ring's per-slot payload region lives at an absolute
        // guest-physical address the guest wrote into `payload_addr`. The host DMA-reads it exactly
        // as it reads any other guest buffer (a scanout target, the ring itself) — same capability,
        // just a different source address. Inline payloads keep the ring-relative offset.
        let payload_addr = if desc.flags & desc_flags::PAYLOAD_ABS != 0 {
            desc.payload_addr
        } else {
            self.ring_base[ctx] + desc.data_offset as u64
        };
        let mut payload = vec![0u8; len];
        if len > 0 && !self.dma.read(payload_addr, &mut payload) {
            log::error!("drain ctx={ctx}: payload {payload_addr:#x} ({len} B) not mapped");
            return;
        }
        // A SUBMIT_CMD over the real ring carries a `SubmitCmd` header followed by the
        // encoding-specific body: route it by encoding (display present or 3D Venus submit), the
        // same command set the Phase-0 single-descriptor path serves. This is how 3D
        // (`VULKAN_VENUSLIKE`) reaches the device over the unified ring drainer.
        if desc.msg_type == infinigpu_abi::wire::msg_type::SUBMIT_CMD {
            use infinigpu_abi::wire::SubmitCmd;
            use zerocopy::FromBytes;
            match SubmitCmd::read_from_prefix(&payload) {
                Ok((sc, enc_payload)) => self.dispatch_submit(&sc, enc_payload, ctx),
                Err(_) => log::warn!("drain ctx={ctx}: short SUBMIT_CMD dropped"),
            }
            return;
        }
        // Otherwise dispatch against the resource table first (needs &mut self.resources), then act
        // on the outcome (the present needs &mut self) — no overlapping borrow.
        match execute_resource(desc, &payload, &mut self.resources) {
            Executed::Flush { res_id, rect } => {
                self.present_resource_flush(res_id, rect, desc.seqno, ctx)
            }
            Executed::Rejected(e) => log::warn!("drain ctx={ctx}: resource op rejected: {e:?}"),
            Executed::ShortPayload => log::warn!("drain ctx={ctx}: short resource payload dropped"),
            Executed::NotResource => {
                log::debug!("drain ctx={ctx}: non-resource msg_type {:#x} ignored", desc.msg_type)
            }
            ok => log::debug!("drain ctx={ctx}: {ok:?}"),
        }
    }

    /// Dispatch a `SUBMIT_CMD` drained from the real ring by its encoding — the ring-path twin of
    /// the Phase-0 `process_ring` match, so the unified drainer serves the same command set.
    /// Display encodings present; `VULKAN_VENUSLIKE` routes 3D work to the (Phase-0-spike-gated)
    /// executor. `enc_payload` is the encoding-specific body following the `SubmitCmd` header.
    fn dispatch_submit(&mut self, sc: &infinigpu_abi::wire::SubmitCmd, enc_payload: &[u8], ctx: usize) {
        use infinigpu_abi::wire::{encoding, ClearPresent, ScanoutPresent, ScanoutPresentDamaged};
        use zerocopy::FromBytes;
        match sc.encoding {
            encoding::DISPLAY_CLEAR => {
                if let Ok((cp, _)) = ClearPresent::read_from_prefix(enc_payload) {
                    self.render_clear_present(&cp, sc.seqno, ctx);
                }
            }
            encoding::DISPLAY_SCANOUT => {
                if let Ok((sp, _)) = ScanoutPresent::read_from_prefix(enc_payload) {
                    self.present_scanout(&sp, sc.seqno, ctx);
                }
            }
            encoding::DISPLAY_SCANOUT_DAMAGE => {
                // Bounded, zero-filled read (ADR-0004 forward-compat), same as the Phase-0 path.
                let mut pb = [0u8; core::mem::size_of::<ScanoutPresentDamaged>()];
                let want = if sc.payload_len == 0 {
                    pb.len()
                } else {
                    (sc.payload_len as usize).min(pb.len())
                };
                let n = want.min(enc_payload.len());
                pb[..n].copy_from_slice(&enc_payload[..n]);
                if let Ok(sp) = ScanoutPresentDamaged::read_from_bytes(&pb) {
                    self.present_scanout_damaged(&sp, sc.seqno, ctx);
                }
            }
            encoding::VULKAN_VENUSLIKE => self.submit_vulkan(sc, enc_payload, ctx),
            other => log::warn!("drain ctx={ctx}: unsupported submit encoding {:#x}", other),
        }
    }

    /// Resource-backed present (`RESOURCE_FLUSH`): the blob bound to a scanout head **is** the
    /// framebuffer. Resolve its geometry (from the `SET_SCANOUT_BLOB` binding) and its phase-1
    /// single-segment backing, then reuse the damage-present path (`present_scanout_damaged`) over
    /// the backing address — so only the damage rect is patched into the persistent per-VM
    /// `ScanoutBuffer` and streamed. Fail-closed: an unbound resource, non-contiguous/short backing,
    /// or bad geometry degrades to a skip (the fence still retires in `drain_ctx`).
    fn present_resource_flush(&mut self, res_id: u32, rect: (u32, u32, u32, u32), seqno: u64, ctx: usize) {
        use infinigpu_abi::wire::ScanoutPresentDamaged;

        // Copy out the small facts we need so we don't hold a &self.resources borrow across the
        // &mut self present.
        let (width, height, pitch, format, backing_addr) = {
            let Some((_sid, b)) = self.resources.scanout_binding_for(res_id) else {
                log::warn!("flush ctx={ctx}: res {res_id} not bound to a scanout — skipped");
                return;
            };
            let (width, height, pitch, format) = (b.width, b.height, b.stride, b.format);
            let Some(r) = self.resources.get(res_id) else { return };
            // Phase-1 shortcut: a single contiguous backing segment (dma_alloc_coherent). Multi-
            // segment scatter-gather is a later rung.
            let backing_addr = match r.backing.as_slice() {
                [seg] => seg.addr,
                _ => {
                    log::warn!("flush ctx={ctx}: res {res_id} backing not single-segment — skipped");
                    return;
                }
            };
            (width, height, pitch, format, backing_addr)
        };

        let (dx, dy, dw, dh) = rect;
        // The blob backing is just another guest framebuffer address — hand it to the existing
        // damage-present machinery (geometry-guarded, channel-correct, persistent-surface patch).
        let sp = ScanoutPresentDamaged {
            width,
            height,
            pitch,
            format,
            scanout_addr: backing_addr,
            dx,
            dy,
            dw,
            dh,
        };
        self.present_scanout_damaged(&sp, seqno, ctx);
    }

    /// 3D command submission (`encoding::VULKAN_VENUSLIKE`) — the own-remoting datapath
    /// (`docs/adr/3D-ACCEL-IMPLEMENTATION.md`, Phase-0 Step 4/5). Decode the [`VulkanWorkload`]
    /// header the guest's thin ICD wrote (`vk_op` names one hand-rolled workload; a fuller ICD
    /// appends a `vkCmd*` stream past the header), **replay it against real Vulkan on the physical
    /// GPU** via [`SharedGpu`] under the broker's fair-share run-lock, and DMA-write the resulting
    /// `R8G8B8A8` pixels to the guest scanout — the same present shape as `render_clear_present`,
    /// but the pixels come from GPU pipeline/SM execution. This is our own decoder against `ash`;
    /// it needs **no Mesa venus / virglrenderer**, so it runs on the stock host driver (no Phase-0
    /// driver-upgrade gate). Fail-closed throughout: bad admission/geometry/GPU error is logged and
    /// skipped, but the fence **always retires** so the guest's `out_fence` resolves and the ring
    /// never wedges.
    fn submit_vulkan(&mut self, sc: &infinigpu_abi::wire::SubmitCmd, payload: &[u8], ctx: usize) {
        use infinigpu_abi::wire::{vk_op, VulkanWorkload};
        use zerocopy::FromBytes;

        // Admission gate (fail-closed): a VM the broker didn't admit cannot submit 3D work.
        if !self.ensure_admitted() {
            self.ring_retired[ctx] = sc.seqno;
            return;
        }
        // Fail-closed payload bound (mirrors resource::MAX_BLOB_BYTES). A hostile/oversized
        // command stream is dropped, but the fence still retires so the ring never wedges.
        const MAX_CMD_BYTES: u32 = 64 * 1024 * 1024;
        if sc.payload_len > MAX_CMD_BYTES {
            log::warn!(
                "vulkan submit ctx={ctx}: payload {} B exceeds {} B cap — dropped",
                sc.payload_len,
                MAX_CMD_BYTES
            );
            self.ring_retired[ctx] = sc.seqno;
            return;
        }
        // Decode the fixed workload header (Phase-0 replays the named workload; a fuller ICD's
        // trailing vkCmd* stream sits past it and is ignored here). Malformed → drop, still retire.
        let Ok((wl, _)) = VulkanWorkload::read_from_prefix(payload) else {
            log::warn!("vulkan submit ctx={ctx}: short payload ({} B) — dropped", payload.len());
            self.ring_retired[ctx] = sc.seqno;
            return;
        };
        // Validate guest-controlled geometry BEFORE the GPU sees it (fail-closed; same guard as
        // render_clear_present — bound each dim and reject an overflowing width*height*4).
        let bytes = (wl.width as u64)
            .checked_mul(wl.height as u64)
            .and_then(|px| px.checked_mul(4));
        if wl.width == 0
            || wl.height == 0
            || wl.width > 16384
            || wl.height > 16384
            || bytes.is_none_or(|b| b > MAX_CMD_BYTES as u64)
        {
            log::error!("vulkan submit ctx={ctx}: bad geometry {}x{}", wl.width, wl.height);
            self.ring_retired[ctx] = sc.seqno;
            return;
        }
        // Count the validated 3D submit here (GPU-independent: it reflects the work that reached
        // the executor, so the datapath is observable even on a host whose GPU render then fails).
        let (w, h, bg, op) = (wl.width, wl.height, wl.bg, wl.op);
        self.vulkan_submits += 1;
        if self.vulkan_submits.is_multiple_of(120) || self.vulkan_submits == 1 {
            info!(
                "3D: vm={} replaying Vulkan workload op={op} {w}x{h} on the GPU → scanout {:#x} ({} total)",
                self.vm_config.vm_id, wl.scanout_addr, self.vulkan_submits
            );
            // Phase-2: pipeline-cache hit rate (Fix A). ~100% in steady state = compiles reused.
            if let Some((hits, misses, cached)) = self.shared_gpu.cache_stats() {
                let total = hits + misses;
                let rate = if total > 0 { hits as f64 / total as f64 * 100.0 } else { 0.0 };
                info!(
                    "3D: vm={} pipeline cache: {hits} hit / {misses} miss ({rate:.1}% hit), {cached} pipelines cached",
                    self.vm_config.vm_id
                );
            }
        }
        // Pre-warm the shared GPU OUTSIDE the broker's timed region (the one-time Vulkan init is
        // never billed to this tenant), then replay the named workload under the fair-share run-lock.
        let _ = self.shared_gpu.device_name();
        let gpu = Arc::clone(&self.shared_gpu);
        // Per-submit latency profiling (opt-in, env INFINIGPU_PROFILE): time the decode, the
        // broker wait vs. render (from RunStats), and the DMA writeback. `Instant` reads are a few
        // ns — negligible next to a multi-ms submit — so take them unconditionally and only fold
        // them in when the profiler is enabled.
        let t_start = std::time::Instant::now();
        let mut decode_us = 0u64;
        let scanout_addr = wl.scanout_addr; // 0 → render-only (no scanout present)
        let (wait_us, render_us, dma_us, presented) = if op == vk_op::FORWARDED {
            // Phase-1 own-ICD path: the guest ICD forwarded a real app's shaders + draw. Decode
            // the tail + SPIR-V (fail-closed on hostile input), then replay it on the GPU.
            let Some(fwd) = decode_forwarded(payload, MAX_CMD_BYTES as usize) else {
                log::warn!("vulkan submit ctx={ctx}: malformed forwarded draw — dropped");
                self.ring_retired[ctx] = sc.seqno;
                return;
            };
            decode_us = t_start.elapsed().as_micros() as u64;
            // One-copy path: dma-write the frame straight from the GPU readback mapping into the
            // guest scanout (no intermediate Frame Vec, no per-frame heap alloc — perf finding #5).
            // The present closure runs inside the timed region, so subtract it back out below.
            let dma = &self.dma;
            match self.ticket.as_ref().unwrap().run_timed(move || {
                gpu.render_forwarded_present(w, h, bg, &fwd.as_draw(), |px| {
                    scanout_addr == 0 || dma.write(scanout_addr, px)
                })
            }) {
                Ok((Some(ps), st)) => (
                    st.wait_us,
                    st.gpu_us.saturating_sub(ps.present_us),
                    ps.present_us,
                    ps.presented,
                ),
                _ => {
                    // GPU-open/render failure (already logged) — retire so the ring never wedges.
                    self.ring_retired[ctx] = sc.seqno;
                    return;
                }
            }
        } else {
            // Triangle/clear (debug + 2D): keep the Frame path, dma-write outside the timed region.
            let outcome = self.ticket.as_ref().unwrap().run_timed(move || match op {
                vk_op::TRIANGLE => gpu.render_triangle(w, h, bg),
                _ => gpu.render_clear(w, h, bg), // vk_op::CLEAR + forward-compat default
            });
            let (frame, st) = match outcome {
                Ok((Some(f), st)) => (f, st),
                _ => {
                    self.ring_retired[ctx] = sc.seqno;
                    return;
                }
            };
            let t_dma = std::time::Instant::now();
            let ok = scanout_addr == 0 || self.dma.write(scanout_addr, &frame.rgba);
            (
                st.wait_us,
                st.gpu_us,
                t_dma.elapsed().as_micros() as u64,
                ok,
            )
        };
        if !presented {
            log::error!("vulkan submit ctx={ctx}: scanout {scanout_addr:#x} not fully mapped");
        }
        if let Some(p) = self.profiler.as_mut() {
            let now = std::time::Instant::now();
            p.record(profile::HopSample {
                decode_us,
                wait_us,
                render_us,
                dma_us,
                total_us: now.saturating_duration_since(t_start).as_micros() as u64,
            });
        }
        self.ring_retired[ctx] = sc.seqno;
    }

    /// Present a guest-supplied framebuffer (the DRM/KMS page-flip path): read
    /// `pitch * height` bytes from guest RAM, count non-blank pixels (proof that
    /// real console/desktop content flowed through the device), and — if
    /// `INFINIGPU_PRESENT_DIR` is set — write the frame as a PPM so it can be viewed
    /// host-side. This is a **pure 2D scan-out**: no GPU render is implied (the guest
    /// already produced the pixels), matching PHASE-0 Step 3.
    fn present_scanout(
        &mut self,
        sp: &infinigpu_abi::wire::ScanoutPresent,
        seqno: u64,
        ctx: usize,
    ) {
        use infinigpu_abi::wire::format;

        // Admission gate (fail-closed): a VM the broker didn't admit cannot present.
        if !self.ensure_admitted() {
            return;
        }
        let (w, h, pitch) = (sp.width as usize, sp.height as usize, sp.pitch as usize);
        let total = w
            .checked_mul(4)
            .filter(|min_pitch| pitch >= *min_pitch)
            .and_then(|_| pitch.checked_mul(h));
        let total = match total {
            // Cap at 64 MiB so a hostile geometry can't make us allocate wildly.
            Some(t) if w != 0 && h != 0 && t <= 64 * 1024 * 1024 => t,
            _ => {
                log::error!("present: bad geometry {w}x{h} pitch {pitch}");
                return;
            }
        };
        // PR8: true up the broker VRAM reservation to this scanout's real footprint (once per size).
        self.account_scanout_vram(sp.width, sp.height);
        let mut buf = vec![0u8; total];
        if !self.dma.read(sp.scanout_addr, &mut buf) {
            log::error!(
                "present: framebuffer {:#x} ({total} bytes) not mapped",
                sp.scanout_addr
            );
            return;
        }
        // A full-frame present invalidates any damage surface (this is the resize/modeset
        // path in practice) and reads the whole framebuffer.
        self.last_scanout = None;
        self.bytes_read_last = total;

        // Byte order of one 32-bpp pixel. fbcon's XRGB8888 is little-endian
        // [B,G,R,X]; R8G8B8A8 is [R,G,B,A].
        let (ri, gi, bi) = match sp.format {
            format::R8G8B8A8 => (0, 1, 2),
            _ => (2, 1, 0), // B8G8R8A8 / B8G8R8X8 (fbcon default)
        };
        let mut rgb = vec![0u8; w * h * 3];
        // When streaming, also produce a tightly-packed BGRA frame for the encoder.
        let streaming = self.pixel.is_some();
        let mut bgra = if streaming { vec![0u8; w * h * 4] } else { Vec::new() };
        let mut nonblank = 0usize;
        for y in 0..h {
            let row = &buf[y * pitch..y * pitch + w * 4];
            for x in 0..w {
                let px = &row[x * 4..x * 4 + 4];
                let (r, g, b) = (px[ri], px[gi], px[bi]);
                if r | g | b != 0 {
                    nonblank += 1;
                }
                let o = (y * w + x) * 3;
                rgb[o] = r;
                rgb[o + 1] = g;
                rgb[o + 2] = b;
                if streaming {
                    let o4 = (y * w + x) * 4;
                    bgra[o4] = b;
                    bgra[o4 + 1] = g;
                    bgra[o4 + 2] = r;
                    bgra[o4 + 3] = 255;
                }
            }
        }

        self.present_count += 1;
        let pct = 100.0 * nonblank as f64 / (w * h).max(1) as f64;
        info!(
            "present: frame {} {w}x{h} pitch {pitch} @ {:#x} — {nonblank} non-blank px ({pct:.1}%)",
            self.present_count, sp.scanout_addr
        );
        if let Some(dir) = self.present_dir.clone() {
            // Keep the first few numbered frames plus an always-current one.
            if self.present_count <= 12 {
                let _ = self.write_ppm(&dir.join(format!("frame-{:04}.ppm", self.present_count)), w, h, &rgb);
            }
            let _ = self.write_ppm(&dir.join("latest.ppm"), w, h, &rgb);
        }

        // Publish completion FIRST — the guest's framebuffer has already been fully consumed
        // (DMA-read + converted into `bgra`/`rgb` above), so retire this ring's seqno and let
        // the guest's non-posted scanout doorbell return immediately, BEFORE the encode handoff.
        // This keeps the guest vCPU (and QEMU's BQL) from being parked across any downstream
        // streamer cost, which would stall the QMP monitor and freeze input (mouse-lag-hunt).
        // (The guest polls this retired register; the MSI-X is raised by the doorbell handler.)
        self.ring_retired[ctx] = seqno;

        // infiniPixel: encode this framebuffer on NVENC and stream it to any browsers.
        if streaming {
            self.stream_frame(&bgra, w as u32, h as u32);
            if self.present_count.is_multiple_of(60) {
                if let Some(p) = &self.pixel {
                    let (sent, skipped) = p.stats();
                    info!(
                        "infiniPixel: {sent} frames encoded, {skipped} idle-skipped (unchanged); \
                         last present read {} KiB from guest",
                        self.bytes_read_last / 1024
                    );
                }
            }
        }
    }

    /// Accelerated 2D present (`DISPLAY_SCANOUT_DAMAGE`): patch only the guest-reported
    /// damage rect into the persistent [`ScanoutBuffer`] and stream the whole surface.
    ///
    /// The encoder still receives a complete BGRA frame (its P-frames encode only the delta),
    /// but the device DMA-reads + repacks just the `dw*dh*4` dirty bytes instead of the whole
    /// `pitch*height` framebuffer — the win during drags/scrolls. Fail-closed on every guest-
    /// controlled value: bad geometry is rejected, the damage rect is clamped into the frame,
    /// and a resize/first-frame/stale buffer forces a full rebuild (never partial-frame
    /// corruption). If any damaged row is unmapped we bail **without** retiring, so the guest
    /// can re-present rather than seeing a torn surface.
    fn present_scanout_damaged(
        &mut self,
        sp: &infinigpu_abi::wire::ScanoutPresentDamaged,
        seqno: u64,
        ctx: usize,
    ) {
        use infinigpu_abi::wire::format;

        // Admission gate (fail-closed): a VM the broker didn't admit cannot present.
        if !self.ensure_admitted() {
            return;
        }
        let (w, h, pitch) = (sp.width as usize, sp.height as usize, sp.pitch as usize);
        // Same fail-closed geometry guard as `present_scanout`: pitch covers a row, the
        // framebuffer fits in 64 MiB, non-zero dims.
        let fb_bytes = w
            .checked_mul(4)
            .filter(|min_pitch| pitch >= *min_pitch)
            .and_then(|_| pitch.checked_mul(h));
        let fb_bytes = match fb_bytes {
            Some(t) if w != 0 && h != 0 && t <= 64 * 1024 * 1024 => t,
            _ => {
                log::error!("present(dmg): bad geometry {w}x{h} pitch {pitch}");
                return;
            }
        };
        // PR8: true up the broker VRAM reservation to this scanout's real footprint (once per size).
        self.account_scanout_vram(sp.width, sp.height);

        // Clamp the guest damage rect into [0,w]×[0,h] (see `clamp_damage`): a hostile
        // origin or width can never index outside the frame.
        let (dx, dy, dw, dh) = clamp_damage(w, h, sp.dx, sp.dy, sp.dw, sp.dh);

        // One pixel's source byte order → BGRA. fbcon XRGB8888 is [B,G,R,X]; R8G8B8A8 is [R,G,B,A].
        let (ri, gi, bi) = match sp.format {
            format::R8G8B8A8 => (0usize, 1usize, 2usize),
            _ => (2usize, 1usize, 0usize),
        };

        // Full rebuild when there is no matching surface (first frame / resize / post-reset).
        let need_full = !matches!(&self.last_scanout, Some(b) if b.w == w && b.h == h);

        if need_full {
            let mut buf = vec![0u8; fb_bytes];
            if !self.dma.read(sp.scanout_addr, &mut buf) {
                log::error!("present(dmg): framebuffer {:#x} not mapped", sp.scanout_addr);
                return;
            }
            let mut bgra = vec![0u8; w * h * 4];
            for y in 0..h {
                let row = &buf[y * pitch..y * pitch + w * 4];
                for x in 0..w {
                    let px = &row[x * 4..x * 4 + 4];
                    let o4 = (y * w + x) * 4;
                    bgra[o4] = px[bi];
                    bgra[o4 + 1] = px[gi];
                    bgra[o4 + 2] = px[ri];
                    bgra[o4 + 3] = 255;
                }
            }
            self.last_scanout = Some(ScanoutBuffer { w, h, bgra });
            self.bytes_read_last = fb_bytes;
        } else if dw > 0 && dh > 0 {
            // Incremental: DMA-read ALL `dh` damaged rows (each `dw*4` bytes) into a scratch first;
            // only once every read has succeeded do we repack them into the persistent surface — so
            // a mid-rect DMA failure leaves the surface untouched (no partial/torn update) and we
            // don't retire, letting the guest re-present. (Previously the writes were interleaved
            // with the reads, so a failure left the already-processed rows applied — audit finding.)
            let mut rows = vec![0u8; dw * dh * 4];
            for r in 0..dh {
                let y = dy + r;
                let src = sp.scanout_addr + (y as u64) * (pitch as u64) + (dx as u64) * 4;
                if !self.dma.read(src, &mut rows[r * dw * 4..(r + 1) * dw * 4]) {
                    log::error!("present(dmg): damaged row {y} @ {src:#x} not mapped");
                    return;
                }
            }
            // `unwrap` is sound — `need_full` was false, so `last_scanout` is `Some`.
            let b = self.last_scanout.as_mut().unwrap();
            for r in 0..dh {
                let y = dy + r;
                let dst_row = (y * w + dx) * 4;
                let row_buf = &rows[r * dw * 4..(r + 1) * dw * 4];
                for x in 0..dw {
                    let px = &row_buf[x * 4..x * 4 + 4];
                    let o4 = dst_row + x * 4;
                    b.bgra[o4] = px[bi];
                    b.bgra[o4 + 1] = px[gi];
                    b.bgra[o4 + 2] = px[ri];
                    b.bgra[o4 + 3] = 255;
                }
            }
            self.bytes_read_last = dw * dh * 4;
        } else {
            // Empty damage after clamp: nothing changed — re-stream the persistent surface
            // (so a freshly-connected viewer still gets a frame) without reading guest RAM.
            self.bytes_read_last = 0;
        }

        self.present_count += 1;
        if self.present_count.is_multiple_of(120) {
            info!(
                "present(dmg): frame {} {w}x{h} damage {dw}x{dh}@({dx},{dy}) — read {} KiB",
                self.present_count,
                self.bytes_read_last / 1024
            );
        }
        if let Some(dir) = self.present_dir.clone() {
            if let Some(b) = &self.last_scanout {
                // Diagnostic PPM from the persistent BGRA surface (BGRA→RGB), dev-only.
                let mut rgb = vec![0u8; w * h * 3];
                for i in 0..w * h {
                    rgb[i * 3] = b.bgra[i * 4 + 2];
                    rgb[i * 3 + 1] = b.bgra[i * 4 + 1];
                    rgb[i * 3 + 2] = b.bgra[i * 4];
                }
                let _ = self.write_ppm(&dir.join("latest.ppm"), w, h, &rgb);
            }
        }

        // Retire BEFORE streaming — the guest framebuffer is fully consumed into the
        // persistent surface, so let the scanout doorbell return immediately (keeps the
        // guest vCPU / QEMU BQL off the encode handoff; see `present_scanout`).
        self.ring_retired[ctx] = seqno;

        // Stream the whole persistent surface. Borrow `last_scanout` (shared) and `pixel`
        // (mut) as disjoint fields directly so the borrow checker allows both at once.
        if let (Some(b), Some(p)) = (self.last_scanout.as_ref(), self.pixel.as_mut()) {
            if let Err(e) = p.submit_bgra(&b.bgra, b.w as u32, b.h as u32) {
                if self.present_count.is_multiple_of(60) {
                    log::warn!("infiniPixel: frame submit failed ({e}); stream may be down");
                }
            }
        }
    }

    /// Decode + forward one guest `CURSOR_UPDATE` to the client-side cursor overlay (the `XIPL`
    /// plane sideband). Runs on the single vfio-user callback thread, so it is fully **fail-closed
    /// and rate-limited**: a bad geometry / wrong format / oversized sprite is dropped (never
    /// forwarded, never OOB), a shape-DEFINE flood is coalesced (defeats the de-dup-alternation
    /// DoS), and pure MOVEs are suppressed for a single driving viewer (which draws the cursor at
    /// its own local pointer) and coalesced otherwise. It **never** touches the encoder — the
    /// sideband is a separate Hub lane — so it can never stall the guest scanout doorbell.
    fn handle_cursor_update(&mut self, cu: &infinigpu_abi::wire::CursorUpdate) {
        use infinigpu_abi::wire::{cursor_flags, format};
        use infinigpu_pixel::{proto::plane, PlaneHeader};

        if !self.ensure_admitted() || self.pixel.is_none() {
            return;
        }

        let visible = cu.flags & cursor_flags::VISIBLE != 0;
        let move_only = cu.flags & cursor_flags::MOVE_ONLY != 0;
        let warp = cu.flags & cursor_flags::WARP != 0;

        // Repack the ABI cursor_flags into the sideband subset the viewer reads.
        let mut sflags = 0u8;
        if visible {
            sflags |= plane::flags::VISIBLE;
        }
        if cu.flags & cursor_flags::PREMULTIPLIED != 0 {
            sflags |= plane::flags::PREMULTIPLIED;
        }
        if warp {
            sflags |= plane::flags::WARP;
        }
        if cu.flags & cursor_flags::RELATIVE != 0 {
            sflags |= plane::flags::RELATIVE;
        }

        // A header-only MOVE (position / visibility), for the hide + coalesced-move paths.
        let move_hdr = PlaneHeader {
            op: plane::op::MOVE,
            plane_kind: plane::kind::CURSOR,
            flags: sflags,
            plane_id: 0,
            codec_or_format: 0,
            z_order: 0,
            width: 0,
            height: 0,
            hot_x: cu.hot_x,
            hot_y: cu.hot_y,
            pos_x: cu.pos_x,
            pos_y: cu.pos_y,
            payload_len: 0,
        };

        // Hide (VISIBLE clear): always forward a header-only MOVE so the viewer drops its overlay
        // and reverts to the baked stream cursor (the D4 compositor HW->SW handoff) — never a stale
        // overlay, never a double cursor.
        if !visible {
            if let Some(p) = self.pixel.as_ref() {
                p.send_plane(&move_hdr, &[]);
            }
            self.cursor.last_visible = false;
            self.cursor.last_pos = Some((cu.pos_x, cu.pos_y));
            return;
        }

        if move_only {
            let pos = (cu.pos_x, cu.pos_y);
            // A single driving viewer renders the cursor at its own local pointer → it needs no
            // MOVE. Suppress unless there are extra (view-only) viewers or this is a warp teleport.
            let extra_viewers = self.pixel.as_ref().map_or(false, |p| p.client_count() > 1);
            if !warp && !extra_viewers {
                self.cursor.last_pos = Some(pos);
                self.cursor.last_visible = true;
                return;
            }
            let now = Instant::now();
            if !warp {
                if self.cursor.last_pos == Some(pos) {
                    return; // unchanged position
                }
                if let Some(t) = self.cursor.last_move_sent {
                    if now.duration_since(t) < CURSOR_MOVE_MIN_INTERVAL {
                        return; // coalesce routine moves
                    }
                }
            }
            if let Some(p) = self.pixel.as_ref() {
                p.send_plane(&move_hdr, &[]);
            }
            self.cursor.last_pos = Some(pos);
            self.cursor.last_move_sent = Some(now);
            self.cursor.last_visible = true;
            return;
        }

        // A shape DEFINE. Validate the guest-controlled geometry fail-closed (see `cursor_shape_ok`).
        let (w, h, pitch) = (cu.width as usize, cu.height as usize, cu.pitch as usize);
        if !cursor_shape_ok(cu.width, cu.height, cu.pitch, cu.format) {
            log::warn!("cursor: bad shape {w}x{h} pitch {pitch} fmt {} — dropped", cu.format);
            return;
        }
        if cu.flags & cursor_flags::SHAPE_BY_RESID != 0 {
            // res_id shapes need PR4's ResourceTable; not resolvable yet — drop.
            return;
        }

        let key = (cu.shape_ref, cu.width, cu.height, cu.pitch);
        if self.cursor.last_shape == Some(key) {
            // Same shape re-DEFINE (a move that re-sent the shape): treat as a move — don't re-read.
            if let Some(p) = self.pixel.as_ref() {
                p.send_plane(&move_hdr, &[]);
            }
            self.cursor.last_pos = Some((cu.pos_x, cu.pos_y));
            self.cursor.last_visible = true;
            return;
        }

        // Rate-limit the DMA-read + repack so a hostile alternating-shape flood can't peg us.
        let now = Instant::now();
        if let Some(t) = self.cursor.last_shape_read {
            if now.duration_since(t) < CURSOR_SHAPE_MIN_INTERVAL {
                return; // coalesce: drop this read; a later one within budget will forward
            }
        }

        // DMA-read the ARGB sprite and de-pitch it into a tightly-packed body. The DRM cursor
        // format ARGB8888 == our B8G8R8A8 byte order, so this strips pitch padding — no swap.
        let mut src = vec![0u8; pitch * h];
        if !self.dma.read(cu.shape_ref, &mut src) {
            log::warn!("cursor: sprite {:#x} ({} bytes) not mapped", cu.shape_ref, pitch * h);
            return;
        }
        let mut bgra = vec![0u8; w * h * 4];
        for y in 0..h {
            bgra[y * w * 4..(y + 1) * w * 4].copy_from_slice(&src[y * pitch..y * pitch + w * 4]);
        }
        let hdr = PlaneHeader {
            op: plane::op::DEFINE,
            plane_kind: plane::kind::CURSOR,
            flags: sflags,
            plane_id: 0,
            codec_or_format: format::B8G8R8A8 as u8,
            z_order: 0,
            width: cu.width,
            height: cu.height,
            hot_x: cu.hot_x,
            hot_y: cu.hot_y,
            pos_x: cu.pos_x,
            pos_y: cu.pos_y,
            payload_len: (w * h * 4) as u32,
        };
        if let Some(p) = self.pixel.as_ref() {
            p.send_plane(&hdr, &bgra);
        }
        self.cursor.last_shape = Some(key);
        self.cursor.last_shape_read = Some(now);
        self.cursor.last_pos = Some((cu.pos_x, cu.pos_y));
        self.cursor.last_visible = true;
    }

    /// Feed one presented frame to the infiniPixel stream. The streamer (re)sizes its
    /// encoder to match, so the 128×128 KMS self-test and the real console resolution
    /// both stream correctly.
    fn stream_frame(&mut self, bgra: &[u8], w: u32, h: u32) {
        if let Some(p) = self.pixel.as_mut() {
            // submit_bgra is now non-blocking (latest-wins mailbox), so this never stalls
            // the vfio-user callback thread on encode. An Err means the encoder couldn't
            // be (re)spawned at all — surface it (rate-limited) instead of silently
            // dropping a dead stream (verify finding).
            if let Err(e) = p.submit_bgra(bgra, w, h) {
                if self.present_count.is_multiple_of(60) {
                    log::warn!("infiniPixel: frame submit failed ({e}); stream may be down");
                }
            }
        }
    }

    fn write_ppm(&self, path: &Path, w: usize, h: usize, rgb: &[u8]) -> io::Result<()> {
        let mut f = File::create(path)?;
        write!(f, "P6\n{w} {h}\n255\n")?;
        f.write_all(rgb)
    }

    fn render_clear_present(
        &mut self,
        cp: &infinigpu_abi::wire::ClearPresent,
        seqno: u64,
        ctx: usize,
    ) {
        log::debug!(
            "  render_clear_present {}x{} scanout={:#x}",
            cp.width,
            cp.height,
            cp.scanout_addr
        );
        // Admission gate (fail-closed) then run the render under the broker's
        // fair-share scheduler: it blocks on token-bucket back-pressure, serializes on
        // the GPU run-lock, and debits the measured GPU-time to this VM.
        if !self.ensure_admitted() {
            return;
        }
        // Validate guest-controlled geometry BEFORE the GPU sees it (fail-closed;
        // mirrors present_scanout). Keeps an overflowing width×height from ever
        // panicking inside the shared run-lock and bricking the fleet (verify finding).
        // The byte count is computed with checked_mul: `width*height*4` on hostile u32
        // dimensions overflows u64 (panicking under debug overflow-checks, silently
        // wrapping past the cap under release) — so an overflow must map to "reject",
        // not to a multiply. Each dimension is also bounded directly so the extent
        // handed to Vulkan is capped here, not only by the downstream driver's limit.
        let bytes = (cp.width as u64)
            .checked_mul(cp.height as u64)
            .and_then(|px| px.checked_mul(4));
        if cp.width == 0
            || cp.height == 0
            || cp.width > 16384
            || cp.height > 16384
            || bytes.is_none_or(|b| b > 64 * 1024 * 1024)
        {
            log::error!("render_clear: bad geometry {}x{}", cp.width, cp.height);
            return;
        }
        // Pre-warm the shared GPU context OUTSIDE the broker's timed region so the
        // one-time Vulkan init is never billed to this tenant's GPU-time (verify
        // finding #3). Idempotent: only the first admitted VM host-wide pays it.
        let _ = self.shared_gpu.device_name();
        let gpu = Arc::clone(&self.shared_gpu);
        let (w, h, rgba) = (cp.width, cp.height, cp.rgba);
        let frame = match self.ticket.as_ref().unwrap().run(move || gpu.render_clear(w, h, rgba)) {
            Ok(Some(f)) => f,
            _ => return, // GPU error, or (unreachable here) not admitted
        };
        info!(
            "seqno {seqno}: vm={} rendered {}x{} on the GPU → scanout {:#x}",
            self.vm_config.vm_id, cp.width, cp.height, cp.scanout_addr
        );
        if !self.dma.write(cp.scanout_addr, &frame.rgba) {
            log::error!("scanout {:#x} not fully mapped", cp.scanout_addr);
            return;
        }
        // Publish completion: the guest polls this ring's retired register (a non-posted
        // read, so it also flushes the doorbell write) and/or takes the MSI-X raised by
        // the doorbell handler.
        self.ring_retired[ctx] = seqno;
    }
}

impl ServerBackend for InfinigpuBackend {
    fn region_read(&mut self, region: u32, offset: u64, data: &mut [u8]) -> io::Result<()> {
        if region == VFIO_PCI_CONFIG_REGION_INDEX {
            for (i, b) in data.iter_mut().enumerate() {
                *b = self.config.get(offset as usize + i).copied().unwrap_or(0);
            }
            if offset == 0 && data.len() >= 4 {
                info!(
                    "config read @0x00 (PCI enumeration): {:#06x}:{:#06x}",
                    u16::from_le_bytes([data[0], data[1]]),
                    u16::from_le_bytes([data[2], data[3]])
                );
            }
        } else if region == VFIO_PCI_BAR0_REGION_INDEX {
            self.read_bar0(offset, data);
        } else {
            data.fill(0);
        }
        Ok(())
    }

    fn region_write(&mut self, region: u32, offset: u64, data: &[u8]) -> io::Result<()> {
        if region == VFIO_PCI_CONFIG_REGION_INDEX {
            for (i, b) in data.iter().enumerate() {
                let idx = offset as usize + i;
                if idx < self.config.len() {
                    self.config[idx] = *b;
                }
            }
        } else if region == VFIO_PCI_BAR0_REGION_INDEX {
            log::debug!("BAR0 write off={:#06x} len={}", offset, data.len());
            self.write_bar0(offset, data);
        }
        Ok(())
    }

    fn dma_map(
        &mut self,
        _flags: DmaMapFlags,
        offset: u64,
        address: u64,
        size: u64,
        fd: Option<File>,
    ) -> io::Result<()> {
        match fd {
            Some(f) => {
                info!("DMA_MAP iova={address:#x} size={size:#x} (guest RAM mapped zero-copy)");
                self.dma.map(address, offset, size, f)
            }
            // No fd: a region QEMU can't share by fd (BIOS/ROM shadow, MMIO holes).
            // Accept as a no-op — we simply never map it, so any guest DMA into it
            // fails closed at translation time. The device never needs these.
            None => {
                log::debug!(
                    "DMA_MAP iova={address:#x} size={size:#x} without fd — not mappable, ignoring"
                );
                Ok(())
            }
        }
    }

    fn dma_unmap(&mut self, flags: DmaUnmapFlags, address: u64, _size: u64) -> io::Result<()> {
        info!(
            "DMA_UNMAP iova={address:#x} all={}",
            flags.contains(DmaUnmapFlags::UNMAP_ALL)
        );
        if flags.contains(DmaUnmapFlags::UNMAP_ALL) {
            self.dma.clear();
        } else {
            self.dma.unmap(address);
        }
        Ok(())
    }

    fn reset(&mut self) -> io::Result<()> {
        info!("DEVICE_RESET");
        self.reset_state();
        Ok(())
    }

    fn set_irqs(
        &mut self,
        index: u32,
        flags: u32,
        start: u32,
        count: u32,
        fds: Vec<File>,
    ) -> io::Result<()> {
        if index != VFIO_PCI_MSIX_IRQ_INDEX {
            return Ok(());
        }
        info!(
            "SET_IRQS msix start={start} count={count} fds={}",
            fds.len()
        );
        if flags & (VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER) != 0 && !fds.is_empty()
        {
            for (i, f) in fds.into_iter().enumerate() {
                let v = start as usize + i;
                if v < self.msix.len() {
                    self.msix[v] = Some(f);
                }
            }
        } else {
            // Disable the requested range.
            for v in start..start + count {
                if let Some(slot) = self.msix.get_mut(v as usize) {
                    *slot = None;
                }
            }
        }
        Ok(())
    }
}

/// The 9 PCI regions (config + BARs). BAR0 = control/index/doorbell, BAR1 = MSI-X.
pub fn build_regions() -> Vec<ServerRegion> {
    let rw = VFIO_REGION_INFO_FLAG_READ | VFIO_REGION_INFO_FLAG_WRITE;
    (0..VFIO_PCI_NUM_REGIONS)
        .map(|index| {
            let mut ri = vfio_region_info {
                argsz: size_of::<vfio_region_info>() as u32,
                index,
                ..Default::default()
            };
            if index == VFIO_PCI_BAR0_REGION_INDEX {
                ri.size = regs::BAR0_SIZE;
                ri.flags = rw;
            } else if index == VFIO_PCI_BAR1_REGION_INDEX {
                ri.size = regs::BAR1_SIZE;
                ri.flags = rw;
            } else if index == VFIO_PCI_CONFIG_REGION_INDEX {
                ri.size = config::CONFIG_SPACE_SIZE as u64;
                ri.flags = rw;
            }
            ServerRegion {
                region_info: ri,
                sparse_areas: Vec::new(),
                mmap_fd: None,
            }
        })
        .collect()
}

/// IRQ descriptors: MSI-X with [`config::MSIX_VECTORS`] eventfd vectors.
pub fn build_irqs() -> Vec<IrqInfo> {
    (0..VFIO_PCI_NUM_IRQS)
        .map(|index| {
            let mut irq = IrqInfo {
                index,
                count: 0,
                flags: 0,
            };
            if index == VFIO_PCI_MSIX_IRQ_INDEX {
                irq.count = config::MSIX_VECTORS as u32;
                irq.flags = VFIO_IRQ_INFO_EVENTFD;
            }
            irq
        })
        .collect()
}

/// Bind a server at `socket_path` and serve one QEMU connection (blocking). Standalone
/// single-VM: its own broker + GPU context.
pub fn serve(socket_path: &Path) -> Result<(), vfio_user::Error> {
    let server = Server::new(socket_path, true, build_irqs(), build_regions())?;
    let shared_gpu = SharedGpu::new();
    // Pre-warm the GPU concurrently with the guest's PCI probe. The first ring submission would
    // otherwise pay the one-time HostGpu::open() (Vulkan init) synchronously on the vfio-user
    // callback thread, parking QEMU past the guest's submit timeout. Opening it on a side thread
    // now means it is usually ready before the first doorbell arrives. (The proper fix — running
    // the whole GPU drain on a worker thread so the callback never blocks — is a follow-up.)
    {
        let gpu = Arc::clone(&shared_gpu);
        std::thread::spawn(move || {
            if let Some(name) = gpu.device_name() {
                info!("GPU pre-warmed on a side thread: {name}");
            }
        });
    }
    let broker = GpuBroker::with_real_clock(broker_config_from_nvml());
    let mut backend =
        InfinigpuBackend::with_broker(broker, shared_gpu, VmConfig::new("vm", 1, 4096));
    server.run(&mut backend)
}

/// Serve one QEMU connection for a VM that **shares** a `broker` + `shared_gpu` with
/// other VMs (the multi-VM path — see `infinigpu-broker`). Blocks until the guest
/// disconnects; on return the backend drops and its admission ticket reaps.
pub fn serve_with_broker(
    socket_path: &Path,
    broker: Arc<GpuBroker>,
    shared_gpu: Arc<SharedGpu>,
    vm_config: VmConfig,
) -> Result<(), vfio_user::Error> {
    let server = Server::new(socket_path, true, build_irqs(), build_regions())?;
    let mut backend = InfinigpuBackend::with_broker(broker, shared_gpu, vm_config);
    server.run(&mut backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use infinigpu_abi::regs::{ctrl, CMD_RING_STRIDE};

    fn base_off(ctx: u64) -> u64 {
        ctrl::CMD_RING_CFG + ctx * CMD_RING_STRIDE + ctrl::CMD_RING_BASE_LO
    }
    fn retired_off(ctx: u64) -> u64 {
        ctrl::CMD_RING_CFG + ctx * CMD_RING_STRIDE + ctrl::CMD_RING_RETIRED_LO
    }

    // Compile-time invariant: multi-ring must serve >1 context.
    const _: () = assert!(NUM_CONTEXTS >= 2, "multi-ring must serve >1 context");

    #[test]
    fn advertises_multiple_command_rings() {
        let mut b = InfinigpuBackend::new();
        assert_eq!(b.bar0_read_u32(ctrl::NUM_CONTEXTS), NUM_CONTEXTS as u32);
    }

    #[test]
    fn per_context_ring_base_is_independent() {
        let mut b = InfinigpuBackend::new();
        // Program three rings with distinct 64-bit bases via their per-context blocks.
        for ctx in 0..3u64 {
            let base = 0x1000_0000u64 * (ctx + 1) + 0x40;
            b.bar0_write_u32(base_off(ctx), (base & 0xFFFF_FFFF) as u32);
            b.bar0_write_u32(base_off(ctx) + 4, (base >> 32) as u32);
        }
        for ctx in 0..3u64 {
            let want = 0x1000_0000u64 * (ctx + 1) + 0x40;
            let lo = b.bar0_read_u32(base_off(ctx)) as u64;
            let hi = b.bar0_read_u32(base_off(ctx) + 4) as u64;
            assert_eq!((hi << 32) | lo, want, "ring {ctx} base read-back");
        }
        assert_ne!(b.ring_base[0], b.ring_base[1], "rings are independent");
        assert_ne!(b.ring_base[1], b.ring_base[2]);
    }

    #[test]
    fn ring0_retired_mirrored_at_legacy_and_per_context_registers() {
        let mut b = InfinigpuBackend::new();
        b.ring_retired[0] = 0xDEAD_BEEF_0000_0007;
        b.ring_retired[3] = 0x0000_0000_0000_002A;
        // Ring 0 via the fixed Phase-0 register…
        assert_eq!(b.bar0_read_u32(ctrl::CMD_RING0_RETIRED_LO), 0x0000_0007);
        assert_eq!(b.bar0_read_u32(ctrl::CMD_RING0_RETIRED_HI), 0xDEAD_BEEF);
        // …and via ring 0's per-context block…
        assert_eq!(b.bar0_read_u32(retired_off(0)), 0x0000_0007);
        // …while ring 3 reports its own retired seqno, independently.
        assert_eq!(b.bar0_read_u32(retired_off(3)), 0x0000_002A);
    }

    #[test]
    fn device_reset_clears_all_rings() {
        let mut b = InfinigpuBackend::new();
        b.ring_base[2] = 0xABCD;
        b.ring_retired[2] = 9;
        b.reset_state();
        assert_eq!(b.ring_base[2], 0);
        assert_eq!(b.ring_retired[2], 0);
    }

    #[test]
    fn advertises_2d_display_accel() {
        // The guest reads DEV_CAPS to decide whether to send DISPLAY_SCANOUT_DAMAGE.
        let mut b = InfinigpuBackend::new();
        let caps = b.bar0_read_u32(ctrl::DEV_CAPS);
        assert!(
            caps & regs::caps::DISPLAY_ACCEL != 0,
            "device must advertise DISPLAY_ACCEL for the 2D damage path"
        );
        // Still a Phase-0 superset — the base submission model is unchanged.
        assert!(caps & regs::caps::POLL_SUBMIT != 0);
    }

    #[test]
    fn cursor_shape_ok_is_fail_closed() {
        use infinigpu_abi::wire::format;
        // A normal ARGB cursor (tight or padded pitch) is accepted.
        assert!(cursor_shape_ok(32, 32, 32 * 4, format::B8G8R8A8));
        assert!(cursor_shape_ok(24, 24, 128, format::B8G8R8A8));
        // Exactly at the 256×256 byte cap is accepted; one row of padding past it is not.
        assert!(cursor_shape_ok(256, 256, 256 * 4, format::B8G8R8A8));
        assert!(!cursor_shape_ok(256, 256, 256 * 4 + 4, format::B8G8R8A8));
        // Zero dims, oversized dims, wrong format, and a pitch too small for the row are rejected.
        assert!(!cursor_shape_ok(0, 16, 64, format::B8G8R8A8));
        assert!(!cursor_shape_ok(16, 0, 64, format::B8G8R8A8));
        assert!(!cursor_shape_ok(257, 16, 257 * 4, format::B8G8R8A8));
        assert!(!cursor_shape_ok(32, 32, 128, format::R8G8B8A8));
        assert!(!cursor_shape_ok(32, 32, 32 * 4 - 1, format::B8G8R8A8));
        // A hostile u32::MAX pitch must be rejected without overflowing (no panic).
        assert!(!cursor_shape_ok(256, 256, u32::MAX, format::B8G8R8A8));
    }

    #[test]
    fn clamp_damage_never_exceeds_frame() {
        // A well-formed rect passes through unchanged.
        assert_eq!(clamp_damage(1920, 1080, 10, 20, 100, 50), (10, 20, 100, 50));
        // dx+dw past the right edge → dw shrunk so the rect ends exactly at the edge.
        let (dx, _, dw, _) = clamp_damage(1920, 1080, 1900, 0, 100, 10);
        assert!(dx + dw <= 1920 && dw == 20);
        // Origin beyond the frame → zero-area, still fully in bounds (no underflow).
        let (dx, dy, dw, dh) = clamp_damage(1920, 1080, 5000, 5000, 100, 100);
        assert!(dx <= 1920 && dy <= 1080 && dx + dw <= 1920 && dy + dh <= 1080);
        // Hostile u32::MAX dimensions must not overflow into an OOB index.
        let (dx, dy, dw, dh) = clamp_damage(1920, 1080, u32::MAX, u32::MAX, u32::MAX, u32::MAX);
        assert!(dx + dw <= 1920 && dy + dh <= 1080);
        // Full-frame damage (the guest's "no damage known" / first-flip sentinel) is preserved.
        assert_eq!(clamp_damage(1920, 1080, 0, 0, 1920, 1080), (0, 0, 1920, 1080));
    }

    #[test]
    fn vulkan_submit_retires_fence_and_is_bounded() {
        use infinigpu_abi::wire::{encoding, vk_op, SubmitCmd, VulkanWorkload};
        use zerocopy::IntoBytes;
        let mut b = InfinigpuBackend::new();
        // A well-formed workload header: CLEAR, small, scanout_addr=0 → render-only (no writeback).
        // The counter advances on any VALIDATED submit (before the GPU render), so this asserts the
        // datapath/fail-closed logic without needing a GPU; the actual render + DMA-writeback is
        // exercised by the #[ignore]'d E2E test `vulkan_workload_renders_a_triangle_to_scanout`.
        let wl = VulkanWorkload {
            op: vk_op::CLEAR,
            width: 16,
            height: 16,
            _pad: 0,
            bg: [0.0; 4],
            scanout_addr: 0,
        };
        let payload = wl.as_bytes().to_vec();
        let mk = |seqno: u64, payload_len: u32| SubmitCmd {
            ctx_id: 0,
            encoding: encoding::VULKAN_VENUSLIKE,
            payload_len,
            flags: 0,
            seqno,
            in_fence: 0,
            out_fence: seqno,
        };
        // A well-formed 3D submit: the fence retires (guest's submit thread progresses) and the
        // datapath counter advances — proof a Vulkan workload reached the executor.
        b.submit_vulkan(&mk(7, payload.len() as u32), &payload, 1);
        assert_eq!(b.ring_retired[1], 7, "out_fence must retire so the guest progresses");
        assert_eq!(b.vulkan_submits, 1);
        // An oversized command stream is dropped fail-closed (not counted) but must NOT stall the
        // ring — the fence still retires to the new seqno.
        b.submit_vulkan(&mk(9, 64 * 1024 * 1024 + 1), &payload, 1);
        assert_eq!(b.vulkan_submits, 1, "oversized submit is not counted as executed");
        assert_eq!(b.ring_retired[1], 9, "even a dropped submit retires its fence (no ring stall)");
        // A too-short payload (can't even decode the header) is dropped fail-closed, still retires.
        b.submit_vulkan(&mk(11, 8), &[0u8; 8], 1);
        assert_eq!(b.vulkan_submits, 1, "short payload is not counted");
        assert_eq!(b.ring_retired[1], 11, "short submit still retires its fence");
        // A device reset zeroes the per-attach counter.
        b.reset_state();
        assert_eq!(b.vulkan_submits, 0);
    }

    /// PR4 accept criterion, end-to-end over memfd-backed guest RAM (no QEMU): program a real
    /// DMA-resident ring, publish CREATE_BLOB → ATTACH_BACKING → SET_SCANOUT_BLOB → RESOURCE_FLUSH
    /// through the SPSC producer, ring the doorbell, and assert the drainer retired all N, drained
    /// the ring (head==tail, seqno_retired==N), registered the resource + scanout, and produced a
    /// present whose BGRA matches the blob framebuffer. Also checks per-VM isolation + fail-closed.
    #[test]
    fn pr4_real_ring_drain_presents_a_blob_resource() {
        use infinigpu_abi::wire::{
            format, msg_type, AttachBacking, Descriptor, MemEntry, ResourceCreateBlob, ResourceFlush,
            SetScanoutBlob,
        };
        use std::os::unix::io::FromRawFd;
        use std::ptr;
        use zerocopy::IntoBytes;

        const GUEST_BASE: u64 = 0x8000_0000;
        const SIZE: usize = 0x10000;
        const IDX_OFF: usize = 0x0;
        const DESC_OFF: usize = 0x40;
        const PAY_OFF: usize = 0x400;
        const BLOB_OFF: usize = 0x2000;
        const CAP: usize = 8;
        const CTX: usize = 0;
        let (w, h) = (4usize, 4usize);
        let stride = w * 4;
        let fb_bytes = stride * h;

        // ---- guest RAM (memfd) mapped for our own writes; the device maps it independently ----
        let fd = unsafe { libc::memfd_create(c"pr4ram".as_ptr(), 0) };
        assert!(fd >= 0, "memfd_create");
        assert_eq!(unsafe { libc::ftruncate(fd, SIZE as libc::off_t) }, 0);
        let ram = unsafe {
            libc::mmap(ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        } as *mut u8;
        assert_ne!(ram as *mut libc::c_void, libc::MAP_FAILED, "mmap guest ram");

        // A known BGRA framebuffer in the blob region (A=255 → BGRA identity through the present).
        let mut blob = vec![0u8; fb_bytes];
        for i in 0..(w * h) {
            blob[i * 4] = i as u8 + 1; // B
            blob[i * 4 + 1] = 0x20 + i as u8; // G
            blob[i * 4 + 2] = 0x40 + i as u8; // R
            blob[i * 4 + 3] = 255; // A
        }
        unsafe { ptr::copy_nonoverlapping(blob.as_ptr(), ram.add(BLOB_OFF), fb_bytes) };

        // Lay out the four descriptor payloads; `data_offset` is relative to the descriptor array
        // base (== ring_base). Returns each payload's (data_offset, len).
        let mut pay = PAY_OFF;
        let mut put = |bytes: &[u8]| -> (u32, u32) {
            unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), ram.add(pay), bytes.len()) };
            let data_offset = (pay - DESC_OFF) as u32;
            let len = bytes.len() as u32;
            pay += (bytes.len() + 15) & !15; // 16-align the next payload
            (data_offset, len)
        };
        let create = put(
            ResourceCreateBlob { res_id: 1, ctx_id: 0, blob_mem: 1, blob_flags: 0, size: fb_bytes as u64 }
                .as_bytes(),
        );
        let mut ab = AttachBacking { res_id: 1, num_entries: 1 }.as_bytes().to_vec();
        ab.extend_from_slice(
            MemEntry { addr: GUEST_BASE + BLOB_OFF as u64, length: fb_bytes as u64 }.as_bytes(),
        );
        let attach = put(&ab);
        let scanout = put(
            SetScanoutBlob {
                scanout_id: 0,
                res_id: 1,
                width: w as u32,
                height: h as u32,
                format: format::B8G8R8A8,
                stride: stride as u32,
            }
            .as_bytes(),
        );
        let flush = put(
            ResourceFlush { res_id: 1, x: 0, y: 0, w: w as u32, h: h as u32, _reserved: 0 }.as_bytes(),
        );

        // ---- build the device; inject the DMA mapping; program the ring via BAR0 registers ----
        let mut b = InfinigpuBackend::new();
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        b.dma.map(GUEST_BASE, 0, SIZE as u64, file).unwrap();

        let blk = |field: u64| ctrl::CMD_RING_CFG + CTX as u64 * CMD_RING_STRIDE + field;
        let desc_iova = GUEST_BASE + DESC_OFF as u64;
        let idx_iova = GUEST_BASE + IDX_OFF as u64;
        b.bar0_write_u32(blk(ctrl::CMD_RING_BASE_LO), desc_iova as u32);
        b.bar0_write_u32(blk(ctrl::CMD_RING_BASE_HI), (desc_iova >> 32) as u32);
        b.bar0_write_u32(blk(ctrl::CMD_RING_SIZE), CAP as u32);
        b.bar0_write_u32(blk(ctrl::CMD_RING_INDEX_LO), idx_iova as u32);
        b.bar0_write_u32(blk(ctrl::CMD_RING_INDEX_HI), (idx_iova >> 32) as u32);
        // Register read-back confirms the decode.
        assert_eq!(b.bar0_read_u32(blk(ctrl::CMD_RING_INDEX_LO)), idx_iova as u32);
        assert_eq!(b.bar0_read_u32(blk(ctrl::CMD_RING_SIZE)), CAP as u32);

        // ---- publish the 4 descriptors through the SPSC producer over the shared page ----
        {
            let idx_ptr = unsafe { ram.add(IDX_OFF) } as *const u8;
            let desc_ptr = unsafe { ram.add(DESC_OFF) } as *const u8;
            let ring = unsafe { drain::ring_over_shared(idx_ptr, desc_ptr, CAP) }.unwrap();
            let plan = [
                (msg_type::RESOURCE_CREATE_BLOB, create),
                (msg_type::RESOURCE_ATTACH_BACKING, attach),
                (msg_type::SET_SCANOUT_BLOB, scanout),
                (msg_type::RESOURCE_FLUSH, flush),
            ];
            for (i, &(mt, (off, len))) in plan.iter().enumerate() {
                ring.push(Descriptor {
                    msg_type: mt,
                    flags: 0,
                    len,
                    data_offset: off,
                    seqno: (i + 1) as u64,
                    payload_addr: 0,
                })
                .unwrap();
            }
        }

        // ---- ring the ctx doorbell (the same trapped write QEMU issues) ----
        b.bar0_write_u32(regs::doorbell::CMD_BASE + CTX as u64 * 4, 1);

        // ---- assertions: full drain, resource state, and a correct present ----
        assert_eq!(b.ring_retired[CTX], 4, "all 4 descriptors retired (highest seqno)");
        {
            let idx_ptr = unsafe { ram.add(IDX_OFF) } as *const u8;
            let desc_ptr = unsafe { ram.add(DESC_OFF) } as *const u8;
            let ring = unsafe { drain::ring_over_shared(idx_ptr, desc_ptr, CAP) }.unwrap();
            assert!(ring.is_empty(), "head advanced to tail — ring fully drained");
            assert_eq!(ring.retired(), 4, "guest observes seqno_retired == 4");
        }
        assert!(b.resources.get(1).is_some(), "blob resource registered");
        assert_eq!(b.resources.scanout_binding_for(1).unwrap().0, 0, "scanout bound to the resource");
        let sb = b.last_scanout.as_ref().expect("RESOURCE_FLUSH produced a scanout buffer");
        assert_eq!((sb.w, sb.h), (w, h), "present geometry matches the scanout binding");
        assert_eq!(sb.bgra, blob, "presented BGRA matches the blob framebuffer");

        // Per-VM isolation: a *second* backend never sees VM-1's resource.
        let b2 = InfinigpuBackend::new();
        assert!(b2.resources.get(1).is_none(), "resources are per-VM (per-backend)");

        // Fail-closed: a flush on an unknown resource is a no-op (no panic, no present change).
        b.present_resource_flush(999, (0, 0, 1, 1), 5, CTX);

        unsafe { libc::munmap(ram as *mut libc::c_void, SIZE) };
    }

    /// 3D over the unified ring drainer: a `SUBMIT_CMD{VULKAN_VENUSLIKE}` published on the real ring
    /// must reach `submit_vulkan` (counted + fence-retired), not be dropped as "non-resource". This
    /// is the datapath a guest render node (3D Phase 1) will drive.
    #[test]
    fn vulkan_submit_flows_through_the_ring_drainer() {
        use infinigpu_abi::wire::{encoding, vk_op, Descriptor, SubmitCmd, VulkanWorkload};
        use zerocopy::IntoBytes;

        const GUEST_BASE: u64 = 0x9000_0000;
        const SIZE: usize = 0x1000;
        const IDX_OFF: usize = 0x0;
        const DESC_OFF: usize = 0x40;
        const PAY_OFF: usize = 0x200;
        const CAP: usize = 4;
        const CTX: usize = 0;

        let fd = unsafe { libc::memfd_create(c"vk3dram".as_ptr(), 0) };
        assert!(fd >= 0);
        assert_eq!(unsafe { libc::ftruncate(fd, SIZE as libc::off_t) }, 0);
        let ram = unsafe {
            libc::mmap(std::ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        } as *mut u8;
        assert_ne!(ram as *mut libc::c_void, libc::MAP_FAILED);

        // A VULKAN_VENUSLIKE submit: a SubmitCmd header followed by a VulkanWorkload body (CLEAR,
        // scanout_addr=0 → render-only, no writeback into this small test region).
        let wl = VulkanWorkload {
            op: vk_op::CLEAR,
            width: 16,
            height: 16,
            _pad: 0,
            bg: [0.0; 4],
            scanout_addr: 0,
        };
        let sc = SubmitCmd {
            ctx_id: 0,
            encoding: encoding::VULKAN_VENUSLIKE,
            payload_len: core::mem::size_of::<VulkanWorkload>() as u32,
            flags: 0,
            seqno: 1,
            in_fence: 0,
            out_fence: 1,
        };
        let sc_len = sc.as_bytes().len();
        let wl_len = wl.as_bytes().len();
        unsafe {
            std::ptr::copy_nonoverlapping(sc.as_bytes().as_ptr(), ram.add(PAY_OFF), sc_len);
            std::ptr::copy_nonoverlapping(wl.as_bytes().as_ptr(), ram.add(PAY_OFF + sc_len), wl_len);
        };

        let mut b = InfinigpuBackend::new();
        let file = unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(fd) };
        b.dma.map(GUEST_BASE, 0, SIZE as u64, file).unwrap();
        b.ring_base[CTX] = GUEST_BASE + DESC_OFF as u64;
        b.ring_cap[CTX] = CAP as u32;
        b.ring_index[CTX] = GUEST_BASE + IDX_OFF as u64;

        {
            let ring = unsafe {
                drain::ring_over_shared(ram.add(IDX_OFF), ram.add(DESC_OFF), CAP)
            }
            .unwrap();
            ring.push(Descriptor {
                msg_type: infinigpu_abi::wire::msg_type::SUBMIT_CMD,
                flags: 0,
                len: (sc_len + wl_len) as u32,
                data_offset: (PAY_OFF - DESC_OFF) as u32,
                seqno: 1,
                payload_addr: 0,
            })
            .unwrap();
        }

        b.process_ring(CTX);

        assert_eq!(b.vulkan_submits, 1, "3D submit reached submit_vulkan via the ring drainer");
        assert_eq!(b.ring_retired[CTX], 1, "the 3D fence retired");

        unsafe { libc::munmap(ram as *mut libc::c_void, SIZE) };
    }

    /// Out-of-line payload (`desc_flags::PAYLOAD_ABS`): a SUBMIT_CMD whose body lives at an absolute
    /// guest address — the transport a forwarded draw uses to carry SPIR-V too large for the ring's
    /// per-slot payload region — must be DMA-read from `payload_addr`, **not** `ring_base +
    /// data_offset`. The descriptor here carries a deliberately-bogus `data_offset`: if the host
    /// wrongly honored it, it would decode garbage and drop the submit (count 0). Reaching
    /// `submit_vulkan` (count 1) proves the absolute-address path. Off-hardware (CLEAR, no writeback).
    #[test]
    fn out_of_line_payload_is_read_from_absolute_address() {
        use infinigpu_abi::wire::{desc_flags, encoding, vk_op, Descriptor, SubmitCmd, VulkanWorkload};
        use zerocopy::IntoBytes;

        const GUEST_BASE: u64 = 0x9000_0000;
        const SIZE: usize = 0x2000;
        const IDX_OFF: usize = 0x0;
        const DESC_OFF: usize = 0x40;
        // The payload sits well past the ring's own region — an address the ring-relative math
        // (ring_base + data_offset, with data_offset u32) would only reach with a bogus offset.
        const PAY_OFF: usize = 0x1000;
        const CAP: usize = 4;
        const CTX: usize = 0;

        let fd = unsafe { libc::memfd_create(c"vkabs".as_ptr(), 0) };
        assert!(fd >= 0);
        assert_eq!(unsafe { libc::ftruncate(fd, SIZE as libc::off_t) }, 0);
        let ram = unsafe {
            libc::mmap(std::ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        } as *mut u8;
        assert_ne!(ram as *mut libc::c_void, libc::MAP_FAILED);

        let wl = VulkanWorkload {
            op: vk_op::CLEAR,
            width: 16,
            height: 16,
            _pad: 0,
            bg: [0.0; 4],
            scanout_addr: 0,
        };
        let sc = SubmitCmd {
            ctx_id: 0,
            encoding: encoding::VULKAN_VENUSLIKE,
            payload_len: core::mem::size_of::<VulkanWorkload>() as u32,
            flags: 0,
            seqno: 1,
            in_fence: 0,
            out_fence: 1,
        };
        let sc_len = sc.as_bytes().len();
        let wl_len = wl.as_bytes().len();
        unsafe {
            std::ptr::copy_nonoverlapping(sc.as_bytes().as_ptr(), ram.add(PAY_OFF), sc_len);
            std::ptr::copy_nonoverlapping(wl.as_bytes().as_ptr(), ram.add(PAY_OFF + sc_len), wl_len);
        };

        let mut b = InfinigpuBackend::new();
        let file = unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(fd) };
        b.dma.map(GUEST_BASE, 0, SIZE as u64, file).unwrap();
        b.ring_base[CTX] = GUEST_BASE + DESC_OFF as u64;
        b.ring_cap[CTX] = CAP as u32;
        b.ring_index[CTX] = GUEST_BASE + IDX_OFF as u64;

        {
            let ring = unsafe {
                drain::ring_over_shared(ram.add(IDX_OFF), ram.add(DESC_OFF), CAP)
            }
            .unwrap();
            ring.push(Descriptor {
                msg_type: infinigpu_abi::wire::msg_type::SUBMIT_CMD,
                flags: desc_flags::PAYLOAD_ABS,
                len: (sc_len + wl_len) as u32,
                data_offset: 0xDEAD_BEEF, // bogus on purpose: PAYLOAD_ABS must make the host ignore it
                seqno: 1,
                payload_addr: GUEST_BASE + PAY_OFF as u64,
            })
            .unwrap();
        }

        b.process_ring(CTX);

        assert_eq!(b.vulkan_submits, 1, "out-of-line submit read from payload_addr, not data_offset");
        assert_eq!(b.ring_retired[CTX], 1, "the out-of-line submit's fence retired");

        unsafe { libc::munmap(ram as *mut libc::c_void, SIZE) };
    }

    /// 3D own-remoting, end-to-end on real silicon: a `VULKAN_VENUSLIKE` submit naming a
    /// `vk_op::TRIANGLE` workload must **execute a real graphics pipeline on the physical GPU** and
    /// DMA-write the rendered `R8G8B8A8` pixels back to the guest scanout — the Phase-0 Step 4/5
    /// definition-of-done for a minimal Vulkan subset, with our own decoder (no Mesa venus). GPU-
    /// gated (#[ignore]); run on real silicon: `cargo test -p infinigpu-device --lib -- --ignored
    /// --test-threads=1`.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn vulkan_workload_renders_a_triangle_to_scanout() {
        use infinigpu_abi::wire::{encoding, vk_op, SubmitCmd, VulkanWorkload};
        use std::os::unix::io::FromRawFd;
        use zerocopy::IntoBytes;

        const GUEST_BASE: u64 = 0xA000_0000;
        const SCAN_OFF: usize = 0x1000;
        let (w, h): (u32, u32) = (64, 64);
        let fb_bytes = (w * h * 4) as usize;
        let size = SCAN_OFF + fb_bytes + 0x1000;

        let fd = unsafe { libc::memfd_create(c"vk3dtri".as_ptr(), 0) };
        assert!(fd >= 0);
        assert_eq!(unsafe { libc::ftruncate(fd, size as libc::off_t) }, 0);
        let ram = unsafe {
            libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        } as *mut u8;
        assert_ne!(ram as *mut libc::c_void, libc::MAP_FAILED);

        let scanout_addr = GUEST_BASE + SCAN_OFF as u64;
        let wl = VulkanWorkload {
            op: vk_op::TRIANGLE,
            width: w,
            height: h,
            _pad: 0,
            bg: [0.02, 0.02, 0.05, 1.0],
            scanout_addr,
        };
        let payload = wl.as_bytes().to_vec();
        let sc = SubmitCmd {
            ctx_id: 0,
            encoding: encoding::VULKAN_VENUSLIKE,
            payload_len: payload.len() as u32,
            flags: 0,
            seqno: 5,
            in_fence: 0,
            out_fence: 5,
        };

        let mut b = InfinigpuBackend::new();
        let file = unsafe { <std::fs::File as FromRawFd>::from_raw_fd(fd) };
        b.dma.map(GUEST_BASE, 0, size as u64, file).unwrap();

        if b.shared_gpu.device_name().is_none() {
            eprintln!("skipping: no GPU");
            unsafe { libc::munmap(ram as *mut libc::c_void, size) };
            return;
        }

        b.submit_vulkan(&sc, &payload, 0);
        assert_eq!(b.ring_retired[0], 5, "the 3D fence retired");
        assert_eq!(b.vulkan_submits, 1);

        // Read the scanout back out of guest RAM: a shader-executed triangle over the dark bg leaves
        // clearly-lit pixels (the render is proven by infinigpu-replay; here we prove it reached the
        // guest framebuffer over the device's DMA path).
        let px = unsafe { std::slice::from_raw_parts(ram.add(SCAN_OFF), fb_bytes) };
        let lit = px
            .chunks_exact(4)
            .filter(|p| p[0] as u16 + p[1] as u16 + p[2] as u16 > 96)
            .count();
        assert!(lit > 0, "the GPU-rendered triangle wrote lit pixels to the guest scanout ({lit})");

        unsafe { libc::munmap(ram as *mut libc::c_void, size) };
    }

    // --- Phase 1b: forwarded-draw wire (vk_op::FORWARDED) ---

    fn spirv_bytes(words: &[u32]) -> Vec<u8> {
        let mut v = Vec::with_capacity(words.len() * 4);
        for &w in words {
            v.extend_from_slice(&w.to_ne_bytes());
        }
        v
    }

    /// Serialize a [`ForwardedDraw`] into a `vk_op::FORWARDED` SUBMIT_CMD payload exactly as the
    /// guest ICD's `driver_submit` will: fixed VulkanWorkload, then the tail, then the SPIR-V blobs
    /// and NUL-terminated entry names. Kept in the test for now; promotes to a shared encoder in 1c.
    fn encode_forwarded(
        width: u32,
        height: u32,
        bg: [f32; 4],
        scanout_addr: u64,
        draw: &ForwardedDraw,
    ) -> Vec<u8> {
        use infinigpu_abi::wire::{vk_op, ForwardedDrawTail, VulkanWorkload};
        use zerocopy::IntoBytes;

        let vspv = spirv_bytes(draw.vertex_spirv);
        let fspv = spirv_bytes(draw.fragment_spirv);
        let ventry = draw.vertex_entry.to_bytes_with_nul();
        let fentry = draw.fragment_entry.to_bytes_with_nul();

        let wl = VulkanWorkload {
            op: vk_op::FORWARDED,
            width,
            height,
            _pad: 0,
            bg,
            scanout_addr,
        };
        let tail = ForwardedDrawTail {
            vertex_count: draw.vertex_count,
            topology: draw.topology,
            vertex_spirv_len: vspv.len() as u32,
            fragment_spirv_len: fspv.len() as u32,
            vertex_entry_len: ventry.len() as u32,
            fragment_entry_len: fentry.len() as u32,
        };

        let mut p = Vec::new();
        p.extend_from_slice(wl.as_bytes());
        p.extend_from_slice(tail.as_bytes());
        p.extend_from_slice(&vspv);
        p.extend_from_slice(&fspv);
        p.extend_from_slice(ventry);
        p.extend_from_slice(fentry);
        p
    }

    /// The wire decoder round-trips a real forwarded draw and rejects hostile inputs fail-closed —
    /// no GPU needed. Proves the byte layout + bounds checks the host relies on before it hands
    /// guest-controlled SPIR-V to the driver. Uses DISTINCT vertex/fragment blobs + entries + a
    /// non-default topology so a slot swap, transposition, or field mis-placement can't hide.
    #[test]
    fn forwarded_draw_decodes_and_rejects_hostile() {
        const CAP: usize = 64 * 1024 * 1024;
        const MAGIC: u32 = 0x0723_0203; // SPIR-V magic; decode_forwarded requires it as word 0.
        // Distinct blobs (different content AND different length) so vertex/fragment can't be
        // confused; distinct entry names; topology=1 (strip) and vertex_count=5, both non-default.
        let vspirv: [u32; 3] = [MAGIC, 0x1111_1111, 0x2222_2222];
        let fspirv: [u32; 5] = [MAGIC, 0x3333_3333, 0x4444_4444, 0x5555_5555, 0x6666_6666];
        let draw = ForwardedDraw {
            vertex_spirv: &vspirv,
            vertex_entry: c"vmain",
            fragment_spirv: &fspirv,
            fragment_entry: c"fragmain",
            vertex_count: 5,
            topology: 1,
        };
        let payload = encode_forwarded(32, 16, [0.1, 0.2, 0.3, 1.0], 0, &draw);

        // Round-trip: EVERY field decodes to the source (distinct blobs catch a vs/fs swap;
        // asserting topology catches a mis-placed tail field the old test missed).
        let owned = decode_forwarded(&payload, CAP).expect("valid forwarded payload decodes");
        assert_eq!(owned.vertex_spirv, vspirv, "vertex SPIR-V round-trips (not the fragment blob)");
        assert_eq!(owned.fragment_spirv, fspirv, "fragment SPIR-V round-trips (not the vertex blob)");
        assert_eq!(owned.vertex_entry.as_c_str(), c"vmain");
        assert_eq!(owned.fragment_entry.as_c_str(), c"fragmain");
        assert_eq!(owned.vertex_count, 5);
        assert_eq!(owned.topology, 1, "topology round-trips");

        // Byte offsets within the payload (VulkanWorkload 40 + ForwardedDrawTail 24, then blobs).
        let (hdr, tail) = (40usize, 24usize);
        let (vlen, flen) = (vspirv.len() * 4, fspirv.len() * 4);
        let velen = c"vmain".to_bytes_with_nul().len(); // 6
        let ventry_off = hdr + tail + vlen + flen;

        // Hostile inputs → None (dropped fail-closed, caller still retires the fence):
        // (a) truncated before the tail even fits.
        assert!(decode_forwarded(&payload[..hdr + 4], CAP).is_none(), "truncated tail rejected");
        // (b) truncated so the declared blobs no longer fit (aggregate length check).
        assert!(decode_forwarded(&payload[..payload.len() - 4], CAP).is_none(), "short payload rejected");
        // (c) vertex_spirv_len (tail's 3rd u32 → offset 40+8) made non-word-aligned.
        let mut bad_len = payload.clone();
        bad_len[hdr + 8] = bad_len[hdr + 8].wrapping_add(1);
        assert!(decode_forwarded(&bad_len, CAP).is_none(), "non-word-aligned SPIR-V rejected");
        // (d) the VERTEX entry's NUL specifically removed (its own bounds check, independent of the
        // fragment entry's) — offset = start of vertex entry + (name len) = its terminating NUL.
        let mut no_vnul = payload.clone();
        no_vnul[ventry_off + velen - 1] = b'x';
        assert!(decode_forwarded(&no_vnul, CAP).is_none(), "vertex entry without NUL rejected");
        // (e) a blob larger than the cap.
        assert!(decode_forwarded(&payload, 8).is_none(), "SPIR-V over the byte cap rejected");
        // (f) a blob whose first word is not the SPIR-V magic (word 0 of the vertex blob).
        let mut bad_magic = payload.clone();
        bad_magic[hdr + tail] = bad_magic[hdr + tail].wrapping_add(1);
        assert!(decode_forwarded(&bad_magic, CAP).is_none(), "non-SPIR-V (bad magic) rejected");
    }

    /// Phase-1b end-to-end on real silicon: a `vk_op::FORWARDED` submit carrying serialized SPIR-V
    /// must be decoded, compiled by the real driver, executed on the physical GPU, and DMA-written
    /// to the guest scanout — the same proof as the named-triangle E2E, but the shader arrives over
    /// the wire (the shape the guest ICD produces). GPU-gated; run with `--ignored --test-threads=1`.
    #[test]
    #[ignore = "needs a Vulkan GPU"]
    fn forwarded_draw_renders_to_scanout() {
        use std::os::unix::io::FromRawFd;

        const GUEST_BASE: u64 = 0xB000_0000;
        const SCAN_OFF: usize = 0x1000;
        let (w, h): (u32, u32) = (64, 64);
        let fb_bytes = (w * h * 4) as usize;
        let size = SCAN_OFF + fb_bytes + 0x1000;

        let fd = unsafe { libc::memfd_create(c"vkfwd".as_ptr(), 0) };
        assert!(fd >= 0);
        assert_eq!(unsafe { libc::ftruncate(fd, size as libc::off_t) }, 0);
        let ram = unsafe {
            libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        } as *mut u8;
        assert_ne!(ram as *mut libc::c_void, libc::MAP_FAILED);

        let scanout_addr = GUEST_BASE + SCAN_OFF as u64;
        let draw = ForwardedDraw::builtin_triangle();
        let payload = encode_forwarded(w, h, [0.02, 0.02, 0.05, 1.0], scanout_addr, &draw);
        let sc = infinigpu_abi::wire::SubmitCmd {
            ctx_id: 0,
            encoding: infinigpu_abi::wire::encoding::VULKAN_VENUSLIKE,
            payload_len: payload.len() as u32,
            flags: 0,
            seqno: 5,
            in_fence: 0,
            out_fence: 5,
        };

        let mut b = InfinigpuBackend::new();
        let file = unsafe { <std::fs::File as FromRawFd>::from_raw_fd(fd) };
        b.dma.map(GUEST_BASE, 0, size as u64, file).unwrap();

        if b.shared_gpu.device_name().is_none() {
            eprintln!("skipping: no GPU");
            unsafe { libc::munmap(ram as *mut libc::c_void, size) };
            return;
        }

        b.submit_vulkan(&sc, &payload, 0);
        assert_eq!(b.ring_retired[0], 5, "the forwarded 3D fence retired");
        assert_eq!(b.vulkan_submits, 1);

        let px = unsafe { std::slice::from_raw_parts(ram.add(SCAN_OFF), fb_bytes) };
        let lit = px
            .chunks_exact(4)
            .filter(|p| p[0] as u16 + p[1] as u16 + p[2] as u16 > 96)
            .count();
        assert!(lit > 0, "the forwarded-SPIR-V triangle wrote lit pixels to the guest scanout ({lit})");
        eprintln!("forwarded_draw_renders_to_scanout: lit={lit}/{}", (w * h) as usize);

        unsafe { libc::munmap(ram as *mut libc::c_void, size) };
    }
}
