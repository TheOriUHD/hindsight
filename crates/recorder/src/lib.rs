//! The always-on recorder: capture -> CFR hardware encode + audio -> replay
//! buffer -> clip on hotkey.

mod audio;
mod clock;
mod feedback;
pub mod hotkey;
mod replay;
mod settings;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hindsight_capture::{monitors, Backend, Capture, GpuDevice};
use hindsight_encode::{EncoderConfig, HwFrame, Mp4Writer, Packet, StreamParams, VideoEncoder};
use serde::Serialize;

pub use settings::{data_dir, CodecChoice, Quality, Settings};

use audio::AudioPipeline;
use clock::now_100ns;
use replay::Replay;

#[derive(Clone, Debug, Serialize, Default)]
pub struct Status {
    pub enabled: bool,
    pub recording: bool,
    pub error: Option<String>,
    pub encoder: String,
    pub monitor: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub buffered_secs: f64,
    pub buffer_mb: f64,
    pub audio_tracks: Vec<String>,
    pub dropped_frames: u64,
    pub last_clip: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MonitorInfo {
    pub id: String,
    pub label: String,
    pub primary: bool,
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    monitors()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, m)| MonitorInfo {
            label: format!("Display {} â€” {}Ã—{}{}", i + 1, m.width, m.height, if m.primary { " (primary)" } else { "" }),
            id: m.name,
            primary: m.primary,
        })
        .collect()
}

/// Frame slot (pts in 1/fps units) for a capture timestamp.
fn slot_at(t_100ns: i64, origin: i64, interval: f64) -> i64 {
    ((t_100ns - origin) as f64 / interval).floor() as i64
}

/// Owns the encoder. Receives converted frames from the capture thread over a
/// bounded channel and fills gaps (static screen) by repeating the last frame,
/// producing constant frame rate. No lock is shared with the capture thread:
/// both talk to the GPU driver, and sharing one deadlocked at high fps.
fn encoder_thread(
    mut enc: VideoEncoder,
    rx: Receiver<(HwFrame, i64)>,
    origin: i64,
    interval: f64,
    margin_slots: i64,
    replay: Arc<Mutex<Replay>>,
    error: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
) {
    let mut next_slot = 0i64;
    let mut out: Vec<Packet> = Vec::new();
    let fail = |e: anyhow::Error| {
        let mut err = error.lock().unwrap();
        if err.is_none() {
            log::error!("video encode: {e:#}");
        }
        *err = Some(format!("{e:#}"));
    };
    while !stop.load(Ordering::Relaxed) {
        let msg = rx.recv_timeout(Duration::from_millis(20));
        let r = (|| -> Result<()> {
            match msg {
                Ok((frame, mut slot)) => {
                    if !enc.has_frame() {
                        // First frame may carry an old present time (static desktop).
                        slot = slot.max(next_slot);
                    }
                    if slot >= next_slot {
                        while next_slot < slot && enc.has_frame() {
                            enc.repeat(next_slot, &mut |p| out.push(p))?;
                            next_slot += 1;
                        }
                        enc.encode(frame, slot, &mut |p| out.push(p))?;
                        next_slot = slot + 1;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(anyhow::anyhow!("stop")),
            }
            // Screen unchanged for a while: keep the timeline moving.
            let upto = slot_at(now_100ns(), origin, interval) - margin_slots;
            while next_slot < upto && enc.has_frame() {
                enc.repeat(next_slot, &mut |p| out.push(p))?;
                next_slot += 1;
            }
            Ok(())
        })();
        if !out.is_empty() {
            let mut replay = replay.lock().unwrap();
            for p in out.drain(..) {
                replay.push(p);
            }
        }
        match r {
            Ok(()) => {}
            Err(e) if e.to_string() == "stop" => break,
            Err(e) => fail(e),
        }
    }
}

struct Session {
    capture: Option<Capture>,
    encoder: Option<JoinHandle<()>>,
    encoder_stop: Arc<AtomicBool>,
    audio: Option<AudioPipeline>,
    replay: Arc<Mutex<Replay>>,
    error: Arc<Mutex<Option<String>>>,
    dropped: Arc<AtomicU64>,
    params: Arc<Vec<StreamParams>>,
    status: Status,
    clip_us: i64,
    origin: i64,
}

impl Drop for Session {
    fn drop(&mut self) {
        log::info!("stopping capture");
        drop(self.capture.take());
        log::info!("stopping encoder");
        self.encoder_stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.encoder.take() {
            let _ = t.join();
        }
        log::info!("stopping audio");
        drop(self.audio.take());
        log::info!("session stopped");
    }
}

fn open_session(s: &Settings) -> Result<Session> {
    let mons = monitors()?;
    let mon = s
        .monitor
        .as_ref()
        .and_then(|n| mons.iter().find(|m| &m.name == n))
        .or_else(|| mons.first())
        .context("no monitor found")?
        .clone();
    let gpu = GpuDevice::for_monitor(&mon)?;
    let (w, h) = s.output_size(mon.width, mon.height);
    let cfg = EncoderConfig {
        width: w,
        height: h,
        fps: s.fps,
        codec: hindsight_encode::Codec::H264,
        cq: s.cq(),
        max_kbps: s.max_kbps(w, h),
    };
    let enc = VideoEncoder::open_best(&gpu, &cfg, &s.codec_prefs())?;
    let encoder_name = enc.encoder_name.to_string();
    let mut params = vec![enc.params()?];

    let origin = now_100ns();
    let replay = Arc::new(Mutex::new(Replay::new(s.buffer_secs)));
    let audio = if s.desktop_audio || s.microphone {
        let (a, p) = audio::AudioPipeline::start(s.desktop_audio, s.microphone, origin, replay.clone())?;
        params.extend(p);
        Some(a)
    } else {
        None
    };

    let interval = 10_000_000.0 / s.fps as f64;
    let error = Arc::new(Mutex::new(None));
    let dropped = Arc::new(AtomicU64::new(0));
    // A few frames of slack; if the encoder falls behind, capture drops frames
    // instead of stalling (the gap is filled by repeats).
    let (tx, rx) = sync_channel::<(HwFrame, i64)>(4);
    let source = enc.frame_source();
    let encoder_stop = Arc::new(AtomicBool::new(false));
    let encoder = {
        let (replay, error, stop) = (replay.clone(), error.clone(), encoder_stop.clone());
        let margin = ((s.fps as f64 * 0.03).ceil() as i64).max(2);
        std::thread::Builder::new()
            .name("video-encoder".into())
            .spawn(move || encoder_thread(enc, rx, origin, interval, margin, replay, error, stop))?
    };

    let start_capture = |backend| {
        let tx = tx.clone();
        let (error, dropped) = (error.clone(), dropped.clone());
        let mut source = source.clone();
        let mut last_slot = i64::MIN;
        Capture::start(&gpu, &mon, backend, move |f| {
            // Decimate on the capture side so frames above the target fps are
            // never converted.
            let slot = slot_at(f.timestamp_100ns, origin, interval);
            if slot <= last_slot && last_slot != i64::MIN {
                return;
            }
            match source.convert(f.texture, f.width, f.height) {
                Ok(frame) => {
                    if tx.try_send((frame, slot)).is_ok() {
                        last_slot = slot;
                    } else {
                        dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    let mut err = error.lock().unwrap();
                    if err.is_none() {
                        log::error!("convert: {e:#}");
                    }
                    *err = Some(format!("{e:#}"));
                }
            }
        })
    };
    let capture = match start_capture(Backend::auto()) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("capture via {:?} failed ({e:#}), falling back to DXGI", Backend::auto());
            start_capture(Backend::Dxgi)?
        }
    };
    drop(tx);
    drop(source);

    let status = Status {
        recording: true,
        error: None,
        encoder: encoder_name,
        monitor: mon.name.clone(),
        width: w,
        height: h,
        fps: s.fps,
        audio_tracks: audio::track_layout(s.desktop_audio, s.microphone).iter().map(|t| t.to_string()).collect(),
        ..Default::default()
    };
    log::info!("recording {} {}x{}@{} via {} on {}", mon.name, w, h, s.fps, status.encoder, gpu.adapter_name);
    Ok(Session {
        capture: Some(capture),
        encoder: Some(encoder),
        encoder_stop,
        audio,
        replay,
        error,
        dropped,
        params: Arc::new(params),
        status,
        clip_us: s.buffer_secs as i64 * 1_000_000,
        origin,
    })
}

pub struct Recorder {
    lifecycle: Mutex<()>,
    session: Mutex<Option<Session>>,
    settings: Mutex<Settings>,
    error: Mutex<Option<String>>,
    last_clip: Mutex<Option<PathBuf>>,
    on_clip: Box<dyn Fn(&Path) + Send + Sync>,
}

impl Recorder {
    pub fn new(settings: Settings, on_clip: impl Fn(&Path) + Send + Sync + 'static) -> Arc<Self> {
        let rec = Arc::new(Self {
            lifecycle: Mutex::new(()),
            session: Mutex::new(None),
            settings: Mutex::new(settings.clone()),
            error: Mutex::new(None),
            last_clip: Mutex::new(None),
            on_clip: Box::new(on_clip),
        });
        let weak: Weak<Recorder> = Arc::downgrade(&rec);
        let binding = hotkey::Binding::parse(&settings.hotkey).unwrap_or_else(|_| hotkey::Binding::parse("F8").unwrap());
        hotkey::start(binding, move || {
            if let Some(r) = weak.upgrade() {
                r.save_clip_async();
            }
        });
        rec
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Start recording if enabled in settings. Errors are kept in the status.
    pub fn start(&self) {
        // Serialize restarts, but never hold `session` while tearing down or
        // opening one, so status queries stay responsive.
        let _lifecycle = self.lifecycle.lock().unwrap();
        let s = self.settings();
        let old = self.session.lock().unwrap().take();
        drop(old);
        if !s.enabled {
            return;
        }
        log::info!("starting session");
        match open_session(&s) {
            Ok(sess) => {
                *self.session.lock().unwrap() = Some(sess);
                *self.error.lock().unwrap() = None;
            }
            Err(e) => {
                log::error!("failed to start recording: {e:#}");
                *self.error.lock().unwrap() = Some(format!("{e:#}"));
            }
        }
    }

    pub fn stop(&self) {
        let _lifecycle = self.lifecycle.lock().unwrap();
        let old = self.session.lock().unwrap().take();
        drop(old);
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        let mut s = self.settings();
        s.enabled = enabled;
        self.apply_settings(s)
    }

    /// Persist new settings; restarts capture only if something it uses changed.
    pub fn apply_settings(&self, new: Settings) -> Result<()> {
        let new = new.sanitized();
        if !new.hotkey.is_empty() {
            hotkey::set_binding(hotkey::Binding::parse(&new.hotkey)?);
        }
        let old = std::mem::replace(&mut *self.settings.lock().unwrap(), new.clone());
        new.save()?;
        let restart = old.enabled != new.enabled
            || old.monitor != new.monitor
            || old.fps != new.fps
            || old.max_height != new.max_height
            || old.codec != new.codec
            || old.quality != new.quality
            || old.buffer_secs != new.buffer_secs
            || old.desktop_audio != new.desktop_audio
            || old.microphone != new.microphone
            || self.session.lock().unwrap().is_none();
        if restart {
            self.start();
        }
        Ok(())
    }

    pub fn status(&self) -> Status {
        let session = self.session.lock().unwrap();
        let mut st = match &*session {
            Some(s) => {
                let mut st = s.status.clone();
                let r = s.replay.lock().unwrap();
                st.buffered_secs = r.span_secs().min(s.clip_us as f64 / 1e6);
                st.buffer_mb = r.bytes() as f64 / 1e6;
                st.error = s.error.lock().unwrap().clone();
                st.dropped_frames = s.dropped.load(Ordering::Relaxed);
                st
            }
            None => Status { error: self.error.lock().unwrap().clone(), ..Default::default() },
        };
        st.last_clip = self.last_clip.lock().unwrap().clone();
        st.enabled = self.settings.lock().unwrap().enabled;
        st
    }

    pub fn save_clip_async(self: &Arc<Self>) {
        let r = self.clone();
        std::thread::spawn(move || match r.save_clip() {
            Ok(p) => log::info!("saved {}", p.display()),
            Err(e) => log::error!("save clip: {e:#}"),
        });
    }

    pub fn save_clip(&self) -> Result<PathBuf> {
        let t = Instant::now();
        let title = feedback::foreground_title();
        let pressed = now_100ns();
        // The clip ends at the key press; give the audio mixer (200 ms behind)
        // and the encoder's lookahead time to deliver that moment.
        std::thread::sleep(Duration::from_millis(300));
        let (packets, base, params) = {
            let session = self.session.lock().unwrap();
            let s = session.as_ref().context("not recording")?;
            let end = (pressed - s.origin) / 10;
            let (packets, base) = s.replay.lock().unwrap().snapshot(s.clip_us, end).context("nothing buffered yet")?;
            (packets, base, s.params.clone())
        };
        let settings = self.settings();
        std::fs::create_dir_all(&settings.output_dir)?;
        let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
        let mut path = settings.output_dir.join(format!("{title} {stamp}.mp4"));
        let mut n = 2;
        while path.exists() {
            path = settings.output_dir.join(format!("{title} {stamp} ({n}).mp4"));
            n += 1;
        }
        let refs: Vec<&StreamParams> = params.iter().collect();
        let mut w = Mp4Writer::create(&path, &refs)?;
        for p in &packets {
            w.write(p, base)?;
        }
        w.finish()?;
        log::info!("clip written in {} ms", t.elapsed().as_millis());
        if settings.save_sound {
            feedback::play_chime();
        }
        *self.last_clip.lock().unwrap() = Some(path.clone());
        (self.on_clip)(&path);
        Ok(path)
    }
}
