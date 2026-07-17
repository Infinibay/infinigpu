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
pub mod dma;

use dma::DmaTable;
use infinigpu_abi::regs;
use infinigpu_replay::{Frame, HostGpu};
use infinigpu_sched::{BrokerConfig, GpuBroker, VmConfig, VmTicket};
use log::info;
use std::fs::File;
use std::io::{self, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
}

impl Default for SharedGpu {
    fn default() -> Self {
        SharedGpu {
            gpu: Mutex::new(None),
        }
    }
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
}

impl Default for InfinigpuBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl InfinigpuBackend {
    /// Standalone single-VM backend: its own default broker + GPU context. Used by the
    /// single-socket `infinigpu-device` binary and the in-process demos/tests.
    pub fn new() -> Self {
        let broker = GpuBroker::with_real_clock(BrokerConfig::default());
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
            broker,
            shared_gpu,
            vm_config,
            ticket: None,
            admission_denied: false,
            present_count: 0,
            present_dir,
            pixel,
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
        match self.broker.admit(self.vm_config.clone(), VRAM_ESTIMATE_MB) {
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
                CMD_RING_RETIRED_LO => (self.ring_retired[ctx] & 0xFFFF_FFFF) as u32,
                CMD_RING_RETIRED_HI => (self.ring_retired[ctx] >> 32) as u32,
                _ => 0,
            };
        }
        match off {
            DEV_MAGIC => infinigpu_abi::DEV_MAGIC,
            ABI_VERSION => infinigpu_abi::abi_version(),
            DEV_CAPS => regs::PHASE0_DEV_CAPS,
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
            encoding, msg_type, ClearPresent, Descriptor, ScanoutPresent, SubmitCmd,
        };
        use zerocopy::FromBytes;

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
            other => log::warn!("unsupported encoding {:#x}", other),
        }
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
        let mut buf = vec![0u8; total];
        if !self.dma.read(sp.scanout_addr, &mut buf) {
            log::error!(
                "present: framebuffer {:#x} ({total} bytes) not mapped",
                sp.scanout_addr
            );
            return;
        }

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

        // infiniPixel: encode this framebuffer on NVENC and stream it to any browsers.
        if streaming {
            self.stream_frame(&bgra, w as u32, h as u32);
            if self.present_count.is_multiple_of(60) {
                if let Some(p) = &self.pixel {
                    let (sent, skipped) = p.stats();
                    info!("infiniPixel: {sent} frames encoded, {skipped} idle-skipped (unchanged)");
                }
            }
        }

        // Publish completion (guest polls this ring's retired register; the MSI-X is
        // raised by the doorbell handler).
        self.ring_retired[ctx] = seqno;
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
    let mut backend = InfinigpuBackend::new();
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
}
