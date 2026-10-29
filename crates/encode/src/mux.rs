//! Remux already-encoded packets into an MP4 — no re-encoding, so writing a clip
//! costs only disk I/O.

use std::ffi::CString;
use std::path::Path;
use std::ptr;

use anyhow::{bail, Result};
use ffmpeg_sys_next as ff;

use crate::{check, Packet, StreamParams, US};

pub struct Mp4Writer {
    oc: *mut ff::AVFormatContext,
    streams: Vec<*mut ff::AVStream>,
    pkt: *mut ff::AVPacket,
    finished: bool,
}

impl Mp4Writer {
    pub fn create(path: &Path, params: &[&StreamParams]) -> Result<Self> {
        let cpath = CString::new(path.to_string_lossy().as_bytes())?;
        unsafe {
            let mut oc = ptr::null_mut();
            check(ff::avformat_alloc_output_context2(&mut oc, ptr::null(), c"mp4".as_ptr(), cpath.as_ptr()), "alloc mp4")?;
            let mut w = Self { oc, streams: Vec::new(), pkt: ff::av_packet_alloc(), finished: true };
            for p in params {
                let s = ff::avformat_new_stream(oc, ptr::null());
                if s.is_null() {
                    bail!("avformat_new_stream failed");
                }
                check(ff::avcodec_parameters_copy((*s).codecpar, p.par), "copy codec parameters")?;
                (*(*s).codecpar).codec_tag = 0;
                (*s).time_base = p.time_base;
                let title = CString::new(p.title.as_str())?;
                ff::av_dict_set(&mut (*s).metadata, c"title".as_ptr(), title.as_ptr(), 0);
                ff::av_dict_set(&mut (*s).metadata, c"handler_name".as_ptr(), title.as_ptr(), 0);
                w.streams.push(s);
            }
            check(ff::avio_open(&mut (*oc).pb, cpath.as_ptr(), ff::AVIO_FLAG_WRITE), "avio_open")?;

            // Moov atom up front so clips start playing instantly in browsers/Discord.
            let mut opts = ptr::null_mut();
            ff::av_dict_set(&mut opts, c"movflags".as_ptr(), c"+faststart".as_ptr(), 0);
            let r = ff::avformat_write_header(oc, &mut opts);
            ff::av_dict_free(&mut opts);
            check(r, "avformat_write_header")?;
            w.finished = false;
            Ok(w)
        }
    }

    /// `base_us` is subtracted from timestamps so a clip cut from the middle of the
    /// replay buffer starts at zero. `p.stream` indexes the `params` passed to `create`.
    pub fn write(&mut self, p: &Packet, base_us: i64) -> Result<()> {
        unsafe {
            let s = self.streams[p.stream];
            let pkt = &mut *self.pkt;
            check(ff::av_new_packet(pkt, p.data.len() as i32), "av_new_packet")?;
            ptr::copy_nonoverlapping(p.data.as_ptr(), pkt.data, p.data.len());
            pkt.pts = p.pts - base_us;
            pkt.dts = p.dts - base_us;
            pkt.duration = p.duration;
            pkt.flags = if p.keyframe { ff::AV_PKT_FLAG_KEY as i32 } else { 0 };
            pkt.stream_index = p.stream as i32;
            ff::av_packet_rescale_ts(pkt, US, (*s).time_base);
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

/// Container duration in seconds (reads only the header/moov).
pub fn probe_duration(path: &Path) -> Option<f64> {
    let cpath = CString::new(path.to_string_lossy().as_bytes()).ok()?;
    unsafe {
        let mut ic = ptr::null_mut();
        if ff::avformat_open_input(&mut ic, cpath.as_ptr(), ptr::null(), ptr::null_mut()) < 0 {
            return None;
        }
        // Without avformat_find_stream_info (slow) only per-stream durations from
        // the moov atom are known.
        let mut d = 0.0f64;
        for i in 0..(*ic).nb_streams as usize {
            let s = *(*ic).streams.add(i);
            if (*s).duration > 0 {
                let tb = (*s).time_base;
                d = d.max((*s).duration as f64 * tb.num as f64 / tb.den as f64);
            }
        }
        ff::avformat_close_input(&mut ic);
        (d > 0.0).then_some(d)
    }
}
