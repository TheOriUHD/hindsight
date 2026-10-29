// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! One exe, two processes:
//! - default: the recorder + tray icon. Never creates a webview, so nothing
//!   heavy runs next to your game.
//! - `--ui`: the library/settings window, spawned on demand. Closing it ends
//!   the process and frees all WebView2 memory. Talks to the recorder via `ipc`.

mod clips;
mod ipc;
mod system;
mod tray;
mod ui;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ui_mode = args.iter().any(|a| a == "--ui" || a == "--popout");
    system::init_logging(ui_mode);
    hindsight_encode::quiet_logs();
    if let Some(i) = args.iter().position(|a| a == "--popout") {
        // `--popout X Y`: tray icon position in physical pixels.
        let at = |k: usize| args.get(i + k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        ui::run(ui::Mode::Popout { x: at(1), y: at(2) });
    } else if ui_mode {
        ui::run(ui::Mode::Full);
    } else {
        tray::run(std::env::args().any(|a| a == "--autostart"));
    }
}
