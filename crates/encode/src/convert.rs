//! BGRA -> NV12 (BT.709 limited range) with optional scaling, done by the GPU's
//! fixed-function video processor. Runs on the media engine, not the shader
//! cores the game is using.

use std::collections::HashMap;
use std::mem::ManuallyDrop;

use anyhow::Result;
use windows::core::Interface;
use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_RATIONAL,
};

pub struct Converter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    in_w: u32,
    in_h: u32,
    // Views are cached per texture; the texture is kept alive alongside its view
    // so a recycled pointer can never alias a stale view.
    inputs: HashMap<usize, (ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    outputs: HashMap<(usize, u32), (ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
}

impl Converter {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Self> {
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast()?;
            let vctx: ID3D11VideoContext1 = context.cast()?;
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let vp = vdev.CreateVideoProcessor(&enumerator, 0)?;

            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            vctx.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            vctx.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            let src = RECT { left: 0, top: 0, right: in_w as i32, bottom: in_h as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&src));

            Ok(Self { vdev, vctx, enumerator, vp, in_w, in_h, inputs: HashMap::new(), outputs: HashMap::new() })
        }
    }

    pub fn input_size(&self) -> (u32, u32) {
        (self.in_w, self.in_h)
    }

    /// Convert `src` into array slice `dst_index` of `dst`.
    pub fn convert(&mut self, src: &ID3D11Texture2D, dst: &ID3D11Texture2D, dst_index: u32) -> Result<()> {
        unsafe {
            let in_view = self.input_view(src)?;
            let out_view = self.output_view(dst, dst_index)?;
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: TRUE,
                pInputSurface: ManuallyDrop::new(Some(in_view)),
                ..Default::default()
            };
            let streams = [stream];
            let r = self.vctx.VideoProcessorBlt(&self.vp, &out_view, 0, &streams);
            let [mut stream] = streams;
            ManuallyDrop::drop(&mut stream.pInputSurface);
            r?;
        }
        Ok(())
    }

    unsafe fn input_view(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let key = src.as_raw() as usize;
        if let Some((_, v)) = self.inputs.get(&key) {
            return Ok(v.clone());
        }
        if self.inputs.len() > 16 {
            self.inputs.clear();
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut view = None;
        self.vdev.CreateVideoProcessorInputView(src, &self.enumerator, &desc, Some(&mut view))?;
        let view = view.unwrap();
        self.inputs.insert(key, (src.clone(), view.clone()));
        Ok(view)
    }

    unsafe fn output_view(&mut self, dst: &ID3D11Texture2D, index: u32) -> Result<ID3D11VideoProcessorOutputView> {
        let key = (dst.as_raw() as usize, index);
        if let Some((_, v)) = self.outputs.get(&key) {
            return Ok(v.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_VPOV { MipSlice: 0, FirstArraySlice: index, ArraySize: 1 },
            },
        };
        let mut view = None;
        self.vdev.CreateVideoProcessorOutputView(dst, &self.enumerator, &desc, Some(&mut view))?;
        let view = view.unwrap();
        self.outputs.insert(key, (dst.clone(), view.clone()));
        Ok(view)
    }
}
