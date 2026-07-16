//! # infinigpu-hal — the vendor HAL (ADR-0008)
//!
//! Capability-flag traits that keep the NVIDIA-specific pieces (Vulkan render, NVENC
//! encode, GPU-time QoS) as **backends**, not the architecture. The rest of the stack
//! (scheduler, session negotiation, infiniPixel) queries *capabilities* — never a
//! vendor by name — so adding AMD (RADV Vulkan + VA-API/Vulkan-Video) or Intel is a new
//! backend, not a rewrite. Vulkan Video is the cross-vendor codec default.
//!
//! This crate is **pure**: no `ash`, no `ffmpeg`, no OS deps. The concrete backends
//! ([`infinigpu-replay`]'s `HostGpu`, [`infinigpu-pixel`]'s `Encoder`) implement these
//! traits and report their real capabilities; consumers program against the traits.

#![forbid(unsafe_code)]

use core::fmt;

/// GPU / media vendor. Selection is by *capability*, never by this tag — it exists for
/// logging, telemetry, and vendor-specific tuning knobs (e.g. NVIDIA's GPU-time QoS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    /// A software implementation (llvmpipe render, libx264 encode).
    Software,
    Other,
}

impl fmt::Display for Vendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Vendor::Nvidia => "NVIDIA",
            Vendor::Amd => "AMD",
            Vendor::Intel => "Intel",
            Vendor::Software => "software",
            Vendor::Other => "other",
        })
    }
}

/// A video codec (for capability negotiation, ADR-0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

impl fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::Hevc => "HEVC",
            VideoCodec::Av1 => "AV1",
        })
    }
}

/// What a GPU render backend can do. Consumers gate behavior on these, not on [`Vendor`].
#[derive(Debug, Clone)]
pub struct GpuCaps {
    pub vendor: Vendor,
    pub device_name: String,
    pub driver_name: String,
    /// Can execute a Vulkan render (the ADR-0002 replay path).
    pub vulkan_render: bool,
    /// Exposes Vulkan timestamp queries — the authoritative GPU-time currency the
    /// ADR-0007 scheduler debits (vs. the wall-clock proxy used until this is wired).
    pub timestamp_queries: bool,
    /// Supports external memory (dma-buf import/export) for zero-readback hand-off of
    /// the rendered image straight to the encoder (ADR-0009).
    pub external_memory: bool,
    /// Supports a hardware submission-priority hint (`VK_EXT_global_priority`) — only a
    /// soft MEDIUM/LOW knob on NVIDIA (ADR-0007); the token bucket is the hard backstop.
    pub global_priority: bool,
}

impl GpuCaps {
    /// True if this backend can be the replay GPU for a session.
    pub fn can_render(&self) -> bool {
        self.vulkan_render
    }
}

/// A GPU render backend (the replay half — ADR-0002). `HostGpu` (Vulkan/NVIDIA now,
/// RADV/ANV later) implements this.
pub trait GpuBackend: Send + Sync {
    fn caps(&self) -> GpuCaps;
}

/// What a media encode backend can do (ADR-0009 negotiation / ADR-0007 NVENC as a
/// first-class admission resource).
#[derive(Debug, Clone)]
pub struct CodecCaps {
    pub vendor: Vendor,
    /// Hardware encode (NVENC / VA-API / Vulkan-Video) vs. software (libx264).
    pub hardware: bool,
    /// Codecs this backend can *encode*.
    pub encode: Vec<VideoCodec>,
    /// Configured for ultra-low latency (no B-frames, delay 0).
    pub low_latency: bool,
    /// Number of concurrent encode sessions this engine supports (GA102 = 1 NVENC
    /// block — a scarce, first-class admission resource per ADR-0007). `None` = unknown
    /// / software (bounded by CPU, not a fixed engine count).
    pub max_sessions: Option<u32>,
}

impl CodecCaps {
    pub fn can_encode(&self, codec: VideoCodec) -> bool {
        self.encode.contains(&codec)
    }
}

/// A media encode backend (the infiniPixel encode half — ADR-0009).
pub trait MediaEncoder: Send {
    fn caps(&self) -> CodecCaps;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeGpu;
    impl GpuBackend for FakeGpu {
        fn caps(&self) -> GpuCaps {
            GpuCaps {
                vendor: Vendor::Amd,
                device_name: "Radeon".into(),
                driver_name: "radv".into(),
                vulkan_render: true,
                timestamp_queries: true,
                external_memory: true,
                global_priority: true,
            }
        }
    }

    #[test]
    fn caps_are_queried_by_capability_not_vendor() {
        let g = FakeGpu;
        // A consumer selects on capability, so a non-NVIDIA backend Just Works.
        assert!(g.caps().can_render());
        assert_eq!(g.caps().vendor, Vendor::Amd);

        let nvenc = CodecCaps {
            vendor: Vendor::Nvidia,
            hardware: true,
            encode: vec![VideoCodec::H264, VideoCodec::Hevc],
            low_latency: true,
            max_sessions: Some(1),
        };
        assert!(nvenc.can_encode(VideoCodec::H264));
        assert!(nvenc.can_encode(VideoCodec::Hevc));
        assert!(!nvenc.can_encode(VideoCodec::Av1)); // GA102 can't AV1-encode (ADR-0009)
        assert_eq!(nvenc.max_sessions, Some(1)); // the scarce NVENC block
    }
}
