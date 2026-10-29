//! M0 spike: capture a monitor, optionally hardware-encode it to MP4, and report
//! frame delivery / encode stats. Run something animated (a game) while it records.
//!
//! Usage: hindsight-spike [--monitor N] [--backend wgc|dxgi] [--secs S]
//!                        [--record out.mp4] [--fps F] [--codec av1|hevc|h264] [--cq Q]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use hindsight_capture::{monitors, Backend, Capture, GpuDevice};
use hindsight_encode::{Codec, Encoder, EncoderConfig, Mp4Writer, Packet};

struct Args {
    monitor: usize,
    backend: Option<Backend>,
    secs: u64,
    record: Option<PathBuf>,
    fps: u32,
    codec: Option<Codec>,
    cq: u32,
}

fn parse_args() -> Result<Args> {
    let mut a = Args { monitor: 0, backend: None, secs: 5, record: None, fps: 120, codec: None, cq: 23 };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().unwrap_or_default();
        match k.as_str() {
            "--monitor" => a.monitor = v.parse()?,
            "--secs" => a.secs = v.parse()?,
            "--record" => a.record = Some(v.into()),
            "--fps" => a.fps = v.parse()?,
            "--cq" => a.cq = v.parse()?,
            "--codec" => a.codec = Some(match v.as_str() {
                "av1" => Codec::Av1,
                "hevc" => Codec::Hevc,
                "h264" => Codec::H264,
                _ => bail!("unknown codec {v}"),
            }),
            "--backend" => a.backend = Some(match v.as_str() {
                "wgc" => Backend::Wgc,
                "dxgi" => Backend::Dxgi,
                _ => bail!("unknown backend {v}"),
            }),
            _ => bail!("unknown arg {k}"),
        }
    }
    Ok(a)
}

#[derive(Default)]
struct Stats {
    stamps: Vec<i64>,
    size: (u32, u32),
    encode_ms: Vec<f64>,
    packets: Vec<Packet>,
    next_due: i64,
    error: Option<String>,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let all = monitors()?;
    for (i, m) in all.iter().enumerate() {
        println!("[{i}] {} {}x{}{} on {}", m.name, m.width, m.height, if m.primary { " (primary)" } else { "" }, m.adapter_name);
    }
    let Some(monitor) = all.get(args.monitor) else { bail!("no monitor {}", args.monitor) };
    let backend = args.backend.unwrap_or_else(Backend::auto);
    let gpu = GpuDevice::for_monitor(monitor)?;
    println!("capturing [{}] via {backend:?} on {}", args.monitor, gpu.adapter_name);

    let mut encoder = match &args.record {
        None => None,
        Some(_) => {
            let (w, h, fps) = (monitor.width, monitor.height, args.fps);
            // Peak cap ~0.1 bit/pixel: 4K120 ≈ 100 Mbps, 1440p120 ≈ 44 Mbps.
            let max_kbps = ((w as u64 * h as u64 * fps as u64) / 10_000) as u32;
            let cfg = EncoderConfig { width: w, height: h, fps, codec: Codec::Av1, cq: args.cq, max_kbps };
            let prefs = match args.codec {
                Some(c) => vec![c],
                None => vec![Codec::Av1, Codec::Hevc, Codec::H264],
            };
            let e = Encoder::open_best(&gpu, &cfg, &prefs)?;
            println!("encoder {} {w}x{h}@{fps} cq {} cap {} Mbps", e.encoder_name, args.cq, max_kbps / 1000);
            Some(e)
        }
    };
    let interval = 10_000_000 / args.fps as i64;

    let stats = Arc::new(Mutex::new(Stats::default()));
    let enc = Arc::new(Mutex::new(encoder.take()));
    let (s, e) = (stats.clone(), enc.clone());
    let cap = Capture::start(&gpu, monitor, backend, move |f| {
        let mut s = s.lock().unwrap();
        s.stamps.push(f.timestamp_100ns);
        s.size = (f.width, f.height);
        let mut enc = e.lock().unwrap();
        let Some(enc) = enc.as_mut() else { return };
        // Decimate to the target fps (display may run faster).
        if f.timestamp_100ns < s.next_due {
            return;
        }
        s.next_due = s.next_due.max(f.timestamp_100ns - interval) + interval;
        let t = Instant::now();
        let mut out = Vec::new();
        if let Err(err) = enc.encode(f.texture, f.width, f.height, f.timestamp_100ns, &mut |p| out.push(p)) {
            s.error.get_or_insert(format!("{err:#}"));
        }
        s.encode_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        s.packets.extend(out);
    })?;

    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(args.secs));
    drop(cap);
    let elapsed = start.elapsed().as_secs_f64();

    let mut s = stats.lock().unwrap();
    let (w, h) = s.size;
    println!("captured {} frames at {w}x{h} in {elapsed:.2}s -> {:.1} fps", s.stamps.len(), s.stamps.len() as f64 / elapsed);
    let deltas: Vec<f64> = s.stamps.windows(2).map(|p| (p[1] - p[0]) as f64 / 10_000.0).collect();
    print_pct("frame interval ms", deltas);
    if let Some(err) = &s.error {
        println!("ENCODE ERROR: {err}");
    }

    if let (Some(path), Some(mut enc)) = (&args.record, enc.lock().unwrap().take()) {
        print_pct("encode submit ms", std::mem::take(&mut s.encode_ms));
        let mut packets = std::mem::take(&mut s.packets);
        enc.flush(&mut |p| packets.push(p))?;
        let t = Instant::now();
        let mut w = Mp4Writer::create(path, &enc)?;
        for p in &packets {
            w.write(p, 0)?;
        }
        w.finish()?;
        let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
        println!(
            "wrote {} packets ({} keyframes), {:.1} MB, avg {:.1} Mbps, mux {:.0} ms -> {}",
            packets.len(),
            packets.iter().filter(|p| p.keyframe).count(),
            bytes as f64 / 1e6,
            bytes as f64 * 8.0 / elapsed / 1e6,
            t.elapsed().as_secs_f64() * 1000.0,
            path.display()
        );
    }
    Ok(())
}

fn print_pct(label: &str, mut v: Vec<f64>) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| v.get(((v.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0.0);
    println!("{label}: p50 {:.2}  p99 {:.2}  max {:.2}", pct(0.5), pct(0.99), pct(1.0));
}
