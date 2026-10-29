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
            let line = format!("{} {:5} {}\n", chrono_like_now(), r.level(), r.args());
            let _ = self.0.lock().unwrap().write_all(line.as_bytes());
        }
    }
    fn flush(&self) {}
}

fn chrono_like_now() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:03}", d.as_secs(), d.subsec_millis())
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
