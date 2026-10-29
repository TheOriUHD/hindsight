//! M0 spike: capture the primary monitor for a few seconds and report frame
//! delivery stats. Run something animated (a game, a video) while it records.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use hindsight_capture::{GpuDevice, MonitorCapture};

fn main() -> Result<()> {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let gpu = GpuDevice::new()?;

    let stamps = Arc::new(Mutex::new(Vec::<i64>::with_capacity(4096)));
    let size = Arc::new(Mutex::new((0u32, 0u32)));
    let (s, z) = (stamps.clone(), size.clone());
    let cap = MonitorCapture::primary(&gpu, move |f| {
        s.lock().unwrap().push(f.timestamp_100ns);
        *z.lock().unwrap() = (f.width, f.height);
    })?;

    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(secs));
    drop(cap);
    let elapsed = start.elapsed().as_secs_f64();

    let stamps = stamps.lock().unwrap();
    let (w, h) = *size.lock().unwrap();
    let mut deltas: Vec<f64> = stamps.windows(2).map(|p| (p[1] - p[0]) as f64 / 10_000.0).collect();
    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| deltas.get(((deltas.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0.0);

    println!("captured {} frames at {w}x{h} in {elapsed:.2}s -> {:.1} fps", stamps.len(), stamps.len() as f64 / elapsed);
    println!("frame interval ms: p50 {:.2}  p99 {:.2}  max {:.2}", pct(0.5), pct(0.99), pct(1.0));
    Ok(())
}
