//! Draws the mouse cursor into Desktop Duplication frames, which don't include
//! it. The frame is copied into a texture of ours and the cursor blended on top
//! with a tiny shader, all on the GPU. Only runs while a cursor is visible, so
//! games that hide theirs pay nothing.

use std::ffi::c_void;

use anyhow::{bail, Context, Result};
use windows::core::{s, Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

const SHADER: &str = r#"
cbuffer Rect : register(b0) { float4 rect; };
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
V vs(uint id : SV_VertexID) {
    float2 uv = float2(id & 1, id >> 1);
    V o;
    o.pos = float4(rect.xy + uv * rect.zw, 0, 1);
    o.uv = uv;
    return o;
}
Texture2D tex : register(t0);
SamplerState smp : register(s0);
float4 ps(V i) : SV_Target { return tex.Sample(smp, i.uv); }
"#;

unsafe fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    let r = D3DCompile(SHADER.as_ptr() as *const c_void, SHADER.len(), None, None, None, entry, target, 0, 0, &mut code, Some(&mut errors));
    if let Err(e) = r {
        let msg = errors
            .map(|b: ID3DBlob| String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).to_string())
            .unwrap_or_default();
        bail!("D3DCompile: {e} {msg}");
    }
    code.context("no shader code")
}

/// BGRA straight-alpha pixels from any of the three pointer shape formats.
fn to_bgra(info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, buf: &[u8]) -> (u32, u32, Vec<u8>) {
    let (w, pitch) = (info.Width as usize, info.Pitch as usize);
    let mono = info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
    // Monochrome shapes stack an AND mask on an XOR mask.
    let h = if mono { info.Height as usize / 2 } else { info.Height as usize };
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let px = &mut out[(y * w + x) * 4..(y * w + x) * 4 + 4];
            if mono {
                let bit = |row: usize| buf[row * pitch + x / 8] & (0x80 >> (x % 8)) != 0;
                match (bit(y), bit(y + h)) {
                    (true, false) => {}                                    // transparent
                    (false, false) => px.copy_from_slice(&[0, 0, 0, 255]), // black
                    // White, and "invert screen" (the text I-beam), which can't be
                    // blended; white reads on the dark UIs and games it's seen over.
                    _ => px.copy_from_slice(&[255, 255, 255, 255]),
                }
            } else {
                let s = &buf[y * pitch + x * 4..y * pitch + x * 4 + 4];
                px.copy_from_slice(s);
                if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
                    // Alpha is a mask here: 0 = replace, 0xFF = XOR with the screen.
                    let xor = s[3] != 0;
                    px[3] = if xor && s[..3] == [0, 0, 0] { 0 } else { 255 };
                } else if info.Type != DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
                    px[3] = 255;
                }
            }
        }
    }
    (w as u32, h as u32, out)
}

pub struct CursorCompositor {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    blend: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
    target: Option<(ID3D11Texture2D, ID3D11RenderTargetView, u32, u32)>,
    shape: Option<(ID3D11ShaderResourceView, u32, u32)>,
}

impl CursorCompositor {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Result<Self> {
        unsafe {
            let (vsb, psb) = (compile(s!("vs"), s!("vs_4_0"))?, compile(s!("ps"), s!("ps_4_0"))?);
            let bytes = |b: &ID3DBlob| std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec();
            let (mut vs, mut ps) = (None, None);
            device.CreateVertexShader(&bytes(&vsb), None, Some(&mut vs))?;
            device.CreatePixelShader(&bytes(&psb), None, Some(&mut ps))?;

            let mut bd = D3D11_BLEND_DESC::default();
            bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_SRC_ALPHA,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_ZERO,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            device.CreateBlendState(&bd, Some(&mut blend))?;

            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sd, Some(&mut sampler))?;

            let cd = D3D11_BUFFER_DESC {
                ByteWidth: 16,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let mut constants = None;
            device.CreateBuffer(&cd, None, Some(&mut constants))?;

            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                vs: vs.context("vertex shader")?,
                ps: ps.context("pixel shader")?,
                blend: blend.context("blend state")?,
                sampler: sampler.context("sampler")?,
                constants: constants.context("constant buffer")?,
                target: None,
                shape: None,
            })
        }
    }

    /// Upload a new pointer image from `GetFramePointerShape`.
    pub fn set_shape(&mut self, info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, buf: &[u8]) -> Result<()> {
        let (w, h, px) = to_bgra(info, buf);
        if w == 0 || h == 0 {
            self.shape = None;
            return Ok(());
        }
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: px.as_ptr() as *const c_void, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
            let mut tex = None;
            self.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex))?;
            let tex: ID3D11Texture2D = tex.context("cursor texture")?;
            let mut srv = None;
            self.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
            self.shape = Some((srv.context("cursor view")?, w, h));
        }
        Ok(())
    }

    pub fn has_shape(&self) -> bool {
        self.shape.is_some()
    }

    /// Copy `frame` and draw the cursor at (x, y); returns the composed texture.
    pub fn compose(&mut self, frame: &ID3D11Texture2D, w: u32, h: u32, x: i32, y: i32) -> Result<ID3D11Texture2D> {
        unsafe {
            if !matches!(&self.target, Some((_, _, tw, th)) if *tw == w && *th == h) {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    ..Default::default()
                };
                let mut tex = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
                let tex: ID3D11Texture2D = tex.context("compose texture")?;
                let mut rtv = None;
                self.device.CreateRenderTargetView(&tex, None, Some(&mut rtv))?;
                self.target = Some((tex, rtv.context("render target")?, w, h));
            }
            let (target, rtv, _, _) = self.target.as_ref().unwrap();
            let c = &self.context;
            c.CopyResource(target, frame);
            if let Some((srv, cw, ch)) = &self.shape {
                // Quad in clip space: origin plus size, y pointing down.
                let rect = [
                    x as f32 / w as f32 * 2.0 - 1.0,
                    1.0 - y as f32 / h as f32 * 2.0,
                    *cw as f32 / w as f32 * 2.0,
                    -(*ch as f32) / h as f32 * 2.0,
                ];
                c.UpdateSubresource(&self.constants, 0, None, rect.as_ptr() as *const c_void, 0, 0);
                let vp = D3D11_VIEWPORT { Width: w as f32, Height: h as f32, MaxDepth: 1.0, ..Default::default() };
                c.RSSetViewports(Some(&[vp]));
                c.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
                c.IASetInputLayout(None);
                c.VSSetShader(&self.vs, None);
                c.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
                c.PSSetShader(&self.ps, None);
                c.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
                c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
                c.OMSetBlendState(&self.blend, None, 0xffff_ffff);
                c.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
                c.Draw(4, 0);
                c.OMSetRenderTargets(None, None);
                c.PSSetShaderResources(0, Some(&[None]));
            }
            Ok(target.cast()?)
        }
    }
}
