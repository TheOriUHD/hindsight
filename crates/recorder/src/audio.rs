//! WASAPI capture onto the shared clock, and a mixer that turns the sources
//! into sample-aligned AAC tracks.
//!
//! Sources: an output device (loopback: everything you hear), an input device
//! (microphone), or a single app (per-process loopback, Windows 10 2004+), so
//! a clip can carry just the game without Discord or music.
//!
//! Each source keeps a timeline of samples indexed from the recorder origin.
//! Gaps (loopback delivers nothing while the system is silent, an app isn't
//! running yet) are filled with silence, so audio never drifts against video.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hindsight_encode::AudioEncoder;
use serde::{Deserialize, Serialize};
use windows::core::{implement, Interface, HRESULT, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::Threading::{
    CreateEventW, GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::clock::{now_100ns, sample_at, SAMPLE_RATE};
use crate::replay::Replay;

const CH: usize = 2;
/// How far behind "now" the mixer runs, giving sources time to deliver.
const MIX_LATENCY: i64 = SAMPLE_RATE / 5; // 200 ms

/// One thing to record audio from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSpec {
    /// Loopback of an output device (None = the Windows default).
    Output { device: Option<String> },
    /// A microphone / input device (None = the Windows default).
    Input { device: Option<String> },
    /// Only this app (and its child processes), by executable name.
    App { exe: String, name: String },
}

impl SourceSpec {
    pub fn title(&self) -> String {
        match self {
            SourceSpec::Output { .. } => "Desktop".into(),
            SourceSpec::Input { .. } => "Microphone".into(),
            SourceSpec::App { name, .. } => name.clone(),
        }
    }
}

/// Track titles for `sources`, in stream order (stream 1 onward; 0 is video).
/// One source is one track; several are mixed into "Mixed", optionally
/// followed by each source on its own track for editing.
pub fn track_titles(sources: &[SourceSpec], separate: bool) -> Vec<String> {
    match sources.len() {
        0 => vec![],
        1 => vec![sources[0].title()],
        _ => {
            let mut t = vec!["Mixed".to_string()];
            if separate {
                t.extend(sources.iter().map(SourceSpec::title));
            }
            t
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioApp {
    pub exe: String,
    pub name: String,
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
        // Never pad more than a few seconds at once, whatever a driver reports.
        let missing = (idx - self.end()).min(SAMPLE_RATE * 5);
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
    title: String,
    timeline: Arc<Mutex<Timeline>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Source {
    fn start(spec: SourceSpec, origin: i64) -> Self {
        let title = spec.title();
        let timeline = Arc::new(Mutex::new(Timeline::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (t, s) = (timeline.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name(format!("audio-{title}"))
            .spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                let mut quiet_errors = 0u32;
                while !s.load(Ordering::Relaxed) {
                    match run_source(&spec, origin, &t, &s) {
                        Ok(()) => quiet_errors = 0,
                        // An app that isn't running is normal; don't spam the log.
                        Err(e) => {
                            if quiet_errors == 0 {
                                log::info!("audio {}: {e:#}; retrying", spec.title());
                            }
                            quiet_errors += 1;
                        }
                    }
                    // Keep the timeline moving while the source is gone.
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
        Self { title, timeline, stop, thread: Some(thread) }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            log::info!("joining {} audio thread", self.title);
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

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { Ok(CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?) }
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

/// The requested device if it's present, otherwise the Windows default.
fn pick_device(flow: EDataFlow, wanted: &Option<String>) -> Result<IMMDevice> {
    let en = enumerator()?;
    unsafe {
        if let Some(id) = wanted {
            let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
            if let Ok(d) = en.GetDevice(PCWSTR(wide.as_ptr())) {
                if d.GetState().map(|s| s == DEVICE_STATE_ACTIVE).unwrap_or(false) {
                    return Ok(d);
                }
            }
        }
        Ok(en.GetDefaultAudioEndpoint(flow, eConsole)?)
    }
}

fn float_format() -> WAVEFORMATEXTENSIBLE {
    // 48 kHz stereo float; Windows converts whatever the device runs at.
    WAVEFORMATEXTENSIBLE {
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
    }
}

fn run_source(spec: &SourceSpec, origin: i64, timeline: &Mutex<Timeline>, stop: &AtomicBool) -> Result<()> {
    unsafe {
        let fmt = float_format();
        let base = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        let (client, mut still_valid): (IAudioClient, Box<dyn FnMut() -> bool>) = match spec {
            SourceSpec::Output { device } | SourceSpec::Input { device } => {
                let output = matches!(spec, SourceSpec::Output { .. });
                let flow = if output { eRender } else { eCapture };
                let dev = pick_device(flow, device)?;
                let id = device_id(&dev);
                let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
                let flags = if output { base | AUDCLNT_STREAMFLAGS_LOOPBACK } else { base };
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &fmt.Format, None)?;
                log::info!("audio {} started on {id}", spec.title());
                // Restart when the device we should use changes: the default
                // moved (headset plugged in) or a chosen device came back.
                let wanted = device.clone();
                (client, Box::new(move || pick_device(flow, &wanted).map(|d| device_id(&d)).unwrap_or_default() == id))
            }
            SourceSpec::App { exe, .. } => {
                let pid = find_process(exe).with_context(|| format!("{exe} isn't running"))?;
                let client = activate_process_loopback(pid)?;
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, base | AUDCLNT_STREAMFLAGS_LOOPBACK, 2_000_000, 0, &fmt.Format, None)?;
                log::info!("audio {} started (pid {pid})", spec.title());
                // Restart (and look the app up again) once it exits.
                (client, Box::new(move || process_alive(pid)))
            }
        };

        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        let result = capture_loop(&capture, origin, timeline, stop, event, &mut *still_valid);
        let _ = client.Stop();
        let _ = CloseHandle(event);
        result
    }
}

unsafe fn capture_loop(
    capture: &IAudioCaptureClient,
    origin: i64,
    timeline: &Mutex<Timeline>,
    stop: &AtomicBool,
    event: HANDLE,
    still_valid: &mut dyn FnMut() -> bool,
) -> Result<()> {
    let mut last_check = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        // Event-driven, but wake at least every 20 ms to pad silence and check `stop`.
        let _ = WaitForSingleObject(event, 20) == WAIT_OBJECT_0;
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
            // Some drivers (often microphones) and process loopback report 0 or
            // garbage QPC positions; fall back to "this packet just ended now".
            let now = sample_at(now_100ns(), origin);
            let mut at = sample_at(qpc as i64, origin);
            if qpc == 0 || (at - now).abs() > SAMPLE_RATE {
                at = now - frames as i64;
            }
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

        if last_check.elapsed() > Duration::from_secs(2) {
            last_check = Instant::now();
            if !still_valid() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/* ---------- per-process loopback ---------- */

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(std::sync::mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: windows_core::Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

/// PROPVARIANT holding a VT_BLOB, laid out as on x64.
#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    _pad: u32,
    data: *const u8,
}

unsafe fn activate_process_loopback(pid: u32) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let blob = BlobVariant {
        vt: 65, // VT_BLOB
        reserved: [0; 3],
        size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        _pad: 0,
        data: &params as *const _ as *const u8,
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
    let op = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        Some(&blob as *const BlobVariant as *const PROPVARIANT),
        &handler,
    )?;
    rx.recv_timeout(Duration::from_secs(5)).context("audio activation timed out")?;
    let mut hr = HRESULT(0);
    let mut unk = None;
    op.GetActivateResult(&mut hr, &mut unk)?;
    hr.ok()?;
    Ok(unk.context("no audio client")?.cast()?)
}

fn exe_of(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Pid of the running app `exe` (e.g. "FortniteClient-Win64-Shipping.exe").
/// With several processes of that name, the top one (whose parent isn't the
/// same app) is chosen; the process tree mode then covers its children.
fn find_process(exe: &str) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = Vec::new();
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let n = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                if String::from_utf16_lossy(&entry.szExeFile[..n]).eq_ignore_ascii_case(exe) {
                    found.push((entry.th32ProcessID, entry.th32ParentProcessID));
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        let pids: Vec<u32> = found.iter().map(|f| f.0).collect();
        found.iter().find(|(_, parent)| !pids.contains(parent)).or(found.first()).map(|f| f.0)
    }
}

fn process_alive(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code).is_ok() && code == 259; // STILL_ACTIVE
        let _ = CloseHandle(h);
        alive
    }
}

/* ---------- discovery for the settings UI ---------- */

/// Active output (`output = true`) or input devices.
pub fn list_devices(output: bool) -> Result<Vec<AudioDevice>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en = enumerator()?;
        let col = en.EnumAudioEndpoints(if output { eRender } else { eCapture }, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..col.GetCount()? {
            let d = col.Item(i)?;
            let name = d
                .OpenPropertyStore(STGM_READ)
                .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
                .map(|v| v.to_string())
                .unwrap_or_default();
            out.push(AudioDevice { id: device_id(&d), name });
        }
        Ok(out)
    }
}

/// Apps that currently have an audio session on any output device.
pub fn list_audio_apps() -> Result<Vec<AudioApp>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en = enumerator()?;
        let col = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let me = std::process::id();
        let own_exe = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_default();
        let mut out: Vec<AudioApp> = Vec::new();
        for i in 0..col.GetCount()? {
            let Ok(mgr) = col.Item(i)?.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
            let Ok(sessions) = mgr.GetSessionEnumerator() else { continue };
            for j in 0..sessions.GetCount().unwrap_or(0) {
                let Ok(ctl) = sessions.GetSession(j).and_then(|s| s.cast::<IAudioSessionControl2>()) else { continue };
                let pid = ctl.GetProcessId().unwrap_or(0);
                if pid == 0 || pid == me {
                    continue; // system sounds / ourselves
                }
                let Some(path) = exe_of(pid) else { continue };
                let exe = path.rsplit('\\').next().unwrap_or(&path).to_string();
                if exe.eq_ignore_ascii_case(&own_exe) || exe.eq_ignore_ascii_case("hindsight.exe") || out.iter().any(|a| a.exe.eq_ignore_ascii_case(&exe)) {
                    continue;
                }
                // The program's own description ("Spotify", "Google Chrome") beats
                // its window title, which for music players is the current song.
                let name = file_description(&path)
                    .or_else(|| window_title(pid))
                    .unwrap_or_else(|| exe.trim_end_matches(".exe").trim_end_matches(".EXE").to_string());
                out.push(AudioApp { exe, name });
            }
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(out)
    }
}

/// "FileDescription" from an executable's version resource.
fn file_description(path: &str) -> Option<String> {
    use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
    let query = |buf: &[u8], q: &str| -> Option<(*const u16, u32)> {
        let q: Vec<u16> = q.encode_utf16().chain([0]).collect();
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        unsafe { VerQueryValueW(buf.as_ptr() as _, PCWSTR(q.as_ptr()), &mut ptr, &mut len) }
            .as_bool()
            .then_some((ptr as *const u16, len))
    };
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, buf.as_mut_ptr() as _).ok()?;
        let (tr, len) = query(&buf, r"\VarFileInfo\Translation")?;
        if len < 4 {
            return None;
        }
        let (lang, cp) = (*tr, *tr.add(1));
        let (p, len) = query(&buf, &format!(r"\StringFileInfo\{lang:04x}{cp:04x}\FileDescription"))?;
        // Some resources report a length past the string; stop at the terminator.
        let raw = std::slice::from_raw_parts(p, len as usize);
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        let s = String::from_utf16_lossy(&raw[..end]).trim().to_string();
        (!s.is_empty() && s.len() <= 60).then_some(s)
    }
}

/// Title of the process's main visible window, a friendlier name than the exe.
fn window_title(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, GW_OWNER};
    struct Find {
        pid: u32,
        title: Option<String>,
    }
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
        let f = &mut *(lp.0 as *mut Find);
        let mut owner = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut owner));
        if owner == f.pid && IsWindowVisible(hwnd).as_bool() && GetWindow(hwnd, GW_OWNER).is_err() {
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(hwnd, &mut buf) as usize;
            if n > 0 {
                f.title = Some(String::from_utf16_lossy(&buf[..n]));
                return false.into();
            }
        }
        true.into()
    }
    let mut f = Find { pid, title: None };
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut f as *mut Find as isize));
    }
    f.title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty() && t.len() <= 60)
}

/* ---------- pipeline ---------- */

pub struct AudioPipeline {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioPipeline {
    /// Returns the pipeline plus the encoders' stream params (in `track_titles` order).
    pub fn start(
        sources: Vec<SourceSpec>,
        separate: bool,
        origin: i64,
        replay: Arc<Mutex<Replay>>,
    ) -> Result<(Self, Vec<hindsight_encode::StreamParams>)> {
        if sources.is_empty() {
            bail!("no audio sources");
        }
        let titles = track_titles(&sources, separate);
        let mut cursor = sample_at(now_100ns(), origin) - MIX_LATENCY;
        let mut encoders = Vec::new();
        for t in &titles {
            encoders.push(AudioEncoder::new(if t == "Microphone" { 128_000 } else { 192_000 }, cursor, t)?);
        }
        let params = encoders.iter().map(|e| e.params()).collect::<Result<Vec<_>>>()?;

        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new().name("audio-mixer".into()).spawn(move || {
            let n = sources.len();
            let live: Vec<Source> = sources.into_iter().map(|spec| Source::start(spec, origin)).collect();
            let mut bufs: Vec<Vec<f32>> = vec![Vec::new(); n];
            let mut mix = Vec::new();
            let mut pending = Vec::new();
            while !s.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                let target = sample_at(now_100ns(), origin) - MIX_LATENCY;
                if target <= cursor {
                    continue;
                }
                for (src, buf) in live.iter().zip(bufs.iter_mut()) {
                    src.timeline.lock().unwrap().take(cursor, target, buf);
                }
                cursor = target;

                let mut feeds: Vec<&[f32]> = Vec::new();
                if n == 1 {
                    feeds.push(&bufs[0]);
                } else {
                    mix.clear();
                    mix.resize(bufs[0].len(), 0.0);
                    for b in &bufs {
                        for (m, x) in mix.iter_mut().zip(b) {
                            *m += x;
                        }
                    }
                    for m in mix.iter_mut() {
                        *m = m.clamp(-1.0, 1.0);
                    }
                    feeds.push(&mix);
                    if separate {
                        feeds.extend(bufs.iter().map(|b| &b[..]));
                    }
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
