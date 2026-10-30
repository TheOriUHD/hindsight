//! UI process: the library/settings window. Recorder state goes through `ipc`;
//! file operations (list, trim, export, rename, delete) run here.

use std::path::PathBuf;

use hindsight_recorder::{list_monitors, AudioApp, AudioDevice, MonitorInfo, Settings};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_updater::UpdaterExt;

use crate::clips;
use crate::ipc::{self, Request};
use crate::system;

type Res<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn rpc<T: for<'de> serde::Deserialize<'de>>(req: Request) -> Res<T> {
    ipc::call(&req).map_err(|e| format!("{e:#}"))
}

fn output_dir() -> Res<PathBuf> {
    Ok(rpc::<Settings>(Request::Settings)?.output_dir)
}

/// The encoder in use, or while paused, the family of the GPU it would run on.
fn recording_encoder() -> String {
    let active = rpc::<Value>(Request::Status).ok().and_then(|v| v["encoder"].as_str().map(String::from)).unwrap_or_default();
    if !active.is_empty() {
        return active;
    }
    let monitor = rpc::<Settings>(Request::Settings).ok().and_then(|s| s.monitor);
    hindsight_recorder::encoder_vendor(monitor.as_deref()).to_string()
}

#[derive(Serialize, Clone)]
struct Progress {
    job: String,
    pct: f64,
}

#[tauri::command]
async fn get_status() -> Res<Value> {
    tauri::async_runtime::spawn_blocking(|| rpc(Request::Status)).await.map_err(err)?
}

#[tauri::command]
async fn get_settings() -> Res<Settings> {
    tauri::async_runtime::spawn_blocking(|| rpc(Request::Settings)).await.map_err(err)?
}

#[tauri::command]
async fn set_settings(handle: AppHandle, settings: Settings) -> Res<()> {
    allow_dir(&handle, &settings.output_dir);
    tauri::async_runtime::spawn_blocking(move || rpc::<Value>(Request::SetSettings { settings }).map(|_| ()))
        .await
        .map_err(err)?
}

#[tauri::command]
async fn set_enabled(enabled: bool) -> Res<()> {
    tauri::async_runtime::spawn_blocking(move || rpc::<Value>(Request::SetEnabled { enabled }).map(|_| ()))
        .await
        .map_err(err)?
}

#[tauri::command]
async fn save_clip() -> Res<PathBuf> {
    tauri::async_runtime::spawn_blocking(|| rpc(Request::SaveClip)).await.map_err(err)?
}

#[tauri::command]
fn monitors() -> Vec<MonitorInfo> {
    list_monitors()
}

#[tauri::command]
async fn list_clips(handle: AppHandle) -> Res<Vec<clips::ClipInfo>> {
    let dir = output_dir()?;
    allow_dir(&handle, &dir);
    tauri::async_runtime::spawn_blocking(move || {
        clips::list(&dir, move |clip, thumb| {
            let _ = handle.emit("thumb", (clip, thumb));
        })
    })
    .await
    .map_err(err)
}

#[tauri::command]
fn delete_clip(path: PathBuf) -> Res<()> {
    clips::delete(&path).map_err(err)
}

#[tauri::command]
async fn scrub_proxy(handle: AppHandle, path: PathBuf, generate: bool) -> Option<PathBuf> {
    tauri::async_runtime::spawn_blocking(move || {
        let enc = recording_encoder();
        clips::scrub_proxy(&path, generate, &enc, move |clip, proxy| {
            let _ = handle.emit("scrub-ready", (clip, proxy));
        })
    })
    .await
    .ok()
    .flatten()
}

#[tauri::command]
fn copy_file(path: PathBuf) -> Res<()> {
    system::copy_file_to_clipboard(&path).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn rename_clip(path: PathBuf, name: String) -> Res<PathBuf> {
    clips::rename(&path, &name).map_err(err)
}

#[tauri::command]
async fn filmstrip(path: PathBuf) -> Option<PathBuf> {
    tauri::async_runtime::spawn_blocking(move || clips::filmstrip(&path)).await.ok().flatten()
}

/// Open one of our own links in the default browser. Only known hosts, so a
/// page can't use this to launch arbitrary things.
#[tauri::command]
fn open_url(url: String) -> Res<()> {
    let allowed = ["https://github.com/TheOriUHD", "https://hindsighthq.app"];
    if !allowed.iter().any(|p| url == *p || url.starts_with(&format!("{p}/"))) {
        return Err("link not allowed".into());
    }
    system::open_with_default(std::path::Path::new(&url));
    Ok(())
}

#[derive(Serialize)]
struct UpdateInfo {
    version: String,
    notes: Option<String>,
}

/// HINDSIGHT_UPDATE_URL points at another manifest for testing. That can't
/// install anything unsigned: the signature check doesn't depend on the URL.
pub fn updater(handle: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    let mut b = handle.updater_builder();
    if let Ok(url) = std::env::var("HINDSIGHT_UPDATE_URL") {
        b = b.endpoints(vec![url.parse().map_err(err)?]).map_err(err)?;
    }
    b.build().map_err(err)
}

#[tauri::command]
async fn check_update(handle: AppHandle) -> Res<Option<UpdateInfo>> {
    let update = updater(&handle)?.check().await.map_err(err)?;
    Ok(update.map(|u| UpdateInfo { version: u.version.clone(), notes: u.body.clone() }))
}

/// Downloads, verifies the signature and runs the installer, which closes
/// Hindsight, updates it and starts it again.
#[tauri::command]
async fn install_update(handle: AppHandle) -> Res<()> {
    let Some(update) = updater(&handle)?.check().await.map_err(err)? else { return Ok(()) };
    let h = handle.clone();
    let mut done = 0u64;
    update
        .download_and_install(
            move |chunk, total| {
                done += chunk as u64;
                let pct = total.map(|t| done as f64 / t as f64).unwrap_or(0.0);
                let _ = h.emit("progress", Progress { job: "update".into(), pct });
            },
            || {},
        )
        .await
        .map_err(err)
}

#[tauri::command]
async fn report_requested() -> Res<bool> {
    tauri::async_runtime::spawn_blocking(|| rpc(Request::TakeReportRequest)).await.map_err(err)?
}

#[tauri::command]
async fn report_preview(handle: AppHandle) -> Res<Value> {
    let version = handle.package_info().version.to_string();
    tauri::async_runtime::spawn_blocking(move || crate::report::collect(&version)).await.map_err(err)
}

#[tauri::command]
async fn send_report(report: Value, description: String, contact: String) -> Res<String> {
    crate::report::send(report, description, contact).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn cancel_job() {
    clips::cancel_job();
}

#[tauri::command]
fn app_version(handle: AppHandle) -> String {
    handle.package_info().version.to_string()
}

#[tauri::command]
fn open_logs() {
    system::open_with_default(&hindsight_recorder::data_dir());
}

#[tauri::command]
fn show_in_folder(path: PathBuf) {
    system::show_in_explorer(&path);
}

#[tauri::command]
fn open_external(path: PathBuf) {
    system::open_with_default(&path);
}

#[tauri::command]
fn open_clips_folder() -> Res<()> {
    let dir = output_dir()?;
    let _ = std::fs::create_dir_all(&dir);
    system::open_with_default(&dir);
    Ok(())
}

#[tauri::command]
async fn pick_folder(handle: AppHandle) -> Option<PathBuf> {
    handle.dialog().file().blocking_pick_folder().and_then(|p| p.into_path().ok())
}

#[tauri::command]
async fn trim_clip(handle: AppHandle, path: PathBuf, start: f64, end: f64, exact: bool, to_exports: bool) -> Res<PathBuf> {
    tauri::async_runtime::spawn_blocking(move || {
        let enc = recording_encoder();
        clips::trim(&path, start, end, exact, to_exports, &enc, &|pct| {
            let _ = handle.emit("progress", Progress { job: "trim".into(), pct });
        })
    })
    .await
    .map_err(err)?
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn export_clip(handle: AppHandle, path: PathBuf, start: f64, end: f64, target_mb: Option<f64>, codec: String, label: String) -> Res<PathBuf> {
    tauri::async_runtime::spawn_blocking(move || {
        let enc = recording_encoder();
        let codec = if codec == "av1" { clips::ExportCodec::Av1 } else { clips::ExportCodec::H264 };
        clips::export(&path, start, end, target_mb, codec, &label, &enc, &|pct| {
            let _ = handle.emit("progress", Progress { job: "export".into(), pct });
        })
    })
    .await
    .map_err(err)?
    .map_err(|e| format!("{e:#}"))
}

/// Let the webview load clips and thumbnails through the asset protocol.
fn allow_dir(handle: &AppHandle, dir: &std::path::Path) {
    let _ = handle.asset_protocol_scope().allow_directory(dir, true);
}

pub enum Mode {
    Full,
    /// Small panel above the tray icon at (x, y), physical pixels.
    Popout { x: f64, y: f64 },
}

#[tauri::command]
async fn audio_devices(output: bool) -> Res<Vec<AudioDevice>> {
    tauri::async_runtime::spawn_blocking(move || hindsight_recorder::list_devices(output)).await.map_err(err)?.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn audio_apps() -> Res<Vec<AudioApp>> {
    tauri::async_runtime::spawn_blocking(hindsight_recorder::list_audio_apps).await.map_err(err)?.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn open_full_app(handle: AppHandle) -> Res<()> {
    tauri::async_runtime::spawn_blocking(|| rpc::<Value>(Request::OpenUi)).await.map_err(err)??;
    handle.exit(0);
    Ok(())
}

/// The popout page calls this once it has rendered, so it never flashes empty.
#[tauri::command]
fn popout_ready(window: tauri::WebviewWindow, handle: AppHandle) {
    let _ = window.show();
    // A freshly started process isn't allowed to take the foreground after a
    // tray click; borrow it from whoever has it so keyboard input (Esc) works.
    if let Ok(hwnd) = window.hwnd() {
        system::force_foreground(hwnd.0 as isize);
    }
    let _ = window.set_focus();
    if std::env::var_os("HINDSIGHT_POPOUT_PERSIST").is_some() {
        return;
    }
    // Close on any click outside the popout, like a Windows flyout. This
    // doesn't depend on focus, which Windows may still refuse to give us.
    let Ok(hwnd) = window.hwnd() else { return };
    let hwnd = hwnd.0 as isize;
    std::thread::spawn(move || {
        // Ignore the button that's still down from the tray click itself.
        std::thread::sleep(std::time::Duration::from_millis(250));
        loop {
            std::thread::sleep(std::time::Duration::from_millis(30));
            if system::clicked_outside(hwnd) {
                handle.exit(0);
                return;
            }
        }
    });
}

#[tauri::command]
fn close_popout(handle: AppHandle) {
    handle.exit(0);
}

const POPOUT_W: f64 = 340.0;
const POPOUT_H: f64 = 430.0;

/// Place the popout above the tray icon, inside the work area of its monitor.
fn place_popout(window: &tauri::WebviewWindow, x: f64, y: f64) -> tauri::Result<()> {
    let Some(mon) = window.monitor_from_point(x, y)?.or(window.primary_monitor()?) else { return Ok(()) };
    let s = mon.scale_factor();
    let (w, h) = (POPOUT_W * s, POPOUT_H * s);
    let wa = mon.work_area();
    let (left, top) = (wa.position.x as f64, wa.position.y as f64);
    let (right, bottom) = (left + wa.size.width as f64, top + wa.size.height as f64);
    let gap = 12.0 * s;
    let px = (x - w / 2.0).clamp(left + gap, right - w - gap);
    // Taskbar at the bottom (usual) or top.
    let py = if y >= top + (bottom - top) / 2.0 { bottom - h - gap } else { top + gap };
    window.set_position(tauri::PhysicalPosition::new(px.round() as i32, py.round() as i32))
}

pub fn run(mode: Mode) {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            let handle = app.handle().clone();
            allow_dir(&handle, &clips::thumbs_dir());
            let scrub_dir = hindsight_recorder::data_dir().join("scrub");
            let _ = std::fs::create_dir_all(&scrub_dir);
            allow_dir(&handle, &scrub_dir);
            match mode {
                Mode::Full => {
                    if let Ok(dir) = output_dir() {
                        allow_dir(&handle, &dir);
                    }
                    // An update relaunches whatever process installed it, which
                    // is this one. Bring the recorder back too.
                    std::thread::spawn(|| {
                        if ipc::call::<Value>(&Request::Status).is_err() {
                            if let Ok(exe) = std::env::current_exe() {
                                let _ = std::process::Command::new(exe).arg("--autostart").spawn();
                            }
                        }
                    });
                    WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                        .title("Hindsight")
                        .inner_size(1200.0, 780.0)
                        .min_inner_size(860.0, 560.0)
                        .background_color(tauri::window::Color(15, 16, 18, 255))
                        .theme(Some(tauri::Theme::Dark))
                        .build()?;
                }
                Mode::Popout { x, y } => {
                    let w = WebviewWindowBuilder::new(app, "popout", WebviewUrl::App("popout.html".into()))
                        .title("Hindsight")
                        .inner_size(POPOUT_W, POPOUT_H)
                        .resizable(false)
                        .decorations(false)
                        .shadow(true)
                        .skip_taskbar(true)
                        .always_on_top(true)
                        .visible(false)
                        .background_color(tauri::window::Color(19, 20, 23, 255))
                        .theme(Some(tauri::Theme::Dark))
                        .build()?;
                    place_popout(&w, x, y)?;
                    // Like a Windows flyout: gone once another app takes the
                    // foreground. Focus also bounces between the window and its
                    // webview while it appears, so check after a moment who
                    // actually has it instead of trusting a single blur.
                    // (HINDSIGHT_POPOUT_PERSIST=1 keeps it open for debugging.)
                    let h = handle.clone();
                    let persist = std::env::var_os("HINDSIGHT_POPOUT_PERSIST").is_some();
                    let had_focus = std::sync::atomic::AtomicBool::new(false);
                    w.on_window_event(move |ev| {
                        if let tauri::WindowEvent::Focused(true) = ev {
                            had_focus.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        if let tauri::WindowEvent::Focused(false) = ev {
                            // Never had focus (Windows refused it): the outside-click
                            // watcher closes the popout instead.
                            if persist || !had_focus.load(std::sync::atomic::Ordering::Relaxed) {
                                return;
                            }
                            let h = h.clone();
                            std::thread::spawn(move || {
                                std::thread::sleep(std::time::Duration::from_millis(150));
                                if !system::foreground_is_this_process() {
                                    h.exit(0);
                                }
                            });
                        }
                    });
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_settings,
            set_settings,
            set_enabled,
            save_clip,
            monitors,
            list_clips,
            delete_clip,
            scrub_proxy,
            app_version,
            open_logs,
            filmstrip,
            audio_devices,
            audio_apps,
            open_full_app,
            cancel_job,
            open_url,
            copy_file,
            check_update,
            install_update,
            report_requested,
            report_preview,
            send_report,
            popout_ready,
            close_popout,
            rename_clip,
            show_in_folder,
            open_external,
            open_clips_folder,
            pick_folder,
            trim_clip,
            export_clip,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run UI");
}
