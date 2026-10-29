use anyhow::{Context, Result};
use windows::core::{factory, h, Interface};
use windows::Foundation::Metadata::ApiInformation;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::{Frame, GpuDevice, Monitor};

pub fn supports_uncapped() -> bool {
    ApiInformation::IsPropertyPresent(
        h!("Windows.Graphics.Capture.GraphicsCaptureSession"),
        h!("MinUpdateInterval"),
    )
    .unwrap_or(false)
}

pub struct WgcCapture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    token: i64,
}

impl WgcCapture {
    pub fn start(gpu: &GpuDevice, monitor: &Monitor, mut on_frame: impl FnMut(Frame) + Send + 'static) -> Result<Self> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor.handle) }.context("CreateForMonitor")?;

        // Free-threaded pool: frames are delivered on a WGC worker thread, no
        // dispatcher queue / message pump needed.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &gpu.winrt,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            item.Size()?,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(true);
        // Without this WGC throttles to ~60 fps.
        let _ = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000 });

        let token = pool.FrameArrived(&TypedEventHandler::new(
            move |pool: windows_core::Ref<Direct3D11CaptureFramePool>, _| {
                let Some(pool) = pool.as_ref() else { return Ok(()) };
                let frame = pool.TryGetNextFrame()?;
                let size = frame.ContentSize()?;
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
                on_frame(Frame {
                    texture: &texture,
                    timestamp_100ns: frame.SystemRelativeTime()?.Duration,
                    width: size.Width as u32,
                    height: size.Height as u32,
                });
                Ok(())
            },
        ))?;

        session.StartCapture()?;
        Ok(Self { session, pool, token })
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        // Unregister first so the handler (and everything it owns) is released.
        let _ = self.pool.RemoveFrameArrived(self.token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
