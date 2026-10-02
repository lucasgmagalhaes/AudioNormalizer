<p align="center">
  <img src="logo.svg" alt="Audio Normalizer logo" width="150" />
  <br />
  <a href="https://github.com/lucasgmagalhaes/AudioNormalizer/actions/workflows/ci.yml"><img src="https://github.com/lucasgmagalhaes/AudioNormalizer/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="https://codecov.io/gh/lucasgmagalhaes/AudioNormalizer"><img src="https://codecov.io/gh/lucasgmagalhaes/AudioNormalizer/graph/badge.svg" alt="codecov" /></a>
</p>

Audio Normalizer is a desktop application that brings videos to a consistent loudness target using EBU R128 / ITU-R BS.1770 measurement. It is built for creators who want predictable playback volume across platforms without re-encoding their video.

## What it does

- Analyzes the first audio track for integrated loudness, loudness range, true peak, and sample peak.
- Applies gain and a look-ahead true-peak limiter with 4x oversampling.
- Re-encodes only the normalized audio track. Video, subtitles, attachments, chapters, metadata, and other audio tracks are stream-copied.
- Verifies the generated file before atomically replacing the original. On failure or cancellation, the original file is preserved.
- Supports targets from -14 to -24 LUFS (or a custom value from -40 to -5) and true-peak ceilings of -1, -1.5, or -2 dBTP.
- Saves the result as a copy next to the original (`name (normalized).ext`) instead of replacing it, if you prefer.
- Lets you pick the audio track to normalize, or normalize all of them. The other tracks are copied unchanged.
- Normalizes several files in a queue with the same settings.
- Optional speech leveling brings quiet and loud passages closer before the final gain; the target is still reached because the gain is calibrated afterwards.
- Optional audio clean-up, off by default: a 20 Hz high-pass (DC offset and inaudible rumble) and declipping of flat-topped peaks. It only changes the audio samples: the video is never touched and the duration stays exactly the same. A stereo phase check warns when the channels cancel out in mono, without changing anything.
- Optional FLAC output avoids a second lossy generation when the container accepts it (for example MKV).
- Custom window frame with a menu bar (File: import, show result in folder, exit; Settings: language, update check at startup; Help: check for updates, report a problem, source code, About) instead of the operating system title bar.
- Reports whether the result met the target (within EBU R128 tolerance of 0.5 LU) and the peak ceiling, plus the loudest momentary and short-term readings, and can copy the report as text.

## How it works

1. **Analyze** decodes the first audio track and shows the required gain, expected limiter reduction, and potential improvement. Compressed audio packets are cached in memory, up to 512 MiB, so normalization can avoid reading the video again.
2. **Normalize** applies gain and limiting, then encodes and muxes the replacement audio track in a bounded threaded pipeline.
3. **Verify** measures the encoded audio and checks duration and video presence before replacing the source file.

When limiting is expected to exceed 1 dB, the engine calibrates gain in a decode-only pass. If the native AAC encoder exceeds the peak ceiling, it retries once with `aac_coder=fast`.

## Architecture

- **Desktop UI:** Tauri 2, TypeScript, and Vite (`src/`)
- **Audio engine:** Rust (`backend/src/engine/`)
- **Media bridge:** C bindings to libavformat, libavcodec, and libswresample (`backend/native/avbridge.c`)

The application does not start an `ffmpeg` process. FFmpeg libraries are linked directly through the native bridge.

## Requirements

- Node.js 26 (the version CI and releases use; 24 also works). Tests import TypeScript directly, so 22.18 or newer is the floor
- Stable Rust with the MSVC toolchain on Windows
- Visual Studio Build Tools with a C compiler
- A shared FFmpeg development build containing `include/`, `lib/`, and `bin/`

Install the shared FFmpeg package on Windows:

```powershell
winget install BtbN.FFmpeg.LGPL.Shared.8.1
```

Set `FFMPEG_DIR` to the installed shared-build directory:

```powershell
$env:FFMPEG_DIR = "C:\path\to\ffmpeg-shared"
```

`build.rs` compiles the bridge, links FFmpeg, and stages the runtime DLLs for development and packaging.

## Verification

The loudness measurement is checked against FFmpeg's own `ebur128` filter, run in-process through `backend/native/avbridge_ref.c` (libavfilter), on tones, noise, stereo and gated signals. Tests generate their media in Rust or through the bridge, so they never start an `ffmpeg` process. That reference code is a separate object in the static library: nothing in the app calls it, so the application does not load `avfilter`.

## Development

```bash
npm install
npm run app:dev
```

Build an installer:

```bash
npm run app:build
```

Run the engine tests:

```bash
npm run test:core
```

## End-to-end testing

The end-to-end test modifies files in place. Always use disposable copies:

```powershell
$env:NORMALIZER_E2E_FILES = "C:/tmp/video-a.mp4;C:/tmp/video-b.mkv"
cargo test --manifest-path backend/Cargo.toml e2e -- --ignored --nocapture
```

## Project layout

```text
src/                     TypeScript UI and styles
backend/
  native/avbridge.{h,c}  FFmpeg bridge: probe, decode, remux, and encode
  src/engine/
    analyze.rs           EBU R128 measurement and assessment
    limiter.rs           Look-ahead true-peak limiter
    normalize.rs         Normalization pipeline and atomic replacement
    job.rs               Progress reporting and cancellation
    bench.rs             Ignored real-media benchmarks using BENCH_FILE
  src/lib.rs             Tauri commands
```
