//! Headless smoke test: record, press the hotkey via SendInput, report the clip.
use std::time::Duration;

use hindsight_recorder::{Recorder, Settings};
use windows::Win32::UI::Input::KeyboardAndMouse::*;

fn press(vk: VIRTUAL_KEY) {
    let mk = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: flags, ..Default::default() } },
    };
    unsafe { SendInput(&[mk(KEYBD_EVENT_FLAGS(0)), mk(KEYEVENTF_KEYUP)], std::mem::size_of::<INPUT>() as i32) };
}

fn main() {
    hindsight_encode::quiet_logs();
    let mut s = Settings::default();
    s.fps = 120;
    s.buffer_secs = 5;
    s.output_dir = std::env::temp_dir().join("hindsight-test");
    s.save_sound = false;
    let (tx, rx) = std::sync::mpsc::channel();
    let rec = Recorder::new(s.clone(), move |p| { let _ = tx.send(p.to_path_buf()); });
    
    rec.start();
    std::thread::sleep(Duration::from_secs(7));
    println!("{:#?}", rec.status());
    press(VK_F8);
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(p) => println!("CLIP {}", p.display()),
        Err(_) => println!("NO CLIP (hotkey not received)"),
    }
}

