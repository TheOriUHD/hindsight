//! Counts delivered frames per backend: capture_probe <monitor index> <wgc|dxgi> <seconds>
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use hindsight_capture::{monitors, Backend, Capture, GpuDevice};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let idx: usize = a[1].parse().unwrap();
    let backend = if a[2] == "wgc" { Backend::Wgc } else { Backend::Dxgi };
    let secs: u64 = a[3].parse().unwrap();
    let all = monitors().unwrap();
    for (i, m) in all.iter().enumerate() {
        println!("[{i}] {} {}x{}", m.name, m.width, m.height);
    }
    let gpu = GpuDevice::for_monitor(&all[idx]).unwrap();
    let n = Arc::new(AtomicU64::new(0));
    let c = n.clone();
    let cap = Capture::start(&gpu, &all[idx], backend, 240, move |_| { c.fetch_add(1, Ordering::Relaxed); }).unwrap();
    for s in 1..=secs {
        std::thread::sleep(std::time::Duration::from_secs(1));
        println!("t={s}s frames={}", n.load(Ordering::Relaxed));
    }
    drop(cap);
}
