use std::ptr;

use anyhow::{bail, Result};
use ffmpeg_sys_next as ff;

use crate::{check, drain, Packet, StreamParams};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

/// AAC-LC encoder fed with interleaved stereo f32 at 48 kHz. Timestamps come
/// from the sample position, so the track stays sample-accurate.
pub struct AudioEncoder {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
    pending: Vec<f32>,
    /// Sample index of `pending[0]`.
    next_pts: i64,
    frame_size: usize,
    title: String,
}

unsafe impl Send for AudioEncoder {}

impl AudioEncoder {
    /// `start_sample` is the sample index (on the recorder clock) of the first sample pushed.
    pub fn new(bitrate: i64, start_sample: i64, title: &str) -> Result<Self> {
        unsafe {
            let codec = ff::avcodec_find_encoder(ff::AVCodecID::AV_CODEC_ID_AAC);
            if codec.is_null() {
                bail!("AAC encoder missing");
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            let c = &mut *ctx;
            c.sample_fmt = ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP;
            c.sample_rate = SAMPLE_RATE as i32;
            ff::av_channel_layout_default(&mut c.ch_layout, CHANNELS as i32);
            c.bit_rate = bitrate;
            c.time_base = ff::AVRational { num: 1, den: SAMPLE_RATE as i32 };
            c.flags |= ff::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            check(ff::avcodec_open2(ctx, codec, ptr::null_mut()), "aac open")?;

            let frame_size = c.frame_size.max(1024) as usize;
            let frame = ff::av_frame_alloc();
            (*frame).format = ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP as i32;
            (*frame).nb_samples = frame_size as i32;
            (*frame).sample_rate = SAMPLE_RATE as i32;
            ff::av_channel_layout_copy(&mut (*frame).ch_layout, &c.ch_layout);
            check(ff::av_frame_get_buffer(frame, 0), "audio frame buffer")?;

            Ok(Self {
                ctx,
                frame,
                pkt: ff::av_packet_alloc(),
                pending: Vec::with_capacity(frame_size * CHANNELS * 4),
                next_pts: start_sample,
                frame_size,
                title: title.into(),
            })
        }
    }

    pub fn params(&self) -> Result<StreamParams> {
        unsafe { StreamParams::from_ctx(self.ctx, &self.title) }
    }

    /// Push interleaved stereo samples.
    pub fn push(&mut self, samples: &[f32], out: &mut dyn FnMut(Packet)) -> Result<()> {
        self.pending.extend_from_slice(samples);
        let n = self.frame_size;
        let mut off = 0;
        while self.pending.len() - off >= n * CHANNELS {
            unsafe {
                check(ff::av_frame_make_writable(self.frame), "audio frame writable")?;
                let l = std::slice::from_raw_parts_mut((*self.frame).data[0] as *mut f32, n);
                let r = std::slice::from_raw_parts_mut((*self.frame).data[1] as *mut f32, n);
                for i in 0..n {
                    l[i] = self.pending[off + i * 2];
                    r[i] = self.pending[off + i * 2 + 1];
                }
                (*self.frame).pts = self.next_pts;
                check(ff::avcodec_send_frame(self.ctx, self.frame), "audio send")?;
                drain(self.ctx, self.pkt, out)?;
            }
            self.next_pts += n as i64;
            off += n * CHANNELS;
        }
        self.pending.drain(..off);
        Ok(())
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        unsafe {
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.pkt);
        }
    }
}
