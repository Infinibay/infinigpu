//! # infinigpu-nvml — real GPU capacity + attribution via NVML
//!
//! The GPU broker (`infinigpu-sched`) is deliberately **GPU-agnostic**: it takes a
//! `total_vram_mb` and per-VM caps as *policy* and never links a vendor library. This
//! crate is the NVIDIA-specific companion that turns policy guesses into **measured**
//! numbers — real total/free VRAM for the admission ledger, live GPU/encoder utilization
//! for telemetry, and per-process GPU-memory attribution for the ADR-0003 jailed
//! per-VM replay process (each replay process is one OS pid → NVML attributes its VRAM).
//!
//! It links `libnvidia-ml` at runtime; on a host without NVIDIA/NVML, [`NvmlProbe::open`]
//! returns an error and the caller falls back to configured policy (fail-open for
//! *capacity discovery*, never for admission).

use nvml_wrapper::enums::device::UsedGpuMemory;
use nvml_wrapper::Nvml;
use std::error::Error;

type R<T> = Result<T, Box<dyn Error>>;

const MB: u64 = 1024 * 1024;

/// A point-in-time capacity/utilization reading for one physical GPU.
#[derive(Debug, Clone)]
pub struct GpuSnapshot {
    pub index: u32,
    pub name: String,
    pub total_mb: u64,
    pub used_mb: u64,
    pub free_mb: u64,
    /// SM (compute) utilization, percent.
    pub gpu_util_pct: u32,
    /// Memory-controller utilization, percent.
    pub mem_util_pct: u32,
    /// Processes with a graphics context on this GPU.
    pub graphics_processes: usize,
    /// Active NVENC encode sessions (the scarce ADR-0007 admission resource on GA102),
    /// or `None` if the driver doesn't report it.
    pub encoder_sessions: Option<u32>,
}

impl GpuSnapshot {
    /// VRAM (MB) the broker may hand out, i.e. free memory minus a host reserve. Clamped
    /// at 0. This is the honest `total_vram_mb - vram_reserve_mb` input for the ledger.
    pub fn usable_vram_mb(&self, reserve_mb: u64) -> u64 {
        self.free_mb.saturating_sub(reserve_mb)
    }
}

/// Per-process GPU-memory attribution: an OS pid and the VRAM (MB) it holds on a GPU.
#[derive(Debug, Clone, Copy)]
pub struct ProcessVram {
    pub pid: u32,
    /// Bytes attributed by NVML, or `None` when the driver can't attribute it.
    pub used_mb: Option<u64>,
}

/// A live NVML handle. Cheap to keep around; one per process.
pub struct NvmlProbe {
    nvml: Nvml,
}

impl NvmlProbe {
    /// Initialise NVML. Errors on a host without the NVIDIA driver / `libnvidia-ml`.
    pub fn open() -> R<Self> {
        Ok(NvmlProbe { nvml: Nvml::init()? })
    }

    /// Number of NVML-visible GPUs.
    pub fn device_count(&self) -> R<u32> {
        Ok(self.nvml.device_count()?)
    }

    /// A capacity/utilization snapshot for GPU `index`.
    pub fn snapshot(&self, index: u32) -> R<GpuSnapshot> {
        let dev = self.nvml.device_by_index(index)?;
        let mem = dev.memory_info()?;
        let util = dev.utilization_rates()?;
        let name = dev.name()?;
        let graphics_processes = dev.running_graphics_processes()?.len();
        // encoder_sessions may be unsupported on some driver/GPU combos — treat as optional.
        let encoder_sessions = dev.encoder_sessions().ok().map(|s| s.len() as u32);
        Ok(GpuSnapshot {
            index,
            name,
            total_mb: mem.total / MB,
            used_mb: mem.used / MB,
            free_mb: mem.free / MB,
            gpu_util_pct: util.gpu,
            mem_util_pct: util.memory,
            graphics_processes,
            encoder_sessions,
        })
    }

    /// Snapshots for every visible GPU.
    pub fn snapshot_all(&self) -> R<Vec<GpuSnapshot>> {
        let n = self.device_count()?;
        (0..n).map(|i| self.snapshot(i)).collect()
    }

    /// Per-process VRAM attribution on GPU `index` — the ADR-0003 mechanism: each jailed
    /// per-VM replay process is one pid, so NVML tells us exactly how much VRAM that VM's
    /// GPU work holds, replacing the broker's fixed per-VM estimate with a measurement.
    pub fn process_vram(&self, index: u32) -> R<Vec<ProcessVram>> {
        let dev = self.nvml.device_by_index(index)?;
        Ok(dev
            .running_graphics_processes()?
            .into_iter()
            .map(|p| ProcessVram {
                pid: p.pid,
                used_mb: match p.used_gpu_memory {
                    UsedGpuMemory::Used(bytes) => Some(bytes / MB),
                    UsedGpuMemory::Unavailable => None,
                },
            })
            .collect())
    }
}
