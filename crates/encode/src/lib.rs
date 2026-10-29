//! Hardware video + AAC audio encoding via FFmpeg, and MP4 remuxing.
//!
//! Video: capture texture -> [Converter: GPU video processor, BGRA->NV12 + scale]
//! -> FFmpeg D3D11 hw frame -> NVENC/AMF -> compressed packets (CPU).

mod audio;
mod convert;
mod ffi;
mod mux;
mod video;

use std::sync::Arc;

use anyhow::{bail, Result};
use ffmpeg_sys_next as ff;

pub use audio::{AudioEncoder, CHANNELS, SAMPLE_RATE};
pub use convert::Converter;
pub use mux::{probe_duration, Mp4Writer};
pub use video::{Codec, EncoderConfig, FrameSource, HwFrame, VideoEncoder};

/// Microseconds; the common time base for packets in the replay buffer.
pub const US: ff::AVRational = ff::AVRational { num: 1, den: 1_000_000 };

/// Encoded packet, owned on the CPU. Timestamps are in microseconds on the
/// recorder's shared clock, so video and audio packets line up.
#[derive(Clone)]
pub struct Packet {
    pub data: Arc<[u8]>,
    pub pts: i64,
    pub dts: i64,
    pub duration: i64,
    pub keyframe: bool,
    pub stream: usize,
}

/// Codec parameters captured when an encoder opens, so clips can be muxed
/// from the replay buffer without the encoder.
pub struct StreamParams {
    par: *mut ff::AVCodecParameters,
    pub time_base: ff::AVRational,
    pub title: String,
}

unsafe impl Send for StreamParams {}
unsafe impl Sync for StreamParams {}

impl StreamParams {
    pub(crate) unsafe fn from_ctx(ctx: *const ff::AVCodecContext, title: &str) -> Result<Self> {
        let par = ff::avcodec_parameters_alloc();
        check(ff::avcodec_parameters_from_context(par, ctx), "codec parameters")?;
        Ok(Self { par, time_base: (*ctx).time_base, title: title.into() })
    }
}

impl Drop for StreamParams {
    fn drop(&mut self) {
        unsafe { ff::avcodec_parameters_free(&mut self.par) }
    }
}

pub(crate) unsafe fn drain(ctx: *mut ff::AVCodecContext, pkt: *mut ff::AVPacket, out: &mut dyn FnMut(Packet)) -> Result<()> {
    loop {
        let r = ff::avcodec_receive_packet(ctx, pkt);
        if r == ff::AVERROR(ff::EAGAIN) || r == ff::AVERROR_EOF {
            return Ok(());
        }
        check(r, "avcodec_receive_packet")?;
        let p = &*pkt;
        let tb = (*ctx).time_base;
        out(Packet {
            data: Arc::from(std::slice::from_raw_parts(p.data, p.size as usize)),
            pts: ff::av_rescale_q(p.pts, tb, US),
            dts: ff::av_rescale_q(p.dts, tb, US),
            duration: ff::av_rescale_q(p.duration, tb, US),
            keyframe: p.flags & ff::AV_PKT_FLAG_KEY as i32 != 0,
            stream: 0,
        });
        ff::av_packet_unref(pkt);
    }
}

pub(crate) fn check(r: i32, what: &str) -> Result<()> {
    if r >= 0 {
        return Ok(());
    }
    let mut buf = [0 as std::ffi::c_char; 256];
    unsafe { ff::av_strerror(r, buf.as_mut_ptr(), buf.len()) };
    let msg = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    bail!("{what}: {msg} ({r})")
}

/// Silence FFmpeg's stderr chatter except real errors.
pub fn quiet_logs() {
    unsafe { ff::av_log_set_level(ff::AV_LOG_ERROR) }
}
