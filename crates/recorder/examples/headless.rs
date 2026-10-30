//! Headless smoke test: record a few seconds, save a clip, report it.
use std::time::Duration;

use hindsight_recorder::{Recorder, Settings};

fn main() {
    hindsight_encode::quiet_logs();
    let mut s = Settings::default();
    s.fps = 60;
    s.buffer_secs = 5;
    s.output_dir = std::env::temp_dir().join("hindsight-test");
    s.save_sound = false;
    s.save_overlay = false;
    if std::env::args().any(|a| a == "wgc") {
        s.capture_method = hindsight_recorder::CaptureMethod::Wgc;
    }
    let rec = Recorder::new(s, |_| {});
    rec.start();
    for t in 1..=8 {
        std::thread::sleep(Duration::from_secs(1));
        println!("t={t}s dropped={}", rec.status().dropped_frames);
    }
    let st = rec.status();
    println!("capture={} encoder={} recording={} dropped={} err={:?}", st.capture, st.encoder, st.recording, st.dropped_frames, st.error);
    match rec.save_clip() {
        Ok(p) => println!("CLIP {}", p.display()),
        Err(e) => println!("SAVE FAILED: {e:#}"),
    }
}
