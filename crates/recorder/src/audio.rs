//! WASAPI capture (desktop loopback + microphone) onto the shared clock, and a
//! mixer that turns the sources into sample-aligned AAC tracks.
//!
//! Each source keeps a timeline of samples indexed from the recorder origin.
//! Gaps (loopback delivers nothing while the system is silent) are filled with
//! silence, so audio never drifts against video.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use hindsight_encode::AudioEncoder;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};

use crate::clock::{now_100ns, sample_at, SAMPLE_RATE};
use crate::replay::Replay;

const CH: usize = 2;
/// How far behind "now" the mixer runs, giving sources time to deliver.
const MIX_LATENCY: i64 = SAMPLE_RATE / 5; // 200 ms

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceKind {
    Desktop,
    Microphone,
}

#[derive(Default)]
struct Timeline {
    /// Sample index of `data[0]` (frames, not floats).
    start: i64,
    data: VecDeque<f32>,
    started: bool,
}

impl Timeline {
    fn end(&self) -> i64 {
        self.start + (self.data.len() / CH) as i64
    }

    fn pad_to(&mut self, idx: i64) {
        if !self.started {
            self.start = idx;
            self.started = true;
        }
        let missing = idx - self.end();
        if missing > 0 {
            self.data.extend(std::iter::repeat(0.0).take(missing as usize * CH));
        }
    }

    /// Take samples [from, to) — zero-filled where absent — and drop everything before `to`.
    fn take(&mut self, from: i64, to: i64, out: &mut Vec<f32>) {
        out.clear();
        out.resize((to - from) as usize * CH, 0.0);
        if self.started {
            for i in from.max(self.start)..to.min(self.end()) {
                let s = ((i - self.start) as usize) * CH;
                let d = ((i - from) as usize) * CH;
                out[d] = self.data[s];
                out[d + 1] = self.data[s + 1];
            }
            let drop = ((to - self.start).clamp(0, (self.data.len() / CH) as i64)) as usize;
            self.data.drain(..drop * CH);
            self.start += drop as i64;
            if self.start < to {
                self.start = to;
            }
        }
    }
}

struct Source {
    timeline: Arc<Mutex<Timeline>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Source {
    fn start(kind: SourceKind, origin: i64) -> Self {
        let timeline = Arc::new(Mutex::new(Timeline::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (t, s) = (timeline.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name(format!("audio-{kind:?}"))
            .spawn(move || {
                unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
                while !s.load(Ordering::Relaxed) {
                    if let Err(e) = run_source(kind, origin, &t, &s) {
                        log::warn!("audio {kind:?}: {e:#}; retrying");
                    }
                    // Keep the timeline moving while the device is gone.
                    for _ in 0..10 {
                        if s.load(Ordering::Relaxed) {
                            break;
                        }
                        pad_silence(&t, origin);
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .expect("spawn audio thread");
        Self { timeline, stop, thread: Some(thread) }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn pad_silence(t: &Mutex<Timeline>, origin: i64) {
    let now = sample_at(now_100ns(), origin);
    let mut t = t.lock().unwrap();
    // Leave headroom for packets still in flight.
    let target = now - SAMPLE_RATE / 20;
    if !t.started || target - t.end() > SAMPLE_RATE / 10 {
        t.pad_to(target);
    }
}

fn default_device(kind: SourceKind) -> Result<IMMDevice> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let flow = if kind == SourceKind::Desktop { eRender } else { eCapture };
        Ok(en.GetDefaultAudioEndpoint(flow, eConsole)?)
    }
}

fn device_id(d: &IMMDevice) -> String {
    unsafe {
        match d.GetId() {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as _));
                s
            }
            Err(_) => String::new(),
        }
    }
}

fn run_source(kind: SourceKind, origin: i64, timeline: &Mutex<Timeline>, stop: &AtomicBool) -> Result<()> {
    unsafe {
        let device = default_device(kind)?;
        let id = device_id(&device);
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;

        // Ask for 48 kHz stereo float; Windows converts whatever the device runs at.
        let fmt = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                nChannels: CH as u16,
                nSamplesPerSec: SAMPLE_RATE as u32,
                nAvgBytesPerSec: SAMPLE_RATE as u32 * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
            dwChannelMask: 3,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };
        let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if kind == SourceKind::Desktop {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &fmt.Format, None)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        log::info!("audio {kind:?} started on {id}");

        let mut last_check = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            loop {
                let n = capture.GetNextPacketSize()?;
                if n == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut bflags = 0u32;
                let mut qpc = 0u64;
                capture.GetBuffer(&mut data, &mut frames, &mut bflags, None, Some(&mut qpc))?;
                let at = sample_at(qpc as i64, origin);
                {
                    let mut t = timeline.lock().unwrap();
                    if !t.started || at > t.end() + SAMPLE_RATE / 100 {
                        t.pad_to(at);
                    }
                    if bflags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        t.data.extend(std::iter::repeat(0.0).take(frames as usize * CH));
                    } else {
                        let s = std::slice::from_raw_parts(data as *const f32, frames as usize * CH);
                        t.data.extend(s.iter().copied());
                    }
                }
                capture.ReleaseBuffer(frames)?;
            }
            pad_silence(timeline, origin);

            // Follow the Windows default device (headset plugged in etc.).
            if last_check.elapsed() > Duration::from_secs(2) {
                last_check = Instant::now();
                if default_device(kind).map(|d| device_id(&d)).unwrap_or_default() != id {
                    log::info!("audio {kind:?} default device changed");
                    let _ = client.Stop();
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = client.Stop();
        Ok(())
    }
}

/// Audio tracks in output order. Stream indices start at 1 (0 is video).
pub fn track_layout(desktop: bool, mic: bool) -> Vec<&'static str> {
    match (desktop, mic) {
        (true, true) => vec!["Game + Mic", "Desktop", "Microphone"],
        (true, false) => vec!["Desktop"],
        (false, true) => vec!["Microphone"],
        (false, false) => vec![],
    }
}

pub struct AudioPipeline {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioPipeline {
    /// Returns the pipeline plus the encoders' stream params (in `track_layout` order).
    pub fn start(
        desktop: bool,
        mic: bool,
        origin: i64,
        replay: Arc<Mutex<Replay>>,
    ) -> Result<(Self, Vec<hindsight_encode::StreamParams>)> {
        let titles = track_layout(desktop, mic);
        let mut cursor = sample_at(now_100ns(), origin) - MIX_LATENCY;
        let mut encoders = Vec::new();
        for t in &titles {
            encoders.push(AudioEncoder::new(if *t == "Microphone" { 128_000 } else { 192_000 }, cursor, t)?);
        }
        let params = encoders.iter().map(|e| e.params()).collect::<Result<Vec<_>>>()?;

        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new().name("audio-mixer".into()).spawn(move || {
            let desk = desktop.then(|| Source::start(SourceKind::Desktop, origin));
            let micro = mic.then(|| Source::start(SourceKind::Microphone, origin));
            let (mut a, mut b, mut mix) = (Vec::new(), Vec::new(), Vec::new());
            let mut pending = Vec::new();
            while !s.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                let target = sample_at(now_100ns(), origin) - MIX_LATENCY;
                if target <= cursor {
                    continue;
                }
                if let Some(d) = &desk {
                    d.timeline.lock().unwrap().take(cursor, target, &mut a);
                }
                if let Some(m) = &micro {
                    m.timeline.lock().unwrap().take(cursor, target, &mut b);
                }
                cursor = target;

                let mut feeds: Vec<&[f32]> = Vec::new();
                match (desktop, mic) {
                    (true, true) => {
                        mix.clear();
                        mix.extend(a.iter().zip(&b).map(|(x, y)| (x + y).clamp(-1.0, 1.0)));
                        feeds.extend([&mix[..], &a[..], &b[..]]);
                    }
                    (true, false) => feeds.push(&a),
                    (false, true) => feeds.push(&b),
                    _ => {}
                }
                for (i, (enc, data)) in encoders.iter_mut().zip(feeds).enumerate() {
                    let r = enc.push(data, &mut |mut p| {
                        p.stream = i + 1;
                        pending.push(p);
                    });
                    if let Err(e) = r {
                        log::error!("audio encode: {e:#}");
                    }
                }
                if !pending.is_empty() {
                    let mut r = replay.lock().unwrap();
                    for p in pending.drain(..) {
                        r.push(p);
                    }
                }
            }
        })?;
        Ok((Self { stop, thread: Some(thread) }, params))
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
