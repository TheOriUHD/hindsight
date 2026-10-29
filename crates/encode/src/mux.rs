//! Remux already-encoded packets into an MP4 — no re-encoding, so writing a clip
//! costs only disk I/O.

use std::ffi::CString;
use std::path::Path;
use std::ptr;

use anyhow::{bail, Result};
use ffmpeg_sys_next as ff;

use crate::{check, Encoder, Packet, TIME_BASE};

pub struct Mp4Writer {
    oc: *mut ff::AVFormatContext,
    stream: *mut ff::AVStream,
    pkt: *mut ff::AVPacket,
    finished: bool,
}

impl Mp4Writer {
    pub fn create(path: &Path, enc: &Encoder) -> Result<Self> {
        let cpath = CString::new(path.to_string_lossy().as_bytes())?;
        unsafe {
            let mut oc = ptr::null_mut();
            check(ff::avformat_alloc_output_context2(&mut oc, ptr::null(), c"mp4".as_ptr(), cpath.as_ptr()), "alloc mp4")?;
            let stream = ff::avformat_new_stream(oc, ptr::null());
            if stream.is_null() {
                bail!("avformat_new_stream failed");
            }
            check(ff::avcodec_parameters_from_context((*stream).codecpar, enc.raw_ctx()), "codec parameters")?;
            (*stream).time_base = TIME_BASE;
            (*stream).avg_frame_rate = ff::AVRational { num: enc.config().fps as i32, den: 1 };
            check(ff::avio_open(&mut (*oc).pb, cpath.as_ptr(), ff::AVIO_FLAG_WRITE), "avio_open")?;

            // Moov atom up front so clips start playing instantly in browsers/Discord.
            let mut opts = ptr::null_mut();
            ff::av_dict_set(&mut opts, c"movflags".as_ptr(), c"+faststart".as_ptr(), 0);
            let r = ff::avformat_write_header(oc, &mut opts);
            ff::av_dict_free(&mut opts);
            check(r, "avformat_write_header")?;
            Ok(Self { oc, stream, pkt: ff::av_packet_alloc(), finished: false })
        }
    }

    /// `base_us` is subtracted from timestamps so a clip cut from the middle of the
    /// replay buffer starts at zero.
    pub fn write(&mut self, p: &Packet, base_us: i64) -> Result<()> {
        unsafe {
            let pkt = &mut *self.pkt;
            check(ff::av_new_packet(pkt, p.data.len() as i32), "av_new_packet")?;
            ptr::copy_nonoverlapping(p.data.as_ptr(), pkt.data, p.data.len());
            pkt.pts = p.pts - base_us;
            pkt.dts = p.dts - base_us;
            pkt.flags = if p.keyframe { ff::AV_PKT_FLAG_KEY as i32 } else { 0 };
            pkt.stream_index = 0;
            ff::av_packet_rescale_ts(pkt, TIME_BASE, (*self.stream).time_base);
            check(ff::av_interleaved_write_frame(self.oc, pkt), "write packet")
        }
    }

    pub fn finish(mut self) -> Result<()> {
        self.finished = true;
        unsafe { check(ff::av_write_trailer(self.oc), "av_write_trailer") }
    }
}

impl Drop for Mp4Writer {
    fn drop(&mut self) {
        unsafe {
            if !self.finished {
                ff::av_write_trailer(self.oc);
            }
            ff::avio_closep(&mut (*self.oc).pb);
            ff::avformat_free_context(self.oc);
            ff::av_packet_free(&mut self.pkt);
        }
    }
}
