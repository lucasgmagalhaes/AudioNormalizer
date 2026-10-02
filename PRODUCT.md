# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Video creators who publish to platforms such as YouTube, Instagram and TikTok and want predictable playback volume without re-encoding their video. They open the app with a finished video file and want it at the right loudness quickly.

## Product Purpose

Audio Normalizer is a Windows desktop app that brings a video's audio to a consistent loudness target using EBU R128 / ITU-R BS.1770 measurement. Success is a file whose volume is on target, whose peaks stay under the chosen ceiling, and whose video, subtitles and other tracks are untouched.

## Positioning

Only the normalized audio track is re-encoded; video, subtitles, attachments, chapters, metadata and other audio tracks are stream-copied. FFmpeg libraries are linked directly, with no `ffmpeg` process. The generated file is verified before it atomically replaces the original, and the original is preserved on failure or cancellation.

## Operating Context

Desktop app built with Tauri 2, TypeScript and Vite. The UI renders in a webview and the audio engine is Rust. Flow: pick or drop a video, optionally Analyze (shows required gain, expected limiter reduction and potential improvement), then Normalize, which replaces the source file after an explicit confirmation. Auto-update is offered at startup from signed GitHub Releases.

## Capabilities and Constraints

- Loudness targets from -14 to -24 LUFS; true-peak ceilings of -1, -1.5 or -2 dBTP.
- Normalization replaces the original file and cannot be undone, so the destructive step stays behind an explicit confirmation.
- The interface ships in three languages (pt-BR, en, es) through Fluent; every screen must work in all three.
- Platform today is Windows only. Cross-platform support is undecided.

## Brand Commitments

Keep the product name "Audio Normalizer" and the current logo (`logo.svg`). No other visual commitments were made binding.

## Evidence on Hand

Real behavior and numbers come from the engine's measurements. No testimonials, customers, benchmarks or pricing exist; future work must not invent them.

## Product Principles

- Never risk the user's original file: confirm before replacing, verify before swapping, preserve on failure.
- Show measured facts (LUFS, true peak, gain, limiter action), never estimates presented as results.
- Make the next step obvious: select, analyze, normalize.
- Every string works in all three languages.

## Accessibility & Inclusion

No formal standard was set. Language coverage (pt-BR, en, es) is the confirmed inclusion requirement.
