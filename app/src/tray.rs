//! Recorder process: tray icon, hotkeys, capture/encode, IPC server.

use std::process::{Child, Command};
use std::sync::{Arc, Mutex};

use hindsight_recorder::{Recorder, Settings};
use serde_json::json;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent};

use crate::ipc::{self, Request};
use crate::system;

struct State {
    recorder: Arc<Recorder>,
    ui: Mutex<Option<Child>>,
    popout: Mutex<Option<Child>>,
}

/// Toggle the small tray popout, anchored at the tray icon (physical px).
fn toggle_popout(handle: &AppHandle, x: f64, y: f64) {
    let state = handle.state::<State>();
    let mut popout = state.popout.lock().unwrap();
    if let Some(mut child) = popout.take() {
        if matches!(child.try_wait(), Ok(None)) {
            // Clicking the icon while it's open closes it, like Windows flyouts.
            let _ = child.kill();
            return;
        }
    }
    // The click gave us foreground rights; let the popout take focus so it
    // can close itself when focus leaves.
    system::allow_foreground();
    let args = ["--popout".to_string(), format!("{}", x.round() as i64), format!("{}", y.round() as i64)];
    match std::env::current_exe().and_then(|exe| Command::new(exe).args(&args).spawn()) {
        Ok(child) => *popout = Some(child),
        Err(e) => log::error!("failed to start popout: {e}"),
    }
}

/// Show the UI process, starting it if needed.
pub fn open_ui(handle: &AppHandle) {
    let state = handle.state::<State>();
    let mut ui = state.ui.lock().unwrap();
    if let Some(child) = ui.as_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            system::focus_process_window(child.id());
            return;
        }
    }
    match std::env::current_exe().and_then(|exe| Command::new(exe).arg("--ui").spawn()) {
        Ok(child) => *ui = Some(child),
        Err(e) => log::error!("failed to start UI: {e}"),
    }
}

fn menu(handle: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let s = handle.state::<State>().recorder.settings();
    let save = MenuItem::with_id(handle, "save", format!("Save clip ({})", s.hotkey), s.enabled, None::<&str>)?;
    let toggle = MenuItem::with_id(handle, "toggle", if s.enabled { "Pause recording" } else { "Resume recording" }, true, None::<&str>)?;
    let open = MenuItem::with_id(handle, "open", "Open Hindsight", true, None::<&str>)?;
    let folder = MenuItem::with_id(handle, "folder", "Open clips folder", true, None::<&str>)?;
    let quit = MenuItem::with_id(handle, "quit", "Quit Hindsight", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(handle)?;
    let sep2 = PredefinedMenuItem::separator(handle)?;
    Menu::with_items(handle, &[&open, &sep, &save, &toggle, &folder, &sep2, &quit])
}

fn refresh(handle: &AppHandle) {
    let h = handle.clone();
    // Tray APIs must run on the main thread.
    let _ = handle.run_on_main_thread(move || {
        if let Some(tray) = h.tray_by_id("tray") {
            if let Ok(m) = menu(&h) {
                let _ = tray.set_menu(Some(m));
            }
            let st = h.state::<State>().recorder.status();
            let tip = if st.recording {
                format!("Hindsight — recording {}p{}", st.height, st.fps)
            } else if st.error.is_some() {
                "Hindsight — not recording (open for details)".into()
            } else {
                "Hindsight — paused".into()
            };
            let _ = tray.set_tooltip(Some(tip));
        }
    });
}

fn handle_ipc(handle: &AppHandle, req: Request) -> anyhow::Result<serde_json::Value> {
    let rec = handle.state::<State>().recorder.clone();
    Ok(match req {
        Request::Status => serde_json::to_value(rec.status())?,
        Request::Settings => serde_json::to_value(rec.settings())?,
        Request::SetSettings { settings } => {
            if rec.settings().start_with_windows != settings.start_with_windows {
                system::set_autostart(settings.start_with_windows)?;
            }
            rec.apply_settings(settings)?;
            refresh(handle);
            json!(null)
        }
        Request::SaveClip => serde_json::to_value(rec.save_clip()?)?,
        Request::SetEnabled { enabled } => {
            rec.set_enabled(enabled)?;
            refresh(handle);
            json!(null)
        }
        Request::OpenUi => {
            let h = handle.clone();
            handle.run_on_main_thread(move || open_ui(&h))?;
            json!(null)
        }
    })
}

pub fn run(autostarted: bool) {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|handle, _args, _cwd| open_ui(handle)))
        .setup(move |app| {
            let handle = app.handle().clone();
            let settings = Settings::load();
            let _ = std::fs::create_dir_all(&settings.output_dir);
            if settings.start_with_windows {
                let _ = system::set_autostart(true);
            }
            let recorder = Recorder::new(settings, |_| {});
            app.manage(State { recorder: recorder.clone(), ui: Mutex::new(None), popout: Mutex::new(None) });

            TrayIconBuilder::with_id("tray")
                .icon(app.default_window_icon().cloned().unwrap())
                .tooltip("Hindsight")
                .menu(&menu(&handle)?)
                .show_menu_on_left_click(false)
                .on_menu_event(|handle, ev| {
                    let rec = handle.state::<State>().recorder.clone();
                    match ev.id().as_ref() {
                        "save" => rec.save_clip_async(),
                        "toggle" => {
                            let on = !rec.settings().enabled;
                            let h = handle.clone();
                            // Starting capture takes a moment; keep the tray responsive.
                            std::thread::spawn(move || {
                                let _ = rec.set_enabled(on);
                                refresh(&h);
                            });
                        }
                        "open" => open_ui(handle),
                        "folder" => {
                            let dir = rec.settings().output_dir;
                            let _ = std::fs::create_dir_all(&dir);
                            system::open_with_default(&dir);
                        }
                        "quit" => {
                            let state = handle.state::<State>();
                            for slot in [&state.ui, &state.popout] {
                                if let Some(mut c) = slot.lock().unwrap().take() {
                                    let _ = c.kill();
                                }
                            }
                            rec.stop();
                            handle.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, ev| match ev {
                    TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, position, .. } => {
                        toggle_popout(tray.app_handle(), position.x, position.y);
                    }
                    TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => open_ui(tray.app_handle()),
                    _ => {}
                })
                .build(app)?;

            let h = handle.clone();
            ipc::serve(move |req| handle_ipc(&h, req));

            let h = handle.clone();
            std::thread::spawn(move || {
                recorder.start();
                refresh(&h);
            });

            if !autostarted {
                open_ui(&handle);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build app");

    app.run(|_, event| {
        // No windows live in this process; only "Quit" ends it.
        if let RunEvent::ExitRequested { api, code: None, .. } = event {
            api.prevent_exit();
        }
    });
}
