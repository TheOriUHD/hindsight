use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use windows::core::Interface;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutputDuplication, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::cursor::CursorCompositor;
use crate::{Frame, GpuDevice, Monitor};

pub struct DxgiCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct SendPtr<T>(T);
unsafe impl<T> Send for SendPtr<T> {}

/// Where the pointer is, as last reported by the duplication.
#[derive(Default)]
struct Pointer {
    visible: bool,
    pos: POINT,
    shape_buf: Vec<u8>,
}

impl DxgiCapture {
    /// `fps` is the rate frames are wanted at.
    pub fn start(gpu: &GpuDevice, monitor: &Monitor, fps: u32, mut on_frame: impl FnMut(Frame) + Send + 'static) -> Result<Self> {
        let output: IDXGIOutput1 = monitor.output.cast()?;
        let device = gpu.device.clone();
        let mut dup = Some(unsafe { output.DuplicateOutput(&device)? });
        // Clips still work without a drawn cursor if the shader can't be built.
        let mut cursor = CursorCompositor::new(&gpu.device, &gpu.context).map_err(|e| log::warn!("cursor drawing unavailable: {e:#}")).ok();

        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SendPtr((output, device));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new().name("dxgi-capture".into()).spawn(move || {
            let ctx = ctx;
            let SendPtr((output, device)) = ctx;
            let mut freq = 0i64;
            unsafe { QueryPerformanceFrequency(&mut freq).ok() };
            let mut ptr = Pointer::default();
            // Never wait inside AcquireNextFrame: it holds the device lock while
            // it waits, which starves the encoder sharing this device. Sleep
            // until a frame is due, then poll.
            let interval = Duration::from_secs_f64(0.9 / fps.max(1) as f64);
            let mut next = Instant::now();
            while !stop2.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now < next {
                    std::thread::sleep(next - now);
                }
                let d = match dup.as_ref() {
                    Some(d) => d,
                    None => match reopen(&output, &device) {
                        Some(d) => {
                            // The shape is only resent when it changes, and a new
                            // duplication starts over.
                            ptr = Pointer::default();
                            dup.insert(d)
                        }
                        None => {
                            // Secure desktop / mode switch in progress; retry shortly.
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                    },
                };
                match acquire(d, freq, &mut ptr, cursor.as_mut(), &mut on_frame) {
                    Ok(()) => next = Instant::now() + interval,
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => next = Instant::now() + Duration::from_millis(1),
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

fn acquire(
    dup: &IDXGIOutputDuplication,
    qpc_freq: i64,
    ptr: &mut Pointer,
    mut cursor: Option<&mut CursorCompositor>,
    on_frame: &mut impl FnMut(Frame),
) -> windows::core::Result<()> {
    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource = None;
    unsafe { dup.AcquireNextFrame(0, &mut info, &mut resource)? };

    let moved = info.LastMouseUpdateTime != 0;
    if moved {
        ptr.visible = info.PointerPosition.Visible.as_bool();
        ptr.pos = info.PointerPosition.Position;
    }
    if let (Some(c), true) = (cursor.as_deref_mut(), info.PointerShapeBufferSize > 0) {
        ptr.shape_buf.resize(info.PointerShapeBufferSize as usize, 0);
        let mut needed = 0;
        let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let got = unsafe { dup.GetFramePointerShape(info.PointerShapeBufferSize, ptr.shape_buf.as_mut_ptr() as _, &mut needed, &mut shape) };
        if got.is_ok() {
            if let Err(e) = c.set_shape(&shape, &ptr.shape_buf[..needed as usize]) {
                log::warn!("cursor shape: {e:#}");
            }
        }
    }

    let drawn = ptr.visible && cursor.as_deref().is_some_and(CursorCompositor::has_shape);
    // LastPresentTime == 0 means only the cursor moved. That's a new frame only
    // if we draw the cursor ourselves.
    let stamp = if info.LastPresentTime != 0 {
        Some(info.LastPresentTime)
    } else if moved && drawn {
        Some(info.LastMouseUpdateTime)
    } else {
        None
    };
    if let (Some(stamp), Some(resource)) = (stamp, resource) {
        let texture: ID3D11Texture2D = resource.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let composed = if drawn {
            cursor
                .as_deref_mut()
                .and_then(|c| c.compose(&texture, desc.Width, desc.Height, ptr.pos.x, ptr.pos.y).map_err(|e| log::warn!("cursor draw: {e:#}")).ok())
        } else {
            None
        };
        on_frame(Frame {
            texture: composed.as_ref().unwrap_or(&texture),
            timestamp_100ns: to_100ns(stamp, qpc_freq),
            width: desc.Width,
            height: desc.Height,
        });
    }
    unsafe { dup.ReleaseFrame() }
}

fn to_100ns(qpc: i64, freq: i64) -> i64 {
    // Some drivers report a stale present time; never hand out zero.
    let qpc = if qpc > 0 {
        qpc
    } else {
        let mut now = 0;
        unsafe { QueryPerformanceCounter(&mut now).ok() };
        now
    };
    (qpc as i128 * 10_000_000 / freq as i128) as i64
}

impl Drop for DxgiCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
