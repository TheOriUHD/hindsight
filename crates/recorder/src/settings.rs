use std::path::PathBuf;

use anyhow::Result;
use hindsight_encode::Codec;
use serde::{Deserialize, Serialize};

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
    pub output_dir: PathBuf,
    pub start_with_windows: bool,
    pub save_sound: bool,
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
            output_dir: dirs::video_dir().unwrap_or_else(|| PathBuf::from(".")).join("Hindsight"),
            start_with_windows: true,
            save_sound: true,
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
