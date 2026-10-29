# Hindsight — a no-nonsense game clipper (plan)

## Context
Medal / Outplayed are bloated: subscriptions, accounts, social feeds, ads, Electron UIs running while you game, and mediocre encode quality. Goal: a Windows clipping tool that does one thing perfectly — **always-on replay buffer → hotkey → clip on disk** — with near-zero game impact (target: 4K120 recording with <2% avg FPS loss), excellent quality, simple UI, and "just works" on NVIDIA, AMD and Intel. Directory is empty; this is a greenfield project.

Decisions made: **Rust + Tauri**, NVIDIA first but all vendors supported, **v1 = core clipper** (replay buffer, hotkey save, audio tracks, settings, clip library with trim/export, no cloud).

## Name
**Hindsight** (top pick — the whole product is "save what just happened"). Alternatives: *Backtrack*, *Afterglow*, *Snapback*, *Keyframe*. Do a quick trademark / store-name check before committing.

## Core performance principle: the frame never leaves the GPU
```
Game → DWM → WGC (D3D11 texture) → GPU shader: scale + BGRA→NV12/P010 (+HDR tonemap)
     → hardware encoder (NVENC / AMF / QSV, dedicated ASIC) → compressed packets (CPU, tiny)
     → ring buffer → [hotkey] → remux to MP4, no re-encode → instant clip
```
- No CPU readback of frames ever. No per-frame allocations (pre-allocated texture pool).
- Encoding runs on the GPU's dedicated media engine, not the shader cores the game uses — this is why 4K120 is feasible.
- **No injection / game hooks** → anti-cheat safe (Vanguard, EAC, BattlEye). WGC handles borderless and Win11 flip-model fullscreen.
- While gaming, only the Rust tray process runs. The Tauri/WebView2 UI is a **separate process launched on demand** and fully exits when closed — zero webview cost in-game.

## Architecture (Cargo workspace)
| Crate | Responsibility | Key tech |
|---|---|---|
| `capture` | Monitor/window capture, frame pacing | `windows-rs`: Windows.Graphics.Capture; DXGI Desktop Duplication fallback |
| `gpu` | Shared D3D11 device, scaling, color convert, HDR→SDR tonemap | HLSL compute shaders or `ID3D11VideoProcessor` |
| `encode` | Encoder probing + encoding from D3D11 surfaces | FFmpeg libavcodec (LGPL build) via `rsmpeg`/`ffmpeg-sys-next`, `d3d11va` hw frames → `*_nvenc`, `*_amf`, `*_qsv`; Media Foundation software H.264 last-resort fallback |
| `audio` | System loopback, mic, **per-process game-only loopback** | WASAPI, `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` (Win10 2004+), resample to 48 kHz, AAC |
| `replay` | Packet ring buffer, clip extraction, MP4 muxing | RAM ring (default) or disk-segmented ring for long buffers; keyframe-aligned cut; libavformat remux |
| `hotkeys` | Global hotkeys incl. mouse side buttons | Raw Input (`RIDEV_INPUTSINK`) — works in fullscreen, no hooks into games |
| `core` | Pipeline orchestration, settings (TOML), clock, IPC, tray | single QPC clock for A/V sync; named-pipe IPC to UI; `tracing` logs |
| `app/` | Tauri UI: settings, library, player, trim/export | Svelte or Solid (tiny bundle), SQLite (`rusqlite`) clip index |

### Threads & scheduling
- Capture thread (MMCSS "Capture"), encode thread, audio thread (MMCSS "Pro Audio"), mux/IO thread; lock-free bounded channels between them.
- Raise GPU scheduling priority for the recorder (as OBS does) so capture isn't starved under 100% GPU load → no dropped frames in clips.
- Frame pacing: WGC only delivers on change → duplicate last frame to produce **CFR** output (editors and Discord hate VFR).

### Encoder "just works" logic
1. On startup, probe: try opening AV1 → HEVC → H.264 on the detected vendor's encoder with the chosen resolution/fps.
2. Pick the best that works (AV1 on RTX 40+/RX 7000+/Arc; HEVC otherwise; H.264 if user wants max compatibility).
3. On any encoder failure at runtime: fall back down the chain automatically, notify via tray, never crash.

### Quality defaults (presets map to these)
- Rate control: **constant-quality VBR with max-bitrate cap** (NVENC `-rc vbr -cq N -maxrate`), preset p5, tune hq, B-frames on, spatial AQ on, GOP = 1 s (fast, precise clip cuts).
- Presets: *Balanced* / *High* (default) / *Max*. Rough caps @ High: 1080p60 ≈ 40 Mbps, 1440p120 ≈ 70 Mbps, 4K120 ≈ 120 Mbps (HEVC/AV1).
- Buffer memory math shown live in settings (e.g. 4K120 @120 Mbps × 60 s ≈ 900 MB) → auto-suggest disk ring when large.
- HDR monitors: capture FP16, tonemap to SDR on GPU in v1 (clips look correct everywhere); HDR10 output is v2.

## Clip save flow (must feel instant)
Hotkey → snapshot ring range [now−N s, now] → seek back to previous keyframe → remux packets to MP4 with `+faststart` on IO thread (no re-encode, <1 s even for 4K) → play a short confirmation sound (overlays don't render over exclusive fullscreen without injection) → write SQLite row + thumbnail lazily.

## v1 features
- **Tray app**: recording on/off, save clip, open library, quit. Auto-start with Windows (optional).
- **Settings**: display/window source, resolution (native or GPU-downscale), fps (30/60/120/144/native), codec (Auto/AV1/HEVC/H.264), quality preset, buffer length, RAM vs disk buffer, audio tracks (game/system/mic, each on separate MP4 track + optional mixed track), hotkeys, output folder, filename pattern.
- **Library**: grid of clips with thumbnails, sorted by date, filter by game (from foreground process name at save time), rename/delete.
- **Player + trim**: HTML5 video in WebView2; lossless keyframe trim (instant) or frame-accurate trim (hardware re-encode).
- **Export presets**: "Fit under X MB" (Discord etc.) → computes bitrate, hardware re-encodes to H.264 for max compatibility.

## Milestones
- **M0 – spike (prove the hot path)**: WGC → GPU NV12 → NVENC HEVC → MP4 file. Measure game FPS impact with PresentMon at 1440p/4K 120. Go/no-go on architecture.
- **M1 – clipper core**: ring buffer, hotkey, keyframe-aligned remux, WASAPI audio + A/V sync, tray. Usable headless.
- **M2 – just works**: encoder probing + fallback chain, AMF & QSV paths (via FFmpeg), DXGI fallback, HDR tonemap, device-lost / resolution-change / monitor-hotplug recovery.
- **M3 – UI**: Tauri settings + library + player + trim/export, IPC with core.
- **M4 – ship**: NSIS/MSI installer via Tauri bundler, bundled LGPL FFmpeg DLLs, crash logs (local only, no telemetry), auto-update optional (GitHub releases).

## Verification
- **Game impact**: PresentMon runs with recorder off vs on (1080p60 / 1440p144 / 4K120) in a GPU-bound and a CPU-bound game. Targets: avg FPS −2% max, 1% lows −3% max.
- **Recorder health**: internal counters (dropped/duplicated frames, encode latency, buffer fill) exposed in a debug panel; NVML for encoder utilisation.
- **Quality**: VMAF of captured clips vs lossless reference capture per preset.
- **Correctness**: A/V sync drift test over a 30-min session; clips open in VLC, Premiere/DaVinci, Discord, browser; save-clip latency < 1 s at 4K.
- **Robustness**: alt-tab, fullscreen toggles, resolution change, GPU driver reset (TDR), unplugging a monitor, audio device change — recorder must recover without restart.
- **Vendors**: NVIDIA locally; AMD/Intel via testers or CI-less manual matrix before release.
