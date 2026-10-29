//! Zero-copy screen capture via Windows.Graphics.Capture.
//!
//! Frames arrive as D3D11 textures owned by the capture frame pool. They never
//! touch system memory; consumers are expected to convert/encode on the GPU.

use anyhow::{Context, Result};
use windows::core::{factory, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// Shared D3D11 device used by capture, conversion and encoding so textures can
/// be handed between stages without cross-device copies.
#[derive(Clone)]
pub struct GpuDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    winrt: IDirect3DDevice,
}

impl GpuDevice {
    pub fn new() -> Result<Self> {
        unsafe {
            // Ignore "already initialized" style errors; apartment just needs to exist.
            let _ = RoInitialize(RO_INIT_MULTITHREADED);

            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let device: ID3D11Device = device.context("no D3D11 device")?;
            let context = context.context("no D3D11 context")?;

            let dxgi: IDXGIDevice = device.cast()?;
            let winrt: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
            Ok(Self { device, context, winrt })
        }
    }
}

/// A captured frame. The texture is only valid until the callback returns.
pub struct Frame<'a> {
    pub texture: &'a ID3D11Texture2D,
    /// QPC-based timestamp in 100ns units (same clock as `QueryPerformanceCounter`).
    pub timestamp_100ns: i64,
    pub width: u32,
    pub height: u32,
}

pub struct MonitorCapture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
}

impl MonitorCapture {
    pub fn primary(gpu: &GpuDevice, mut on_frame: impl FnMut(Frame) + Send + 'static) -> Result<Self> {
        let monitor = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
        Self::monitor(gpu, monitor, move |f| on_frame(f))
    }

    pub fn monitor(gpu: &GpuDevice, monitor: HMONITOR, mut on_frame: impl FnMut(Frame) + Send + 'static) -> Result<Self> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor) }.context("CreateForMonitor")?;
        let size = item.Size()?;

        // Free-threaded pool: frames are delivered on a WGC worker thread, no
        // dispatcher queue / message pump needed.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &gpu.winrt,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // Yellow border / cursor toggles are Win11+; ignore failures on older builds.
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(true);
        // WGC throttles to ~60 fps unless asked otherwise (Win11 24H2+).
        let _ = session.SetMinUpdateInterval(windows::Foundation::TimeSpan { Duration: 10_000 });

        pool.FrameArrived(&TypedEventHandler::new(
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
        Ok(Self { session, pool })
    }
}

impl Drop for MonitorCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
