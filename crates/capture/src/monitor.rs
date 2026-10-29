use anyhow::Result;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

/// A desktop monitor together with the GPU adapter that scans it out.
#[derive(Clone)]
pub struct Monitor {
    pub name: String,
    pub adapter_name: String,
    /// PCI vendor id of the adapter (0x10DE NVIDIA, 0x1002 AMD, 0x8086 Intel).
    pub vendor_id: u32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub handle: HMONITOR,
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) output: IDXGIOutput,
}

// DXGI objects are free-threaded.
unsafe impl Send for Monitor {}
unsafe impl Sync for Monitor {}

/// All monitors attached to the desktop, primary first.
pub fn monitors() -> Result<Vec<Monitor>> {
    crate::ensure_mta();
    let mut out = Vec::new();
    unsafe {
        // Without this, DXGI reports DPI-virtualized sizes (4K at 150% -> 2560x1440).
        // Fails harmlessly if the awareness was already set (e.g. via manifest).
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let adesc = adapter.GetDesc1()?;
            let adapter_name = wide(&adesc.Description);
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                o += 1;
                let desc = output.GetDesc()?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let r = desc.DesktopCoordinates;
                out.push(Monitor {
                    name: wide(&desc.DeviceName),
                    adapter_name: adapter_name.clone(),
                    vendor_id: adesc.VendorId,
                    width: (r.right - r.left) as u32,
                    height: (r.bottom - r.top) as u32,
                    primary: r.left == 0 && r.top == 0,
                    handle: desc.Monitor,
                    adapter: adapter.clone(),
                    output,
                });
            }
        }
    }
    out.sort_by_key(|m| !m.primary);
    Ok(out)
}

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}
