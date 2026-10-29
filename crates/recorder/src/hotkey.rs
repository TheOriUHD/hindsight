//! Global hotkeys via Raw Input. Works while fullscreen games have focus,
//! never swallows the key from the game, and needs no hooks or injection.
//!
//! Bindings look like "F8", "Ctrl+Shift+S", "Alt+Mouse5".

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Result};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Binding {
    ctrl: bool,
    alt: bool,
    shift: bool,
    win: bool,
    key: Key,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Key {
    #[default]
    None,
    Vk(u16),
    Mouse(u8), // 4 or 5
}

impl Binding {
    pub fn parse(s: &str) -> Result<Self> {
        let mut b = Binding::default();
        for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => b.ctrl = true,
                "alt" => b.alt = true,
                "shift" => b.shift = true,
                "win" | "meta" => b.win = true,
                "mouse4" => b.key = Key::Mouse(4),
                "mouse5" => b.key = Key::Mouse(5),
                _ => match vk_from_name(part) {
                    Some(vk) => b.key = Key::Vk(vk),
                    None => bail!("unknown key '{part}'"),
                },
            }
        }
        if b.key == Key::None {
            bail!("hotkey needs a non-modifier key");
        }
        Ok(b)
    }

    fn uses_mouse(&self) -> bool {
        matches!(self.key, Key::Mouse(_))
    }
}

fn vk_from_name(name: &str) -> Option<u16> {
    let n = name.to_ascii_uppercase();
    if let Some(f) = n.strip_prefix('F').and_then(|d| d.parse::<u16>().ok()) {
        return (1..=24).contains(&f).then(|| VK_F1.0 + f - 1);
    }
    if n.len() == 1 {
        let c = n.as_bytes()[0];
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u16);
        }
    }
    if let Some(d) = n.strip_prefix("NUM").and_then(|d| d.parse::<u16>().ok()) {
        return (d <= 9).then(|| VK_NUMPAD0.0 + d);
    }
    Some(match n.as_str() {
        "INSERT" => VK_INSERT.0,
        "DELETE" => VK_DELETE.0,
        "HOME" => VK_HOME.0,
        "END" => VK_END.0,
        "PAGEUP" => VK_PRIOR.0,
        "PAGEDOWN" => VK_NEXT.0,
        "PAUSE" => VK_PAUSE.0,
        "SCROLLLOCK" => VK_SCROLL.0,
        "PRINTSCREEN" => VK_SNAPSHOT.0,
        "SPACE" => VK_SPACE.0,
        "TAB" => VK_TAB.0,
        "BACKQUOTE" => VK_OEM_3.0,
        "MINUS" => VK_OEM_MINUS.0,
        "EQUAL" => VK_OEM_PLUS.0,
        "BRACKETLEFT" => VK_OEM_4.0,
        "BRACKETRIGHT" => VK_OEM_6.0,
        "BACKSLASH" => VK_OEM_5.0,
        "SEMICOLON" => VK_OEM_1.0,
        "QUOTE" => VK_OEM_7.0,
        "COMMA" => VK_OEM_COMMA.0,
        "PERIOD" => VK_OEM_PERIOD.0,
        "SLASH" => VK_OEM_2.0,
        "NUMPADMULTIPLY" => VK_MULTIPLY.0,
        "NUMPADADD" => VK_ADD.0,
        "NUMPADSUBTRACT" => VK_SUBTRACT.0,
        "NUMPADDECIMAL" => VK_DECIMAL.0,
        "NUMPADDIVIDE" => VK_DIVIDE.0,
        _ => return None,
    })
}

type Callback = Arc<dyn Fn() + Send + Sync>;

struct State {
    binding: Binding,
    callback: Callback,
    held: HashSet<u16>,
    hwnd: isize,
    mouse_registered: bool,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// Start the hotkey thread. Call once; use `set_binding` to change keys.
pub fn start(binding: Binding, callback: impl Fn() + Send + Sync + 'static) {
    let _ = STATE.set(Mutex::new(State {
        binding,
        callback: Arc::new(callback),
        held: HashSet::new(),
        hwnd: 0,
        mouse_registered: false,
    }));
    std::thread::Builder::new()
        .name("hotkeys".into())
        .spawn(|| {
            if let Err(e) = run() {
                log::error!("hotkey thread: {e:#}");
            }
        })
        .expect("spawn hotkey thread");
}

pub fn set_binding(binding: Binding) {
    if let Some(s) = STATE.get() {
        let mut s = s.lock().unwrap();
        s.binding = binding;
        if s.hwnd != 0 {
            register(&mut s);
        }
    }
}

fn register(s: &mut State) {
    let hwnd = HWND(s.hwnd as _);
    let size = std::mem::size_of::<RAWINPUTDEVICE>() as u32;
    let kb = RAWINPUTDEVICE { usUsagePage: 1, usUsage: 6, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
    unsafe {
        let _ = RegisterRawInputDevices(&[kb], size);
    }
    // Only listen to the mouse when bound to a mouse button: 8 kHz mice would
    // otherwise wake this thread thousands of times a second for nothing.
    let want_mouse = s.binding.uses_mouse();
    if want_mouse != s.mouse_registered {
        let mouse = if want_mouse {
            RAWINPUTDEVICE { usUsagePage: 1, usUsage: 2, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd }
        } else {
            RAWINPUTDEVICE { usUsagePage: 1, usUsage: 2, dwFlags: RIDEV_REMOVE, hwndTarget: HWND::default() }
        };
        unsafe {
            let _ = RegisterRawInputDevices(&[mouse], size);
        }
        s.mouse_registered = want_mouse;
    }
}

fn run() -> Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: w!("HindsightHotkeys"), ..Default::default() };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("HindsightHotkeys"),
            w!(""),
            WINDOW_STYLE::default(),
            0, 0, 0, 0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        )?;
        {
            let mut s = STATE.get().unwrap().lock().unwrap();
            s.hwnd = hwnd.0 as isize;
            register(&mut s);
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

fn foreground_is_us() -> bool {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        pid == GetCurrentProcessId()
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        let mut raw = RAWINPUT::default();
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        let n = GetRawInputData(
            HRAWINPUT(lparam.0 as _),
            RID_INPUT,
            Some(&mut raw as *mut _ as *mut _),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        );
        if n != u32::MAX && n > 0 {
            handle(&raw);
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn handle(raw: &RAWINPUT) {
    let Some(state) = STATE.get() else { return };
    let mut fire = false;
    {
        let mut s = state.lock().unwrap();
        let b = s.binding.clone();
        let mods_ok = |held: &HashSet<u16>| {
            let has = |vk: VIRTUAL_KEY| held.contains(&vk.0);
            has(VK_CONTROL) == b.ctrl && has(VK_MENU) == b.alt && has(VK_SHIFT) == b.shift && (has(VK_LWIN) || has(VK_RWIN)) == b.win
        };
        if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
            let kb = unsafe { raw.data.keyboard };
            let vk = match VIRTUAL_KEY(kb.VKey) {
                VK_LCONTROL | VK_RCONTROL => VK_CONTROL.0,
                VK_LMENU | VK_RMENU => VK_MENU.0,
                VK_LSHIFT | VK_RSHIFT => VK_SHIFT.0,
                v => v.0,
            };
            let up = kb.Flags as u32 & RI_KEY_BREAK != 0;
            if up {
                s.held.remove(&vk);
            } else if s.held.insert(vk) {
                // Fresh press (not auto-repeat).
                fire = b.key == Key::Vk(vk) && mods_ok(&s.held);
            }
        } else if raw.header.dwType == RIM_TYPEMOUSE.0 {
            let flags = unsafe { raw.data.mouse.Anonymous.Anonymous.usButtonFlags } as u32;
            let pressed = if flags & RI_MOUSE_BUTTON_4_DOWN != 0 {
                Some(4)
            } else if flags & RI_MOUSE_BUTTON_5_DOWN != 0 {
                Some(5)
            } else {
                None
            };
            if let Some(btn) = pressed {
                fire = b.key == Key::Mouse(btn) && mods_ok(&s.held);
            }
        }
    }
    // Don't fire while the user is typing a new hotkey into our own window.
    if fire && !foreground_is_us() {
        let cb = state.lock().unwrap().callback.clone();
        cb();
    }
}
