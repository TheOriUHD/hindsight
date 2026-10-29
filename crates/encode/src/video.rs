use std::ffi::{c_void, CString};
use std::ptr;

use anyhow::{bail, Context, Result};
use ffmpeg_sys_next as ff;
use hindsight_capture::GpuDevice;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_BIND_RENDER_TARGET};

use crate::{check, drain, ffi, Converter, Packet, StreamParams};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Av1,
    Hevc,
    H264,
}

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: Codec,
    /// Constant-quality target (lower = better). ~19-23 is visually transparent for games.
    pub cq: u32,
    /// Peak bitrate cap so fast motion can't blow up the buffer.
    pub max_kbps: u32,
}

/// Constant-frame-rate hardware encoder. Frames are addressed by slot number
/// (pts in units of 1/fps); gaps are filled by re-sending the last frame.
pub struct VideoEncoder {
    ctx: *mut ff::AVCodecContext,
    frames_ref: *mut ff::AVBufferRef,
    device_ref: *mut ff::AVBufferRef,
    frame: *mut ff::AVFrame,
    last: *mut ff::AVFrame,
    has_last: bool,
    pkt: *mut ff::AVPacket,
    converter: Option<Converter>,
    gpu: GpuDevice,
    cfg: EncoderConfig,
    pub encoder_name: &'static str,
}

// All FFmpeg/D3D state is only touched by whichever thread owns the encoder.
unsafe impl Send for VideoEncoder {}

fn encoder_name(vendor_id: u32, codec: Codec) -> &'static str {
    match (vendor_id, codec) {
        (0x10DE, Codec::Av1) => "av1_nvenc",
        (0x10DE, Codec::Hevc) => "hevc_nvenc",
        (0x10DE, Codec::H264) => "h264_nvenc",
        (0x1002, Codec::Av1) => "av1_amf",
        (0x1002, Codec::Hevc) => "hevc_amf",
        (0x1002, Codec::H264) => "h264_amf",
        // Intel and anything else: Media Foundation hardware MFTs.
        (_, Codec::Av1) => "av1_mf",
        (_, Codec::Hevc) => "hevc_mf",
        (_, Codec::H264) => "h264_mf",
    }
}

impl VideoEncoder {
    /// Try codecs best-first and return the first that opens on this GPU.
    pub fn open_best(gpu: &GpuDevice, cfg: &EncoderConfig, prefs: &[Codec]) -> Result<Self> {
        let mut errors = Vec::new();
        for &codec in prefs {
            match Self::new(gpu, EncoderConfig { codec, ..cfg.clone() }) {
                Ok(e) => return Ok(e),
                Err(e) => errors.push(format!("{codec:?}: {e:#}")),
            }
        }
        bail!("no hardware encoder could be opened: {}", errors.join("; "))
    }

    pub fn new(gpu: &GpuDevice, cfg: EncoderConfig) -> Result<Self> {
        let name = encoder_name(gpu.vendor_id, cfg.codec);
        unsafe {
            let mut enc = Self {
                ctx: ptr::null_mut(),
                frames_ref: ptr::null_mut(),
                device_ref: ptr::null_mut(),
                frame: ff::av_frame_alloc(),
                last: ff::av_frame_alloc(),
                has_last: false,
                pkt: ff::av_packet_alloc(),
                converter: None,
                gpu: gpu.clone(),
                cfg: cfg.clone(),
                encoder_name: name,
            };

            // Hand our D3D11 device to FFmpeg so its frames live on the same GPU
            // as the capture textures.
            enc.device_ref = ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            let devctx = (*enc.device_ref).data as *mut ff::AVHWDeviceContext;
            let d3d = (*devctx).hwctx as *mut ffi::AVD3D11VADeviceContext;
            (*d3d).device = gpu.device.clone().into_raw();
            check(ff::av_hwdevice_ctx_init(enc.device_ref), "av_hwdevice_ctx_init")?;

            enc.frames_ref = ff::av_hwframe_ctx_alloc(enc.device_ref);
            let fc = (*enc.frames_ref).data as *mut ff::AVHWFramesContext;
            (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_D3D11;
            (*fc).sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
            (*fc).width = cfg.width as i32;
            (*fc).height = cfg.height as i32;
            (*fc).initial_pool_size = 0; // pooled individual textures, recycled
            let fh = (*fc).hwctx as *mut ffi::AVD3D11VAFramesContext;
            (*fh).BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32; // video processor output
            check(ff::av_hwframe_ctx_init(enc.frames_ref), "av_hwframe_ctx_init")?;

            let cname = CString::new(name)?;
            let codec = ff::avcodec_find_encoder_by_name(cname.as_ptr());
            if codec.is_null() {
                bail!("{name} not available in this FFmpeg build");
            }
            enc.ctx = ff::avcodec_alloc_context3(codec);
            let c = &mut *enc.ctx;
            c.width = cfg.width as i32;
            c.height = cfg.height as i32;
            c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_D3D11;
            c.sw_pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_NV12;
            c.time_base = ff::AVRational { num: 1, den: cfg.fps as i32 };
            c.framerate = ff::AVRational { num: cfg.fps as i32, den: 1 };
            c.gop_size = cfg.fps as i32; // 1 s keyframes => precise, instant clip cuts
            c.max_b_frames = 2;
            c.bit_rate = 0;
            c.rc_max_rate = cfg.max_kbps as i64 * 1000;
            c.rc_buffer_size = (cfg.max_kbps as i64 * 1000 * 2) as i32;
            c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            c.flags |= ff::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            c.hw_frames_ctx = ff::av_buffer_ref(enc.frames_ref);

            let opts: Vec<(&str, String)> = if name.ends_with("_nvenc") {
                vec![
                    ("preset", "p5".into()),
                    ("tune", "hq".into()),
                    ("rc", "vbr".into()),
                    ("cq", cfg.cq.to_string()),
                    ("spatial-aq", "1".into()),
                    ("b_ref_mode", "middle".into()),
                    ("forced-idr", "1".into()),
                ]
            } else if name.ends_with("_amf") {
                vec![
                    ("quality", "quality".into()),
                    ("rc", "qvbr".into()),
                    ("qvbr_quality_level", cfg.cq.to_string()),
                ]
            } else {
                vec![("hw_encoding", "1".into()), ("rate_control", "quality".into())]
            };
            for (k, v) in &opts {
                let (k, v) = (CString::new(*k)?, CString::new(v.as_str())?);
                ff::av_opt_set(c.priv_data, k.as_ptr(), v.as_ptr(), 0);
            }

            check(ff::avcodec_open2(enc.ctx, codec, ptr::null_mut()), name)?;
            Ok(enc)
        }
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    pub fn params(&self) -> Result<StreamParams> {
        unsafe { StreamParams::from_ctx(self.ctx, "Video") }
    }

    pub fn has_frame(&self) -> bool {
        self.has_last
    }

    /// Convert + encode one captured texture as frame `slot`.
    pub fn encode(&mut self, src: &ID3D11Texture2D, src_w: u32, src_h: u32, slot: i64, out: &mut dyn FnMut(Packet)) -> Result<()> {
        unsafe {
            if self.converter.as_ref().map(|c| c.input_size()) != Some((src_w, src_h)) {
                self.converter = Some(Converter::new(
                    &self.gpu.device, &self.gpu.context, src_w, src_h, self.cfg.width, self.cfg.height, self.cfg.fps,
                )?);
            }
            check(ff::av_hwframe_get_buffer(self.frames_ref, self.frame, 0), "av_hwframe_get_buffer")?;
            let raw = (*self.frame).data[0] as *mut c_void;
            let index = (*self.frame).data[1] as usize as u32;
            let dst = ID3D11Texture2D::from_raw_borrowed(&raw).context("null hw frame texture")?;
            self.converter.as_mut().unwrap().convert(src, dst, index)?;

            ff::av_frame_unref(self.last);
            ff::av_frame_ref(self.last, self.frame);
            self.has_last = true;
            self.send(slot, out)
        }
    }

    /// Re-send the previous frame as frame `slot` (screen didn't change).
    pub fn repeat(&mut self, slot: i64, out: &mut dyn FnMut(Packet)) -> Result<()> {
        if !self.has_last {
            return Ok(());
        }
        unsafe {
            ff::av_frame_ref(self.frame, self.last);
            self.send(slot, out)
        }
    }

    unsafe fn send(&mut self, slot: i64, out: &mut dyn FnMut(Packet)) -> Result<()> {
        (*self.frame).pts = slot;
        let r = ff::avcodec_send_frame(self.ctx, self.frame);
        ff::av_frame_unref(self.frame);
        check(r, "avcodec_send_frame")?;
        drain(self.ctx, self.pkt, out)
    }

    pub fn flush(&mut self, out: &mut dyn FnMut(Packet)) -> Result<()> {
        unsafe {
            ff::avcodec_send_frame(self.ctx, ptr::null());
            drain(self.ctx, self.pkt, out)
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        unsafe {
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.last);
            ff::av_packet_free(&mut self.pkt);
            ff::av_buffer_unref(&mut self.frames_ref);
            ff::av_buffer_unref(&mut self.device_ref);
        }
    }
}
