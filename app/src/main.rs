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
    system::init_logging(std::env::args().any(|a| a == "--ui"));
    hindsight_encode::quiet_logs();
    if std::env::args().any(|a| a == "--ui") {
        ui::run();
    } else {
        tray::run(std::env::args().any(|a| a == "--autostart"));
    }
}
