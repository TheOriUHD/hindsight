//! "Clip saved" toast: a tiny native window that slides in at the top-right
//! of the recorded screen for ~2 s.
//!
//! It never takes focus, ignores the mouse, and is excluded from screen
//! capture (so it never shows up in later clips). Visible over windowed and
//! borderless games; exclusive-fullscreen games hide it, the chime still plays.

use std::cell::RefCell;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::core::w;
use windows_numerics::Vector2;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Saved,
    Failed,
}

struct Request {
    kind: Kind,
    text: String,
    monitor: isize,
}

struct Shared {
    hwnd: isize,
    pending: Option<Request>,
}

static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
const WM_SHOW: u32 = WM_APP + 1;
const TIMER: usize = 1;

// Animation timeline (ms).
const IN_MS: f32 = 180.0;
const HOLD_UNTIL_MS: f32 = 2200.0;
const OUT_MS: f32 = 220.0;

/// Show the toast. `monitor` is an HMONITOR; 0 = the monitor the user is on.
pub fn show(kind: Kind, text: &str, monitor: isize) {
    let shared = SHARED.get_or_init(|| {
        std::thread::Builder::new()
            .name("overlay".into())
            .spawn(|| {
                if let Err(e) = run() {
                    log::warn!("overlay: {e:#}");
                }
            })
            .ok();
        Mutex::new(Shared { hwnd: 0, pending: None })
    });
    let mut s = shared.lock().unwrap();
    s.pending = Some(Request { kind, text: text.to_string(), monitor });
    if s.hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(s.hwnd as _)), WM_SHOW, WPARAM(0), LPARAM(0));
        }
    }
    // Otherwise the thread picks `pending` up as soon as its window exists.
}

struct Anim {
    start: Instant,
    dc: HDC,
    bmp: HBITMAP,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    scale: f32,
}

impl Drop for Anim {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

thread_local! {
    static ANIM: RefCell<Option<Anim>> = const { RefCell::new(None) };
}

fn run() -> anyhow::Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: w!("HindsightOverlay"), ..Default::default() };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            w!("HindsightOverlay"),
            w!("Hindsight"),
            WS_POPUP,
            0, 0, 1, 1,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;
        // Keep the toast out of our own (and any other) screen capture.
        // (HINDSIGHT_OVERLAY_CAPTURABLE=1 keeps it visible to screenshots for debugging.)
        if std::env::var_os("HINDSIGHT_OVERLAY_CAPTURABLE").is_none() {
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        }
        {
            let mut s = SHARED.get().unwrap().lock().unwrap();
            s.hwnd = hwnd.0 as isize;
            if s.pending.is_some() {
                let _ = PostMessageW(Some(hwnd), WM_SHOW, WPARAM(0), LPARAM(0));
            }
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SHOW => {
            let req = SHARED.get().and_then(|s| s.lock().unwrap().pending.take());
            if let Some(req) = req {
                match render(&req) {
                    Ok(anim) => {
                        ANIM.with(|a| *a.borrow_mut() = Some(anim));
                        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                        step(hwnd);
                        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        SetTimer(Some(hwnd), TIMER, 15, None);
                    }
                    Err(e) => log::warn!("overlay render: {e:#}"),
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            step(hwnd);
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Advance the slide/fade animation by one frame.
unsafe fn step(hwnd: HWND) {
    let done = ANIM.with(|a| {
        let a = a.borrow();
        let Some(a) = a.as_ref() else { return true };
        let t = a.start.elapsed().as_secs_f32() * 1000.0;
        let ease = |p: f32| 1.0 - (1.0 - p.clamp(0.0, 1.0)).powi(3);
        let (offset, alpha) = if t < IN_MS {
            let p = ease(t / IN_MS);
            ((1.0 - p) * 36.0, p)
        } else if t < HOLD_UNTIL_MS {
            (0.0, 1.0)
        } else if t < HOLD_UNTIL_MS + OUT_MS {
            let p = ease((t - HOLD_UNTIL_MS) / OUT_MS);
            (p * 24.0, 1.0 - p)
        } else {
            return true;
        };
        let pos = POINT { x: a.x + (offset * a.scale) as i32, y: a.y };
        let size = SIZE { cx: a.w, cy: a.h };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: (alpha * 255.0) as u8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(hwnd, None, Some(&pos), Some(&size), Some(a.dc), Some(&POINT::default()), COLORREF(0), Some(&blend), ULW_ALPHA);
        false
    });
    if done {
        let _ = KillTimer(Some(hwnd), TIMER);
        let _ = ShowWindow(hwnd, SW_HIDE);
        ANIM.with(|a| *a.borrow_mut() = None);
    }
}

fn color(hex: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a,
    }
}

unsafe fn render(req: &Request) -> anyhow::Result<Anim> {
    // Position on the requested monitor (or wherever the user is).
    let hmon = if req.monitor != 0 {
        HMONITOR(req.monitor as _)
    } else {
        MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY)
    };
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let _ = GetMonitorInfoW(hmon, &mut info);
    let (mut dx, mut dy) = (96u32, 96u32);
    let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
    let s = dx as f32 / 96.0;

    let (w, h) = ((300.0 * s) as i32, (64.0 * s) as i32);
    let margin = (22.0 * s) as i32;
    let x = info.rcMonitor.right - w - margin;
    let y = info.rcMonitor.top + margin;

    // 32-bit premultiplied DIB that UpdateLayeredWindow composites per-pixel.
    let dc = CreateCompatibleDC(None);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    SelectObject(dc, bmp.into());
    let anim = Anim { start: Instant::now(), dc, bmp, x, y, w, h, scale: s };

    let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
    let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    let rt = d2d.CreateDCRenderTarget(&props)?;
    rt.BindDC(dc, &RECT { left: 0, top: 0, right: w, bottom: h })?;
    rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

    let (fw, fh) = (w as f32, h as f32);
    let accent = if req.kind == Kind::Saved { 0xff4d5a } else { 0xf5b94a };
    let bg = rt.CreateSolidColorBrush(&color(0x16181e, 0.96), None)?;
    let border = rt.CreateSolidColorBrush(&color(0x2c303b, 1.0), None)?;
    let accent_b = rt.CreateSolidColorBrush(&color(accent, 1.0), None)?;
    let white = rt.CreateSolidColorBrush(&color(0xffffff, 1.0), None)?;
    let text_b = rt.CreateSolidColorBrush(&color(0xeef0f5, 1.0), None)?;
    let muted = rt.CreateSolidColorBrush(&color(0x8b91a1, 1.0), None)?;

    let title_fmt = dw.CreateTextFormat(w!("Segoe UI"), None, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, 14.5 * s, w!("en-us"))?;
    let sub_fmt = dw.CreateTextFormat(w!("Segoe UI"), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, 12.0 * s, w!("en-us"))?;
    for f in [&title_fmt, &sub_fmt] {
        f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        let sign = dw.CreateEllipsisTrimmingSign(f)?;
        let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
        f.SetTrimming(&trim, &sign)?;
    }

    rt.BeginDraw();
    rt.Clear(Some(&color(0, 0.0)));
    let card = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F { left: 0.5, top: 0.5, right: fw - 0.5, bottom: fh - 0.5 },
        radiusX: 12.0 * s,
        radiusY: 12.0 * s,
    };
    rt.FillRoundedRectangle(&card, &bg);
    rt.DrawRoundedRectangle(&card, &border, 1.0, None);

    // Badge: red circle with a check (or amber with "!").
    let (cx, cy, r) = (32.0 * s, fh / 2.0, 13.0 * s);
    rt.FillEllipse(&D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: r, radiusY: r }, &accent_b);
    if req.kind == Kind::Saved {
        let p = |x: f32, y: f32| Vector2 { X: cx + x * s, Y: cy + y * s };
        rt.DrawLine(p(-5.5, 0.5), p(-1.5, 4.5), &white, 2.4 * s, None);
        rt.DrawLine(p(-1.5, 4.5), p(6.0, -4.5), &white, 2.4 * s, None);
    } else {
        rt.DrawLine(Vector2 { X: cx, Y: cy - 6.0 * s }, Vector2 { X: cx, Y: cy + 1.5 * s }, &white, 2.6 * s, None);
        rt.FillEllipse(&D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy + 6.0 * s }, radiusX: 1.6 * s, radiusY: 1.6 * s }, &white);
    }

    let left = 56.0 * s;
    let title: Vec<u16> = if req.kind == Kind::Saved { "Clip saved" } else { "Couldn't save clip" }.encode_utf16().collect();
    let sub: Vec<u16> = req.text.encode_utf16().collect();
    rt.DrawText(&title, &title_fmt, &D2D_RECT_F { left, top: 12.0 * s, right: fw - 14.0 * s, bottom: 32.0 * s }, &text_b, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
    rt.DrawText(&sub, &sub_fmt, &D2D_RECT_F { left, top: 33.0 * s, right: fw - 14.0 * s, bottom: 52.0 * s }, &muted, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
    rt.EndDraw(None, None)?;
    Ok(anim)
}
