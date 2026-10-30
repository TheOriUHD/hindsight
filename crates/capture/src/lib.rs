//! Zero-copy screen capture.
//!
//! Frames arrive as D3D11 textures on the GPU that drives the captured monitor.
//! They never touch system memory; consumers convert/encode on the GPU.
//!
//! Backends:
//! - DXGI Desktop Duplication (preferred): full refresh rate on any Win8+ system,
//!   invisible to the user. We draw the cursor into the frames ourselves.
//! - Windows.Graphics.Capture as the fallback. It forces a software cursor, and
//!   OpenGL/Vulkan exclusive-fullscreen games then lose their real cursor while
//!   it runs: https://github.com/microsoft/Windows.UI.Composition-Win32-Samples/issues/128

mod cursor;
mod device;
mod dxgi;
mod monitor;
mod wgc;

use anyhow::Result;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

pub use device::GpuDevice;
pub use monitor::{monitors, Monitor};

/// Keep the process-wide multithreaded COM apartment alive for the life of
/// the process. Without this, the MTA is torn down when the last thread that
/// joined it exits, and cached WinRT factories (capture, ApiInformation)
/// become dangling pointers.
pub fn ensure_mta() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let _ = windows::Win32::System::Com::CoIncrementMTAUsage();
    });
}

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
    pub fn other(self) -> Self {
        match self {
            Backend::Wgc => Backend::Dxgi,
            Backend::Dxgi => Backend::Wgc,
        }
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
        fps: u32,
        on_frame: impl FnMut(Frame) + Send + 'static,
    ) -> Result<Self> {
        Ok(match backend {
            Backend::Wgc => Capture::Wgc(wgc::WgcCapture::start(gpu, monitor, on_frame)?),
            Backend::Dxgi => Capture::Dxgi(dxgi::DxgiCapture::start(gpu, monitor, fps, on_frame)?),
        })
    }
}
