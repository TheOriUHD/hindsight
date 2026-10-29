use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::Monitor;

/// Shared D3D11 device used by capture, conversion and encoding so textures can
/// be handed between stages without cross-device copies. Always created on the
/// adapter that drives the captured monitor.
#[derive(Clone)]
pub struct GpuDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter_name: String,
    pub vendor_id: u32,
    pub(crate) winrt: IDirect3DDevice,
}

unsafe impl Send for GpuDevice {}
unsafe impl Sync for GpuDevice {}

impl GpuDevice {
    pub fn for_monitor(monitor: &Monitor) -> Result<Self> {
        unsafe {
            crate::ensure_mta();
            // Apartment just needs to exist; "already initialized" is fine.
            let _ = RoInitialize(RO_INIT_MULTITHREADED);

            let adapter: IDXGIAdapter = monitor.adapter.cast()?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
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
            let context: ID3D11DeviceContext = context.context("no D3D11 context")?;

            // Capture callbacks and the encoder touch the immediate context from
            // different threads.
            let _ = context.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);

            let dxgi: IDXGIDevice = device.cast()?;
            let winrt: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
            Ok(Self { device, context, adapter_name: monitor.adapter_name.clone(), vendor_id: monitor.vendor_id, winrt })
        }
    }
}
