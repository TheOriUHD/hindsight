//! "Clip saved" popup: a small card that slides out of the right edge of the
//! recorded screen, stays for ~2 s, and slides back in.
//!
//! The window sits flush against the screen edge and never moves; each frame
//! shifts the pre-rendered card inside it, so the card appears to come out of
//! the bezel and never spills onto a neighbouring monitor. It never takes
//! focus, ignores the mouse, and is excluded from screen capture (so it never
//! shows up in later clips). Visible over windowed and borderless games;
//! exclusive-fullscreen games cover it, the chime still plays.

use std::cell::RefCell;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_numerics::Vector2;

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

// Timeline in ms: slide out, hold, slide back in.
const OUT_MS: f32 = 240.0;
const HOLD_UNTIL_MS: f32 = 2300.0;
const IN_MS: f32 = 200.0;

/// Show the popup. `monitor` is an HMONITOR; 0 = the monitor the user is on.
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

/// A 32-bit premultiplied top-down DIB selected into its own DC.
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    bits: *mut u32,
    w: i32,
    h: i32,
}

impl Dib {
    unsafe fn new(w: i32, h: i32) -> anyhow::Result<Self> {
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
        Ok(Self { dc, bmp, bits: bits as *mut u32, w, h })
    }

    fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

struct Anim {
    start: Instant,
    card: Vec<u32>,
    frame: Dib,
    x: i32,
    y: i32,
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
        // Keep the popup out of our own (and any other) screen capture.
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
                        SetTimer(Some(hwnd), TIMER, 10, None);
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

/// How far the card is out of the edge, 0..=1, at `t` ms.
fn reveal(t: f32) -> Option<f32> {
    let out = |p: f32| 1.0 - (1.0 - p).powi(3); // decelerate out of the edge
    let back = |p: f32| p * p * p; // accelerate back into it
    if t < OUT_MS {
        Some(out(t / OUT_MS))
    } else if t < HOLD_UNTIL_MS {
        Some(1.0)
    } else if t < HOLD_UNTIL_MS + IN_MS {
        Some(1.0 - back((t - HOLD_UNTIL_MS) / IN_MS))
    } else {
        None
    }
}

unsafe fn step(hwnd: HWND) {
    let done = ANIM.with(|a| {
        let mut a = a.borrow_mut();
        let Some(a) = a.as_mut() else { return true };
        let Some(p) = reveal(a.start.elapsed().as_secs_f32() * 1000.0) else { return true };
        let (w, h) = (a.frame.w, a.frame.h);
        // Card shifted right by the hidden part; whatever is past the window's
        // right edge (the screen edge) is simply not drawn.
        let shift = ((1.0 - p) * w as f32).round() as usize;
        let w = w as usize;
        let card = &a.card;
        let px = a.frame.pixels();
        for row in 0..h as usize {
            let dst = &mut px[row * w..(row + 1) * w];
            let src = &card[row * w..(row + 1) * w];
            dst[..shift.min(w)].fill(0);
            if shift < w {
                dst[shift..].copy_from_slice(&src[..w - shift]);
            }
        }
        let pos = POINT { x: a.x, y: a.y };
        let size = SIZE { cx: a.frame.w, cy: a.frame.h };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let _ = UpdateLayeredWindow(hwnd, None, Some(&pos), Some(&size), Some(a.frame.dc), Some(&POINT::default()), COLORREF(0), Some(&blend), ULW_ALPHA);
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
    let hmon = if req.monitor != 0 {
        HMONITOR(req.monitor as _)
    } else {
        MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY)
    };
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let _ = GetMonitorInfoW(hmon, &mut info);
    let (mut dpi, mut _dy) = (96u32, 96u32);
    let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi, &mut _dy);
    let s = dpi as f32 / 96.0;

    let (w, h) = ((264.0 * s).round() as i32, (58.0 * s).round() as i32);
    let x = info.rcMonitor.right - w;
    let y = info.rcMonitor.top + (72.0 * s) as i32;

    // Draw the card once; frames are shifted copies of it.
    let mut card_dib = Dib::new(w, h)?;
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
    rt.BindDC(card_dib.dc, &RECT { left: 0, top: 0, right: w, bottom: h })?;
    rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

    let (fw, fh) = (w as f32, h as f32);
    let accent = if req.kind == Kind::Saved { 0xe5484d } else { 0xe2a336 };
    let bg = rt.CreateSolidColorBrush(&color(0x18191d, 1.0), None)?;
    let border = rt.CreateSolidColorBrush(&color(0x2d3037, 1.0), None)?;
    let accent_b = rt.CreateSolidColorBrush(&color(accent, 1.0), None)?;
    let text_b = rt.CreateSolidColorBrush(&color(0xe7e8eb, 1.0), None)?;
    let muted = rt.CreateSolidColorBrush(&color(0x8d919b, 1.0), None)?;

    let title_fmt = dw.CreateTextFormat(w!("Segoe UI Variable Text"), None, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, 13.5 * s, w!("en-us"))?;
    let sub_fmt = dw.CreateTextFormat(w!("Segoe UI Variable Text"), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, 12.0 * s, w!("en-us"))?;
    for f in [&title_fmt, &sub_fmt] {
        f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        let sign = dw.CreateEllipsisTrimmingSign(f)?;
        let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
        f.SetTrimming(&trim, &sign)?;
    }

    rt.BeginDraw();
    rt.Clear(Some(&color(0, 0.0)));
    // Only the left corners are rounded; the right side runs into the screen
    // edge (the extra width past `fw` is clipped by the window).
    let r = 10.0 * s;
    let card = D2D1_ROUNDED_RECT { rect: D2D_RECT_F { left: 0.5, top: 0.5, right: fw + r * 2.0, bottom: fh - 0.5 }, radiusX: r, radiusY: r };
    rt.FillRoundedRectangle(&card, &bg);
    rt.DrawRoundedRectangle(&card, &border, 1.0, None);
    // Accent bar along the leading edge.
    let bar = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F { left: 9.0 * s, top: 14.0 * s, right: 12.0 * s, bottom: fh - 14.0 * s },
        radiusX: 1.5 * s,
        radiusY: 1.5 * s,
    };
    rt.FillRoundedRectangle(&bar, &accent_b);
    // Record dot before the title.
    let left = 24.0 * s;
    let title_top = 11.0 * s;
    rt.FillEllipse(
        &D2D1_ELLIPSE { point: Vector2 { X: left + 4.0 * s, Y: title_top + 10.0 * s }, radiusX: 4.0 * s, radiusY: 4.0 * s },
        &accent_b,
    );

    let title: Vec<u16> = if req.kind == Kind::Saved { "Clip saved" } else { "Clip not saved" }.encode_utf16().collect();
    let sub: Vec<u16> = req.text.encode_utf16().collect();
    let text_left = left + 15.0 * s;
    rt.DrawText(&title, &title_fmt, &D2D_RECT_F { left: text_left, top: title_top, right: fw - 16.0 * s, bottom: title_top + 20.0 * s }, &text_b, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
    rt.DrawText(&sub, &sub_fmt, &D2D_RECT_F { left: text_left, top: title_top + 20.0 * s, right: fw - 16.0 * s, bottom: fh - 6.0 * s }, &muted, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
    rt.EndDraw(None, None)?;

    let card = card_dib.pixels().to_vec();
    Ok(Anim { start: Instant::now(), card, frame: Dib::new(w, h)?, x, y })
}
