# Hindsight

Instant-replay clipping for Windows. Always records the last N seconds on the
GPU; press a hotkey and they're saved as an MP4. No account, no cloud, no ads.

## Install

Run `dist/Hindsight_0.1.0_x64-setup.exe` (per-user install, no admin). Hindsight
starts in the tray and opens its window. Press **F8** in any game to save a clip
to `Videos\Hindsight`.

## How it works

```
Game → DWM → Windows.Graphics.Capture (GPU texture)
     → GPU video processor: BGRA → NV12, scaling
     → NVENC / AMF / Media Foundation hardware encoder (AV1 → HEVC → H.264)
     → compressed packets in a RAM ring buffer
     → hotkey → remux to MP4 (no re-encode)
```

- Frames never leave the GPU; encoding runs on the GPU's media engine.
- No injection or game hooks, so it's safe with anti-cheat (Vanguard, EAC, BattlEye).
- Two processes from one exe: the recorder/tray (no webview) and the UI
  (`--ui`, started on demand, fully exits when closed).
- Constant frame rate output; audio and video share one QPC clock.
- With mic enabled, clips get three audio tracks: Game + Mic (mixed), Desktop, Microphone.
- Desktop Duplication is used automatically where WGC can't run above 60 fps (pre-24H2 Windows).

## Files

| Path | What |
|---|---|
| `%LOCALAPPDATA%\Hindsight` | Installed app + bundled LGPL FFmpeg |
| `%LOCALAPPDATA%\app.hindsight.clips` | `settings.json`, `hindsight.log`, `hindsight-ui.log`, thumbnails |
| `Videos\Hindsight` | Clips (configurable) |

## Build

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup.ps1   # fetch vendored FFmpeg + libclang
npm install
npx tauri build                                              # -> target\release\bundle\nsis\*.exe
```

Crates: `crates/capture` (WGC + DXGI), `crates/encode` (FFmpeg hw encode, AAC, MP4 mux),
`crates/recorder` (pipeline, audio, replay buffer, hotkeys), `app` (Tauri tray + UI), `ui` (plain HTML/JS).

Tuning: `HINDSIGHT_ENC_OPTS="key=value,..."` passes extra private options to the encoder.

## Known gaps (v0.1)

- Desktop Duplication fallback doesn't draw the mouse cursor.
- Intel GPUs use Media Foundation encoders, not native QSV.
- HDR displays are captured as SDR (no tone mapping yet).
- NVIDIA's driver reserves ~0.5 GB of process memory per 4K encoder session.
- Only RAM-backed replay buffer (no disk ring for very long buffers).
