//! Memory profile: prints private bytes after each pipeline stage.
use std::time::Duration;

use hindsight_capture::{monitors, Backend, Capture, GpuDevice};
use hindsight_encode::{Codec, EncoderConfig, VideoEncoder};

fn private_mb() -> f64 {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!("(Get-Process -Id {}).PrivateMemorySize64", std::process::id())])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1e6
}

fn main() {
    let codec = match std::env::args().nth(1).as_deref() {
        Some("hevc") => Codec::Hevc,
        Some("h264") => Codec::H264,
        _ => Codec::Av1,
    };
    println!("start: {:.0} MB", private_mb());
    let m = monitors().unwrap().remove(0);
    let gpu = GpuDevice::for_monitor(&m).unwrap();
    println!("d3d device: {:.0} MB", private_mb());
    let cfg = EncoderConfig { width: m.width, height: m.height, fps: 60, codec, cq: 23, max_kbps: 100_000 };
    let enc = std::sync::Arc::new(std::sync::Mutex::new(VideoEncoder::new(&gpu, cfg).unwrap()));
    println!("encoder open ({codec:?}): {:.0} MB", private_mb());
    let e = enc.clone();
    let mut n = 0i64;
    let cap = Capture::start(&gpu, &m, Backend::auto(), move |f| {
        let _ = e.lock().unwrap().encode(f.texture, f.width, f.height, n, &mut |_| {});
        n += 1;
    })
    .unwrap();
    std::thread::sleep(Duration::from_secs(5));
    println!("after 5s encoding: {:.0} MB", private_mb());
    drop(cap);
}
