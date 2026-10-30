//! Small Windows integrations: autostart, Explorer, logging.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn set_autostart(on: bool) -> std::io::Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
    if on {
        let exe = std::env::current_exe()?;
        key.set_value("Hindsight", &format!("\"{}\" --autostart", exe.display()))
    } else {
        match key.delete_value("Hindsight") {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
}

/// Bring the top-level window of process `pid` to the front.
pub fn focus_process_window(pid: u32) {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
        let mut owner = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut owner));
        if owner == lp.0 as u32 && IsWindowVisible(hwnd).as_bool() {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(hwnd);
            return false.into();
        }
        true.into()
    }
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(pid as isize));
    }
}

/// Tie a child process to this one: it's killed when we exit, so closing the
/// window mid-export doesn't leave ffmpeg running. Only ffmpeg goes in here.
/// The update installer must outlive us, so this process itself stays out.
pub fn adopt_child(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::*;
    static JOB: OnceLock<isize> = OnceLock::new();
    unsafe {
        let job = *JOB.get_or_init(|| {
            let Ok(job) = CreateJobObjectW(None, None) else { return 0 };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let _ = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            // Deliberately never closed: Windows closes it when we exit, and
            // that is what kills the children.
            job.0 as isize
        });
        if job != 0 {
            let _ = AssignProcessToJobObject(HANDLE(job as _), HANDLE(child.as_raw_handle() as _));
        }
    }
}

/// Put a file on the clipboard the way Explorer's Copy does, so pasting into
/// Discord, a browser or a folder attaches the file itself.
pub fn copy_file_to_clipboard(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT};
    use windows::Win32::UI::Shell::DROPFILES;
    const CF_HDROP: u32 = 15;

    anyhow::ensure!(path.is_file(), "the file is gone");
    // A double-terminated list of paths, here with one entry.
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    let header = std::mem::size_of::<DROPFILES>();
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, header + wide.len() * 2)?;
        let p = GlobalLock(mem) as *mut u8;
        anyhow::ensure!(!p.is_null(), "out of memory");
        let drop = p as *mut DROPFILES;
        (*drop).pFiles = header as u32;
        (*drop).fWide = true.into();
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p.add(header) as *mut u16, wide.len());
        let _ = GlobalUnlock(mem);

        // Other apps hold the clipboard for a moment while reading it.
        let mut open = OpenClipboard(None);
        for _ in 0..10 {
            if open.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            open = OpenClipboard(None);
        }
        open?;
        let _ = EmptyClipboard();
        // On success the clipboard owns `mem`.
        let set = SetClipboardData(CF_HDROP, Some(HANDLE(mem.0)));
        let _ = CloseClipboard();
        set?;
    }
    Ok(())
}

/// Bring `hwnd` to the foreground even when Windows' foreground lock would
/// refuse it, by briefly attaching to the input queue of the current
/// foreground thread.
pub fn force_foreground(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow};
    unsafe {
        let hwnd = HWND(hwnd as _);
        let fg_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let me = GetCurrentThreadId();
        let attached = fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(me, fg_thread, false);
        }
    }
}

/// True while a mouse button is down with the cursor outside `hwnd`.
pub fn clicked_outside(hwnd: isize) -> bool {
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect};
    unsafe {
        let down = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON].iter().any(|k| GetAsyncKeyState(k.0 as i32) as u16 & 0x8000 != 0);
        if !down {
            return false;
        }
        let (mut pt, mut r) = (POINT::default(), RECT::default());
        if GetCursorPos(&mut pt).is_err() || GetWindowRect(HWND(hwnd as _), &mut r).is_err() {
            return false;
        }
        pt.x < r.left || pt.x >= r.right || pt.y < r.top || pt.y >= r.bottom
    }
}

pub fn foreground_is_this_process() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid)) };
    pid == std::process::id()
}

/// Let whichever process we start next take the foreground (it may only do so
/// if we currently own it, e.g. right after a tray click).
pub fn allow_foreground() {
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(windows::Win32::UI::WindowsAndMessaging::ASFW_ANY);
    }
}

pub fn show_in_explorer(path: &Path) {
    // raw_arg: explorer wants `/select,"C:\path with spaces"` unsplit.
    let _ = Command::new("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
}

pub fn open_with_default(path: &Path) {
    let _ = Command::new("explorer").arg(path).spawn();
}

struct FileLogger(Mutex<std::fs::File>);

impl log::Log for FileLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) && r.target().starts_with("hindsight") {
            let line = format!("{} {:5} {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"), r.level(), r.args());
            let _ = self.0.lock().unwrap().write_all(line.as_bytes());
        }
    }
    fn flush(&self) {}
}

/// Log to <data dir>\hindsight.log (or hindsight-ui.log). Appends, because a
/// second launch briefly runs before handing off to the running instance and
/// must not clobber its log; rotates once the file passes 2 MB.
pub fn init_logging(ui: bool) {
    let dir = hindsight_recorder::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(if ui { "hindsight-ui.log" } else { "hindsight.log" });
    if std::fs::metadata(&path).map(|m| m.len() > 2_000_000).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("old.log"));
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = log::set_boxed_logger(Box::new(FileLogger(Mutex::new(f))));
        log::set_max_level(log::LevelFilter::Info);
    }
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        log::error!(target: "hindsight", "panic on thread '{thread}': {info}\n{}", std::backtrace::Backtrace::force_capture());
        default(info);
    }));
}
