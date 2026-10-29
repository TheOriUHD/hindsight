//! UI process: the library/settings window. Recorder state goes through `ipc`;
//! file operations (list, trim, export, rename, delete) run here.

use std::path::PathBuf;

use hindsight_recorder::{list_monitors, MonitorInfo, Settings};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::DialogExt;

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

fn recording_encoder() -> String {
    rpc::<Value>(Request::Status).ok().and_then(|v| v["encoder"].as_str().map(String::from)).unwrap_or_default()
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
async fn scrub_proxy(handle: AppHandle, path: PathBuf) -> Option<PathBuf> {
    tauri::async_runtime::spawn_blocking(move || {
        let enc = recording_encoder();
        clips::scrub_proxy(&path, &enc, move |clip, proxy| {
            let _ = handle.emit("scrub-ready", (clip, proxy));
        })
    })
    .await
    .ok()
    .flatten()
}

#[tauri::command]
fn rename_clip(path: PathBuf, name: String) -> Res<PathBuf> {
    clips::rename(&path, &name).map_err(err)
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
async fn trim_clip(handle: AppHandle, path: PathBuf, start: f64, end: f64, exact: bool) -> Res<PathBuf> {
    tauri::async_runtime::spawn_blocking(move || {
        let enc = recording_encoder();
        clips::trim(&path, start, end, exact, &enc, &|pct| {
            let _ = handle.emit("progress", Progress { job: "trim".into(), pct });
        })
    })
    .await
    .map_err(err)?
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn export_clip(handle: AppHandle, path: PathBuf, start: f64, end: f64, target_mb: f64) -> Res<PathBuf> {
    tauri::async_runtime::spawn_blocking(move || {
        let enc = recording_encoder();
        clips::export_for_size(&path, start, end, target_mb, &enc, &|pct| {
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

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let handle = app.handle().clone();
            allow_dir(&handle, &clips::thumbs_dir());
            let scrub_dir = hindsight_recorder::data_dir().join("scrub");
            let _ = std::fs::create_dir_all(&scrub_dir);
            allow_dir(&handle, &scrub_dir);
            if let Ok(dir) = output_dir() {
                allow_dir(&handle, &dir);
            }
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Hindsight")
                .inner_size(1200.0, 780.0)
                .min_inner_size(860.0, 560.0)
                .background_color(tauri::window::Color(14, 15, 19, 255))
                .build()?;
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
