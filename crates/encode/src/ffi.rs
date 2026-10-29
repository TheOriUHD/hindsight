//! Structs from libavutil/hwcontext_d3d11va.h, which ffmpeg-sys-next doesn't bind
//! (it would need the Windows SDK headers). Layout matches FFmpeg 8.x.

use std::ffi::c_void;

#[repr(C)]
pub struct AVD3D11VADeviceContext {
    pub device: *mut c_void,
    pub device_context: *mut c_void,
    pub video_device: *mut c_void,
    pub video_context: *mut c_void,
    pub lock: Option<unsafe extern "C" fn(*mut c_void)>,
    pub unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    pub lock_ctx: *mut c_void,
}

#[repr(C)]
pub struct AVD3D11FrameDescriptor {
    pub texture: *mut c_void,
    pub index: isize,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct AVD3D11VAFramesContext {
    pub texture: *mut c_void,
    pub BindFlags: u32,
    pub MiscFlags: u32,
    pub texture_infos: *mut AVD3D11FrameDescriptor,
}
