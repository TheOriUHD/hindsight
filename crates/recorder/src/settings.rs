use std::path::PathBuf;

use anyhow::Result;
use hindsight_encode::Codec;
use serde::{Deserialize, Serialize};

use crate::audio::{AudioApp, SourceSpec};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesktopMode {
    /// Everything that plays on the output device.
    All,
    /// Only the apps in `audio_apps`.
    Apps,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodecChoice {
    Auto,
    Av1,
    Hevc,
    H264,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Balanced,
    High,
    Max,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Recording on/off (persists across restarts).
    pub enabled: bool,
    /// DXGI device name like `\\.\DISPLAY2`; None = primary monitor.
    pub monitor: Option<String>,
    pub fps: u32,
    /// Output height cap; 0 = native.
    pub max_height: u32,
    pub codec: CodecChoice,
    pub quality: Quality,
    /// Replay length = clip length.
    pub buffer_secs: u32,
    pub hotkey: String,
    pub desktop_audio: bool,
    pub microphone: bool,
    /// Output device for desktop audio; None = Windows default.
    pub output_device: Option<String>,
    /// Microphone; None = Windows default.
    pub input_device: Option<String>,
    pub desktop_mode: DesktopMode,
    /// Apps recorded when `desktop_mode` is `Apps`.
    pub audio_apps: Vec<AudioApp>,
    /// With several sources, also keep each one on its own track.
    pub separate_tracks: bool,
    pub output_dir: PathBuf,
    pub start_with_windows: bool,
    pub save_sound: bool,
    /// Small "Clip saved" toast in the corner of the recorded screen.
    pub save_overlay: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            monitor: None,
            fps: 60,
            max_height: 0,
            codec: CodecChoice::Auto,
            quality: Quality::High,
            buffer_secs: 60,
            hotkey: "F8".into(),
            desktop_audio: true,
            microphone: false,
            output_device: None,
            input_device: None,
            desktop_mode: DesktopMode::All,
            audio_apps: Vec::new(),
            separate_tracks: true,
            output_dir: dirs::video_dir().unwrap_or_else(|| PathBuf::from(".")).join("Hindsight"),
            start_with_windows: true,
            save_sound: true,
            save_overlay: true,
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        data_dir().join("settings.json")
    }

    pub fn load() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
            .map(Settings::sanitized)
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(data_dir())?;
        std::fs::write(Self::path(), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn sanitized(mut self) -> Self {
        self.fps = self.fps.clamp(24, 360);
        self.buffer_secs = self.buffer_secs.clamp(5, 600);
        self
    }

    /// What to record audio from, in track order.
    pub fn audio_sources(&self) -> Vec<SourceSpec> {
        let mut out = Vec::new();
        if self.desktop_audio {
            if self.desktop_mode == DesktopMode::Apps && !self.audio_apps.is_empty() {
                out.extend(self.audio_apps.iter().map(|a| SourceSpec::App { exe: a.exe.clone(), name: a.name.clone() }));
            } else {
                out.push(SourceSpec::Output { device: self.output_device.clone() });
            }
        }
        if self.microphone {
            out.push(SourceSpec::Input { device: self.input_device.clone() });
        }
        out
    }

    pub fn codec_prefs(&self) -> Vec<Codec> {
        match self.codec {
            CodecChoice::Auto => vec![Codec::Av1, Codec::Hevc, Codec::H264],
            CodecChoice::Av1 => vec![Codec::Av1, Codec::Hevc, Codec::H264],
            CodecChoice::Hevc => vec![Codec::Hevc, Codec::H264],
            CodecChoice::H264 => vec![Codec::H264],
        }
    }

    pub fn cq(&self) -> u32 {
        match self.quality {
            Quality::Balanced => 28,
            Quality::High => 23,
            Quality::Max => 19,
        }
    }

    /// Peak bitrate cap in kbps for the given output.
    pub fn max_kbps(&self, w: u32, h: u32) -> u32 {
        let bits_per_pixel = match self.quality {
            Quality::Balanced => 0.06,
            Quality::High => 0.10,
            Quality::Max => 0.16,
        };
        let kbps = w as f64 * h as f64 * self.fps as f64 * bits_per_pixel / 1000.0;
        kbps.clamp(8_000.0, 300_000.0) as u32
    }

    /// Output size: native, or scaled down to `max_height` keeping aspect (even dims).
    pub fn output_size(&self, w: u32, h: u32) -> (u32, u32) {
        if self.max_height == 0 || h <= self.max_height {
            return (w & !1, h & !1);
        }
        let oh = self.max_height & !1;
        let ow = ((w as u64 * oh as u64 / h as u64) as u32) & !1;
        (ow, oh)
    }
}

/// Settings, logs and thumbnails: %LOCALAPPDATA%\app.hindsight.clips (the
/// installer owns %LOCALAPPDATA%\Hindsight, so user data stays out of it).
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("app.hindsight.clips")
}
