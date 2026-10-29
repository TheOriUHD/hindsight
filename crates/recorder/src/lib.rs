//! The always-on recorder: capture -> CFR hardware encode + audio -> replay
//! buffer -> clip on hotkey.

mod audio;
mod clock;
mod feedback;
pub mod hotkey;
mod replay;
mod settings;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hindsight_capture::{monitors, Backend, Capture, Frame, GpuDevice};
use hindsight_encode::{EncoderConfig, Mp4Writer, Packet, StreamParams, VideoEncoder};
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
            label: format!("Display {} — {}×{}{}", i + 1, m.width, m.height, if m.primary { " (primary)" } else { "" }),
            id: m.name,
            primary: m.primary,
        })
        .collect()
}

struct VideoState {
    enc: VideoEncoder,
    origin: i64,
    interval: f64,
    next_slot: i64,
    margin_slots: i64,
    replay: Arc<Mutex<Replay>>,
    error: Option<String>,
}

impl VideoState {
    fn slot_at(&self, t_100ns: i64) -> i64 {
        ((t_100ns - self.origin) as f64 / self.interval).floor() as i64
    }

    fn on_frame(&mut self, f: Frame) {
        let mut slot = self.slot_at(f.timestamp_100ns);
        if !self.enc.has_frame() {
            // First frame may carry an old present time (static desktop).
            slot = slot.max(self.next_slot);
        }
        if slot < self.next_slot {
            return; // display runs faster than the target fps
        }
        let mut out = Vec::new();
        let r = self.fill(slot, &mut out).and_then(|_| {
            self.enc.encode(f.texture, f.width, f.height, slot, &mut |p| out.push(p))
        });
        self.next_slot = slot + 1;
        self.finish(r, out);
    }

    /// Repeat the last frame for every slot before `upto` that got no new frame.
    fn fill(&mut self, upto: i64, out: &mut Vec<Packet>) -> Result<()> {
        while self.next_slot < upto && self.enc.has_frame() {
            self.enc.repeat(self.next_slot, &mut |p| out.push(p))?;
            self.next_slot += 1;
        }
        Ok(())
    }

    fn tick(&mut self) {
        let upto = self.slot_at(now_100ns()) - self.margin_slots;
        let mut out = Vec::new();
        let r = self.fill(upto, &mut out);
        self.finish(r, out);
    }

    fn finish(&mut self, r: Result<()>, out: Vec<Packet>) {
        if let Err(e) = r {
            if self.error.is_none() {
                log::error!("video encode: {e:#}");
            }
            self.error = Some(format!("{e:#}"));
        }
        if !out.is_empty() {
            let mut replay = self.replay.lock().unwrap();
            for p in out {
                replay.push(p);
            }
        }
    }
}

struct Session {
    capture: Option<Capture>,
    pacer_stop: Arc<AtomicBool>,
    pacer: Option<JoinHandle<()>>,
    audio: Option<AudioPipeline>,
    video: Arc<Mutex<VideoState>>,
    replay: Arc<Mutex<Replay>>,
    params: Arc<Vec<StreamParams>>,
    status: Status,
    clip_us: i64,
    origin: i64,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Stop capture callbacks first, then the pacer, so nothing touches the
        // encoder while it is torn down.
        log::info!("stopping capture");
        drop(self.capture.take());
        self.pacer_stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.pacer.take() {
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

    let video = Arc::new(Mutex::new(VideoState {
        enc,
        origin,
        interval: 10_000_000.0 / s.fps as f64,
        next_slot: 0,
        margin_slots: ((s.fps as f64 * 0.03).ceil() as i64).max(2),
        replay: replay.clone(),
        error: None,
    }));

    let start_capture = |backend| {
        let v = video.clone();
        Capture::start(&gpu, &mon, backend, move |f| v.lock().unwrap().on_frame(f))
    };
    let capture = match start_capture(Backend::auto()) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("capture via {:?} failed ({e:#}), falling back to DXGI", Backend::auto());
            start_capture(Backend::Dxgi)?
        }
    };

    let pacer_stop = Arc::new(AtomicBool::new(false));
    let (ps, v) = (pacer_stop.clone(), video.clone());
    let pacer = std::thread::Builder::new().name("video-pacer".into()).spawn(move || {
        while !ps.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
            v.lock().unwrap().tick();
        }
    })?;

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
        pacer_stop,
        pacer: Some(pacer),
        audio,
        video,
        replay,
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
                st.error = s.video.lock().unwrap().error.clone();
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
