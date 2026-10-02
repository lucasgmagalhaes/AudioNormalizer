//! Synthetic test signals generated in Rust, so tests need neither the
//! `ffmpeg` executable nor any committed media file.

use std::fs;
use std::path::Path;

pub const RATE: u32 = 48_000;

/// Writes interleaved samples as a 32-bit float WAV file.
pub fn write_wav(path: &Path, channels: u16, samples: &[f32]) {
    let data_len = (samples.len() * 4) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * u32::from(channels) * 4).to_le_bytes());
    wav.extend_from_slice(&(channels * 4).to_le_bytes());
    wav.extend_from_slice(&32u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(path, wav).unwrap();
}

fn frames(seconds: f64) -> usize {
    (seconds * f64::from(RATE)) as usize
}

fn sine(hz: f64, t: f64) -> f32 {
    (std::f64::consts::TAU * hz * t).sin() as f32
}

/// A steady sine.
pub fn tone(seconds: f64, hz: f64, amplitude: f32) -> Vec<f32> {
    (0..frames(seconds)).map(|i| amplitude * sine(hz, i as f64 / f64::from(RATE))).collect()
}

/// Deterministic pink noise (Paul Kellet's filter over xorshift white noise).
pub fn pink_noise(seconds: f64, amplitude: f32, seed: u32) -> Vec<f32> {
    let mut state = seed.max(1);
    let mut b = [0.0f32; 7];
    (0..frames(seconds))
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let white = (state as f32 / u32::MAX as f32) * 2.0 - 1.0;
            b[0] = 0.99886 * b[0] + white * 0.055_517_9;
            b[1] = 0.99332 * b[1] + white * 0.075_075_9;
            b[2] = 0.96900 * b[2] + white * 0.153_852;
            b[3] = 0.86650 * b[3] + white * 0.310_485_6;
            b[4] = 0.55000 * b[4] + white * 0.532_952_2;
            b[5] = -0.7616 * b[5] - white * 0.016_898;
            let pink = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + white * 0.5362;
            b[6] = white * 0.115_926;
            (pink * 0.11 * amplitude).clamp(-1.0, 1.0)
        })
        .collect()
}

/// Six-second cycle: two loud seconds, two quiet ones, two of silence. It
/// exercises the loudness gates and gives a wide loudness range.
pub fn bursts(seconds: f64) -> Vec<f32> {
    (0..frames(seconds))
        .map(|i| {
            let t = i as f64 / f64::from(RATE);
            match t % 6.0 {
                phase if phase < 2.0 => 0.5 * sine(330.0, t),
                phase if phase < 4.0 => 0.04 * sine(180.0, t),
                _ => 0.0,
            }
        })
        .collect()
}

/// Interleaves two mono signals into one stereo signal.
pub fn stereo(left: &[f32], right: &[f32]) -> Vec<f32> {
    left.iter().zip(right).flat_map(|(l, r)| [*l, *r]).collect()
}
