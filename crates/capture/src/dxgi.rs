use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutputDuplication, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::System::Performance::QueryPerformanceFrequency;

use crate::{Frame, GpuDevice, Monitor};

// TODO: Desktop Duplication does not composite the mouse cursor; draw it from
// the pointer shape info when the cursor is visible.

pub struct DxgiCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct SendPtr<T>(T);
unsafe impl<T> Send for SendPtr<T> {}

impl DxgiCapture {
    pub fn start(gpu: &GpuDevice, monitor: &Monitor, mut on_frame: impl FnMut(Frame) + Send + 'static) -> Result<Self> {
        let output: IDXGIOutput1 = monitor.output.cast()?;
        let device = gpu.device.clone();
        let mut dup = Some(unsafe { output.DuplicateOutput(&device)? });

        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SendPtr((output, device));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new().name("dxgi-capture".into()).spawn(move || {
            let SendPtr((output, device)) = ctx;
            let mut freq = 0i64;
            unsafe { QueryPerformanceFrequency(&mut freq).ok() };
            while !stop2.load(Ordering::Relaxed) {
                let d = match dup.as_ref() {
                    Some(d) => d,
                    None => match reopen(&output, &device) {
                        Some(d) => dup.insert(d),
                        None => {
                            // Secure desktop / mode switch in progress; retry shortly.
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                    },
                };
                match acquire(d, freq, &mut on_frame) {
                    Ok(()) => {}
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                    Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => dup = None,
                    Err(_) => dup = None,
                }
            }
        })?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

fn reopen(output: &IDXGIOutput1, device: &ID3D11Device) -> Option<IDXGIOutputDuplication> {
    unsafe { output.DuplicateOutput(device).ok() }
}

fn acquire(dup: &IDXGIOutputDuplication, qpc_freq: i64, on_frame: &mut impl FnMut(Frame)) -> windows::core::Result<()> {
    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource = None;
    unsafe { dup.AcquireNextFrame(100, &mut info, &mut resource)? };
    // LastPresentTime == 0 means only the cursor moved; no new desktop image.
    if info.LastPresentTime != 0 {
        if let Some(resource) = resource {
            let texture: ID3D11Texture2D = resource.cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            on_frame(Frame {
                texture: &texture,
                timestamp_100ns: (info.LastPresentTime as i128 * 10_000_000 / qpc_freq as i128) as i64,
                width: desc.Width,
                height: desc.Height,
            });
        }
    }
    unsafe { dup.ReleaseFrame() }
}

impl Drop for DxgiCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
