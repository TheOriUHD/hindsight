//! Zero-copy screen capture.
//!
//! Frames arrive as D3D11 textures on the GPU that drives the captured monitor.
//! They never touch system memory; consumers convert/encode on the GPU.
//!
//! Backends:
//! - Windows.Graphics.Capture (preferred) when it can run uncapped (Win11 24H2+).
//! - DXGI Desktop Duplication otherwise: full refresh rate on any Win8+ system,
//!   no yellow capture border, invisible to the user.

mod device;
mod dxgi;
mod monitor;
mod wgc;

use anyhow::Result;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

pub use device::GpuDevice;
pub use monitor::{monitors, Monitor};

/// A captured frame. The texture is only valid until the callback returns.
pub struct Frame<'a> {
    pub texture: &'a ID3D11Texture2D,
    /// QPC-based timestamp in 100ns units.
    pub timestamp_100ns: i64,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Wgc,
    Dxgi,
}

impl Backend {
    /// WGC throttles to ~60 fps unless `MinUpdateInterval` exists, so only pick it
    /// where it can run at the display's full refresh rate.
    pub fn auto() -> Self {
        if wgc::supports_uncapped() { Backend::Wgc } else { Backend::Dxgi }
    }
}

/// Running capture; stops when dropped.
pub enum Capture {
    Wgc(wgc::WgcCapture),
    Dxgi(dxgi::DxgiCapture),
}

impl Capture {
    pub fn start(
        gpu: &GpuDevice,
        monitor: &Monitor,
        backend: Backend,
        on_frame: impl FnMut(Frame) + Send + 'static,
    ) -> Result<Self> {
        Ok(match backend {
            Backend::Wgc => Capture::Wgc(wgc::WgcCapture::start(gpu, monitor, on_frame)?),
            Backend::Dxgi => Capture::Dxgi(dxgi::DxgiCapture::start(gpu, monitor, on_frame)?),
        })
    }
}
