# Third-party notices

## FFmpeg

Hindsight ships unmodified FFmpeg 8.1 shared libraries (`avcodec`, `avformat`,
`avutil`, `avfilter`, `avdevice`, `swscale`, `swresample`) and `ffmpeg.exe`,
built with the LGPL configuration (no GPL or non-free components).

- License: GNU Lesser General Public License v2.1 or later (see `FFMPEG-LICENSE.txt`)
- Build: https://github.com/BtbN/FFmpeg-Builds (ffmpeg-n8.1-latest-win64-lgpl-shared-8.1)
- Source: https://ffmpeg.org/download.html and https://github.com/FFmpeg/FFmpeg/tree/release/8.1

The libraries are dynamically linked and can be replaced with compatible builds.

## Rust crates

Hindsight is built on Tauri, windows-rs, ffmpeg-sys-next, serde, anyhow, chrono,
dirs, log, trash and winreg, each under MIT and/or Apache-2.0 (ffmpeg-sys-next: WTFPL).
