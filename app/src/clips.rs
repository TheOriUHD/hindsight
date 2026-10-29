//! Clip library on disk (the folder is the source of truth) and edit jobs run
//! through the bundled ffmpeg.exe: thumbnails, trims and size-targeted exports.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};
use serde::Serialize;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Serialize, Clone)]
pub struct ClipInfo {
    pub path: PathBuf,
    pub name: String,
    pub game: String,
    pub created: u64,
    pub size: u64,
    pub duration: f64,
    pub thumb: Option<PathBuf>,
}

pub fn ffmpeg_exe() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("ffmpeg.exe"))).unwrap_or_else(|| "ffmpeg.exe".into())
}

pub fn thumbs_dir() -> PathBuf {
    hindsight_recorder::data_dir().join("thumbs")
}

/// "Valorant 2026-10-29 15-30-12 (2)" -> "Valorant"
fn game_from_name(stem: &str) -> String {
    let parts: Vec<&str> = stem.split(' ').collect();
    for i in 0..parts.len() {
        let p = parts[i];
        if p.len() == 10 && p.as_bytes()[4] == b'-' && p.as_bytes()[7] == b'-' && p[..4].chars().all(|c| c.is_ascii_digit()) {
            let g = parts[..i].join(" ");
            return if g.is_empty() { "Clips".into() } else { g };
        }
    }
    "Clips".into()
}

fn thumb_path(clip: &Path, mtime: u64) -> PathBuf {
    let mut h = DefaultHasher::new();
    clip.hash(&mut h);
    mtime.hash(&mut h);
    thumbs_dir().join(format!("{:016x}.jpg", h.finish()))
}

pub fn list(dir: &Path, on_thumb: impl Fn(PathBuf, PathBuf) + Send + Sync + 'static) -> Vec<ClipInfo> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let path = e.path();
        if !path.extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4")) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let thumb = thumb_path(&path, mtime);
        out.push(ClipInfo {
            game: game_from_name(&stem),
            name: stem,
            created: mtime,
            size: meta.len(),
            duration: hindsight_encode::probe_duration(&path).unwrap_or(0.0),
            thumb: thumb.exists().then(|| thumb.clone()),
            path: path.clone(),
        });
    }
    out.sort_by(|a, b| b.created.cmp(&a.created));
    let missing: Vec<_> = out.iter().filter(|c| c.thumb.is_none()).map(|c| (c.path.clone(), thumb_path(&c.path, c.created))).collect();
    if !missing.is_empty() {
        let cb = std::sync::Arc::new(on_thumb);
        for (clip, thumb) in missing {
            let cb = cb.clone();
            queue_thumb(clip, thumb, Box::new(move |c, t| cb(c, t)));
        }
    }
    out
}

type ThumbJob = (PathBuf, PathBuf, Box<dyn FnOnce(PathBuf, PathBuf) + Send>);

/// One background worker so a big library doesn't spawn dozens of ffmpegs.
fn queue_thumb(clip: PathBuf, thumb: PathBuf, done: Box<dyn FnOnce(PathBuf, PathBuf) + Send>) {
    static TX: OnceLock<Sender<ThumbJob>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = channel::<ThumbJob>();
        std::thread::Builder::new()
            .name("thumbs".into())
            .spawn(move || {
                for (clip, thumb, done) in rx {
                    if thumb.exists() || make_thumb(&clip, &thumb).is_ok() {
                        done(clip, thumb);
                    }
                }
            })
            .unwrap();
        tx
    });
    let _ = tx.send((clip, thumb, done));
}

fn make_thumb(clip: &Path, thumb: &Path) -> Result<()> {
    std::fs::create_dir_all(thumbs_dir())?;
    let status = Command::new(ffmpeg_exe())
        .args(["-v", "error", "-y", "-ss", "1", "-i"])
        .arg(clip)
        .args(["-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "4"])
        .arg(thumb)
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    if !status.success() {
        bail!("ffmpeg thumbnail failed");
    }
    Ok(())
}

/// Next free "<stem> <suffix>.mp4" next to `src`.
fn sibling(src: &Path, suffix: &str) -> PathBuf {
    let dir = src.parent().unwrap_or(Path::new("."));
    let stem = src.file_stem().unwrap_or_default().to_string_lossy();
    let mut p = dir.join(format!("{stem} {suffix}.mp4"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem} {suffix} {n}.mp4"));
        n += 1;
    }
    p
}

/// H.264 hardware encoder matching the GPU the recorder runs on.
fn h264_encoder(recording_encoder: &str) -> &'static str {
    if recording_encoder.contains("nvenc") {
        "h264_nvenc"
    } else if recording_encoder.contains("amf") {
        "h264_amf"
    } else {
        "h264_mf"
    }
}

fn same_family_encoder(recording_encoder: &str, src: &Path) -> String {
    // Re-encode with the codec the clip already uses so quality/size stay alike.
    let codec = probe_codec(src).unwrap_or_else(|| "h264".into());
    let vendor = if recording_encoder.contains("nvenc") {
        "nvenc"
    } else if recording_encoder.contains("amf") {
        "amf"
    } else {
        "mf"
    };
    format!("{codec}_{vendor}")
}

fn probe_codec(src: &Path) -> Option<String> {
    let out = Command::new(ffmpeg_exe()).arg("-hide_banner").arg("-i").arg(src).creation_flags(CREATE_NO_WINDOW).output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    let line = text.lines().find(|l| l.contains("Video:"))?;
    let codec = line.split("Video:").nth(1)?.split_whitespace().next()?.trim_end_matches(',');
    Some(match codec {
        "hevc" | "h265" => "hevc",
        "av1" => "av1",
        _ => "h264",
    }.into())
}

/// Run ffmpeg reporting progress 0..1 over `duration` seconds.
fn run_ffmpeg(args: &[String], duration: f64, progress: &dyn Fn(f64)) -> Result<()> {
    let mut child = Command::new(ffmpeg_exe())
        .args(["-hide_banner", "-v", "error", "-y", "-nostats", "-progress", "pipe:1"])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("starting ffmpeg")?;
    let stdout = child.stdout.take().unwrap();
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok()) {
            if duration > 0.0 {
                progress((us / 1e6 / duration).clamp(0.0, 1.0));
            }
        }
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    progress(1.0);
    Ok(())
}

fn source_kbps(src: &Path) -> f64 {
    let size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0) as f64;
    let dur = hindsight_encode::probe_duration(src).unwrap_or(1.0).max(0.1);
    size * 8.0 / dur / 1000.0
}

/// Lossless trim: cuts on keyframes, instant. Exact: frame-accurate re-encode on the GPU.
pub fn trim(src: &Path, start: f64, end: f64, exact: bool, recording_encoder: &str, progress: &dyn Fn(f64)) -> Result<PathBuf> {
    let dst = sibling(src, "trim");
    let dur = (end - start).max(0.1);
    let mut args: Vec<String> = Vec::new();
    if exact {
        let kbps = source_kbps(src) * 1.1;
        args.extend(["-hwaccel".into(), "auto".into()]);
        args.extend(["-ss".into(), format!("{start:.3}"), "-i".into(), src.to_string_lossy().into()]);
        args.extend(["-t".into(), format!("{dur:.3}"), "-map".into(), "0".into()]);
        args.extend(["-c:v".into(), same_family_encoder(recording_encoder, src)]);
        args.extend(["-b:v".into(), format!("{kbps:.0}k"), "-maxrate".into(), format!("{:.0}k", kbps * 1.5), "-bufsize".into(), format!("{:.0}k", kbps * 2.0)]);
        args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "192k".into()]);
    } else {
        args.extend(["-ss".into(), format!("{start:.3}"), "-i".into(), src.to_string_lossy().into()]);
        args.extend(["-t".into(), format!("{dur:.3}"), "-map".into(), "0".into(), "-c".into(), "copy".into()]);
        args.extend(["-avoid_negative_ts".into(), "make_zero".into()]);
    }
    args.extend(["-movflags".into(), "+faststart".into(), dst.to_string_lossy().into()]);
    run_ffmpeg(&args, dur, progress)?;
    Ok(dst)
}

/// Re-encode to H.264 + AAC (plays everywhere) so the file fits under `target_mb`.
pub fn export_for_size(src: &Path, start: f64, end: f64, target_mb: f64, recording_encoder: &str, progress: &dyn Fn(f64)) -> Result<PathBuf> {
    let dur = (end - start).max(0.1);
    let audio_kbps = 128.0;
    // 6% container/rate-control headroom.
    let video_kbps = ((target_mb * 8_000.0 * 0.94) / dur - audio_kbps).max(150.0);
    let height = if video_kbps < 1_500.0 {
        480
    } else if video_kbps < 3_500.0 {
        720
    } else if video_kbps < 9_000.0 {
        1080
    } else {
        1440
    };
    let dst = sibling(src, &format!("{}MB", target_mb.round() as u32));
    let args: Vec<String> = vec![
        "-hwaccel".into(), "auto".into(),
        "-ss".into(), format!("{start:.3}"), "-i".into(), src.to_string_lossy().into(),
        "-t".into(), format!("{dur:.3}"),
        "-map".into(), "0:v:0".into(), "-map".into(), "0:a:0?".into(),
        "-vf".into(), format!("scale=-2:'min({height},ih)',fps='min(60,source_fps)'"),
        "-c:v".into(), h264_encoder(recording_encoder).into(),
        "-b:v".into(), format!("{video_kbps:.0}k"), "-maxrate".into(), format!("{video_kbps:.0}k"), "-bufsize".into(), format!("{:.0}k", video_kbps),
        "-pix_fmt".into(), "yuv420p".into(),
        "-c:a".into(), "aac".into(), "-b:a".into(), format!("{audio_kbps:.0}k"),
        "-movflags".into(), "+faststart".into(),
        dst.to_string_lossy().into(),
    ];
    run_ffmpeg(&args, dur, progress)?;
    Ok(dst)
}

/// The 360p all-keyframe copy the player scrubs through. The recorder writes
/// one with every clip; for older clips and trims it's built here in the
/// background (low priority) and `on_ready` fires when done.
pub fn scrub_proxy(clip: &Path, recording_encoder: &str, on_ready: impl FnOnce(PathBuf, PathBuf) + Send + 'static) -> Option<PathBuf> {
    let proxy = hindsight_recorder::scrub_path(clip);
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if proxy.exists() && modified(&proxy) >= modified(clip) {
        return Some(proxy);
    }
    let (clip, enc) = (clip.to_path_buf(), h264_encoder(recording_encoder));
    std::thread::spawn(move || {
        static BUSY: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one_at_a_time = BUSY.lock().unwrap();
        let _ = std::fs::create_dir_all(proxy.parent().unwrap());
        let tmp = proxy.with_extension("tmp.mp4");
        let mut args: Vec<String> = vec!["-v".into(), "error".into(), "-y".into(), "-hwaccel".into(), "auto".into(), "-i".into(), clip.to_string_lossy().into()];
        args.extend(["-an", "-vf", "fps=30,scale=-2:360", "-c:v", enc, "-g", "1", "-bf", "0"].map(String::from));
        if enc == "h264_nvenc" {
            args.extend(["-preset", "p1", "-rc", "constqp", "-qp", "30"].map(String::from));
        } else {
            args.extend(["-b:v", "6M"].map(String::from));
        }
        args.extend(["-movflags".into(), "+faststart".into(), tmp.to_string_lossy().into()]);
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x4000;
        let ok = Command::new(ffmpeg_exe())
            .args(&args)
            .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && std::fs::rename(&tmp, &proxy).is_ok() {
            on_ready(clip, proxy);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    });
    None
}

pub fn delete(clip: &Path) -> Result<()> {
    trash::delete(clip)?;
    let _ = std::fs::remove_file(hindsight_recorder::scrub_path(clip));
    Ok(())
}

pub fn rename(src: &Path, new_name: &str) -> Result<PathBuf> {
    let clean: String = new_name.chars().filter(|c| !"<>:\"/\\|?*".contains(*c) && !c.is_control()).collect();
    let clean = clean.trim();
    if clean.is_empty() {
        bail!("name is empty");
    }
    let dst = src.with_file_name(format!("{clean}.mp4"));
    if dst.exists() {
        bail!("a clip with that name already exists");
    }
    std::fs::rename(src, &dst)?;
    let _ = std::fs::rename(hindsight_recorder::scrub_path(src), hindsight_recorder::scrub_path(&dst));
    Ok(dst)
}
