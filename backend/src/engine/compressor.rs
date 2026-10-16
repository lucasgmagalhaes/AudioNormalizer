//! Voice compressor: a fast, gentle feed-forward compressor that holds back
//! the short peaks (shouts, laughs, plosives) the slow leveler cannot catch.
//!
//! The threshold follows the program loudness, so it behaves the same on a
//! quiet recording and on a hot one. There is no make-up gain: the pipeline
//! calibrates the final gain afterwards, so the loudness still lands on the
//! target. The channels share one gain, so the stereo image does not move.

use super::{db_to_linear, linear_to_db};

/// Compression starts this far above the program loudness (in dB).
const THRESHOLD_ABOVE_PROGRAM_DB: f64 = 5.0;
/// Input/output ratio above the threshold.
const RATIO: f64 = 3.0;
/// Width of the soft knee around the threshold, in dB.
const KNEE_DB: f64 = 6.0;
/// How fast the gain comes down when a peak arrives.
const ATTACK_SECONDS: f64 = 0.004;
/// How fast the gain comes back after the peak.
const RELEASE_SECONDS: f64 = 0.12;

pub struct Compressor {
    channels: usize,
    threshold_db: f64,
    attack_coef: f64,
    release_coef: f64,
    /// Gain reduction currently applied, in dB (zero or positive).
    reduction_db: f64,
}

impl Compressor {
    /// `program_lufs` is the integrated loudness of the whole program.
    pub fn new(channels: usize, sample_rate: u32, program_lufs: f64) -> Self {
        let rate = f64::from(sample_rate);
        let coef = |seconds: f64| 1.0 - (-1.0 / (seconds * rate)).exp();
        Self {
            channels,
            threshold_db: program_lufs + THRESHOLD_ABOVE_PROGRAM_DB,
            attack_coef: coef(ATTACK_SECONDS),
            release_coef: coef(RELEASE_SECONDS),
            reduction_db: 0.0,
        }
    }

    /// Compresses interleaved `block` in place.
    pub fn process(&mut self, block: &mut [f32]) {
        for frame in block.chunks_exact_mut(self.channels) {
            let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let wanted = self.wanted_reduction_db(linear_to_db(f64::from(peak)));
            let coef = if wanted > self.reduction_db { self.attack_coef } else { self.release_coef };
            self.reduction_db += coef * (wanted - self.reduction_db);
            let gain = db_to_linear(-self.reduction_db) as f32;
            for sample in frame {
                *sample *= gain;
            }
        }
    }

    /// Static curve: how many dB to take off a sample at `level_db`.
    fn wanted_reduction_db(&self, level_db: f64) -> f64 {
        let over = level_db - self.threshold_db;
        let slope = 1.0 - 1.0 / RATIO;
        if over <= -KNEE_DB / 2.0 {
            0.0
        } else if over >= KNEE_DB / 2.0 {
            slope * over
        } else {
            slope * (over + KNEE_DB / 2.0).powi(2) / (2.0 * KNEE_DB)
        }
    }

    /// Gain reduction currently applied, in dB (for tests and diagnostics).
    #[cfg(test)]
    pub fn reduction_db(&self) -> f64 {
        self.reduction_db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::testsig;

    const RATE: u32 = testsig::RATE;

    /// Mean-square level in dB of a mono sine of the given amplitude, as the
    /// leveler and the meter see it (LUFS of a sine is 0.691 dB below it).
    fn lufs_of(amplitude: f32) -> f64 {
        10.0 * (f64::from(amplitude).powi(2) / 2.0).log10() - 0.691
    }

    fn peak(signal: &[f32]) -> f32 {
        signal.iter().fold(0.0, |m, s| m.max(s.abs()))
    }

    #[test]
    fn audio_under_the_threshold_is_left_alone() {
        // A sine peaks 3.7 dB over its loudness; the program here is louder.
        let mut compressor = Compressor::new(1, RATE, lufs_of(0.5));
        let mut signal = testsig::tone(2.0, 1000.0, 0.25);
        let original = signal.clone();

        compressor.process(&mut signal);

        assert_eq!(signal, original);
        assert_eq!(compressor.reduction_db(), 0.0);
    }

    #[test]
    fn loud_peaks_are_held_back() {
        let mut compressor = Compressor::new(1, RATE, lufs_of(0.1));
        // 20 dB over the program loudness, well above the threshold.
        let mut signal = testsig::tone(2.0, 1000.0, 0.9);

        compressor.process(&mut signal);

        let settled = &signal[signal.len() / 2..];
        assert!(peak(settled) < 0.9 * 0.7, "peak still {}", peak(settled));
        assert!(compressor.reduction_db() > 3.0, "reduction {} dB", compressor.reduction_db());
    }

    #[test]
    fn the_ratio_is_respected_once_settled() {
        let program = lufs_of(0.1);
        let mut compressor = Compressor::new(1, RATE, program);
        let mut signal = testsig::tone(3.0, 1000.0, 0.9);
        compressor.process(&mut signal);

        let over = linear_to_db(0.9) - (program + THRESHOLD_ABOVE_PROGRAM_DB);
        let expected = (1.0 - 1.0 / RATIO) * over;
        // The detector follows the rectified wave, so between its crests the
        // gain relaxes a little: the average sits just under the static curve.
        let reduction = compressor.reduction_db();
        assert!((expected - 1.0..=expected + 0.05).contains(&reduction), "{reduction} vs {expected}");
    }

    #[test]
    fn the_gain_comes_back_after_the_peak() {
        let mut compressor = Compressor::new(1, RATE, lufs_of(0.1));
        let mut loud = testsig::tone(1.0, 1000.0, 0.9);
        compressor.process(&mut loud);
        let mut quiet = testsig::tone(2.0, 1000.0, 0.05);
        compressor.process(&mut quiet);

        assert!(compressor.reduction_db() < 0.05, "still reducing {} dB", compressor.reduction_db());
    }

    #[test]
    fn the_channels_share_one_gain() {
        let mut compressor = Compressor::new(2, RATE, lufs_of(0.1));
        let mut signal: Vec<f32> = testsig::tone(1.0, 1000.0, 0.9).iter().flat_map(|s| [*s, s * 0.5]).collect();

        compressor.process(&mut signal);

        for frame in signal.chunks_exact(2).skip(RATE as usize / 2) {
            assert!((frame[0] * 0.5 - frame[1]).abs() < 1e-6, "image moved: {frame:?}");
        }
    }

    /// Runs `signal` through a fresh compressor in blocks of `block` frames.
    fn run(channels: usize, rate: u32, program: f64, signal: &[f32], block: usize) -> (Compressor, Vec<f32>) {
        let mut compressor = Compressor::new(channels, rate, program);
        let mut out = signal.to_vec();
        for chunk in out.chunks_mut(block * channels) {
            compressor.process(chunk);
        }
        (compressor, out)
    }

    #[test]
    fn silence_stays_silent_and_finite() {
        let (compressor, out) = run(2, RATE, -23.0, &vec![0.0; 2 * 4800], 480);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(compressor.reduction_db(), 0.0);
    }

    #[test]
    fn it_never_amplifies_anything() {
        let program = lufs_of(0.1);
        for amplitude in [0.001, 0.05, 0.1, 0.3, 0.7, 1.0, 4.0] {
            let signal = testsig::tone(1.0, 440.0, amplitude);
            let (_, out) = run(1, RATE, program, &signal, 1024);
            for (before, after) in signal.iter().zip(&out) {
                assert!(after.abs() <= before.abs() + 1e-6, "amplitude {amplitude}: {before} became {after}");
            }
        }
    }

    #[test]
    fn it_keeps_the_sign_and_the_length() {
        let signal = testsig::tone(1.0, 330.0, 0.9);
        let (_, out) = run(1, RATE, lufs_of(0.1), &signal, 777);
        assert_eq!(out.len(), signal.len());
        assert!(signal.iter().zip(&out).all(|(a, b)| a.signum() == b.signum() || *b == 0.0 || *a == 0.0));
    }

    #[test]
    fn the_block_size_does_not_change_the_result() {
        let signal = testsig::bursts(8.0);
        let (_, whole) = run(1, RATE, lufs_of(0.1), &signal, signal.len());
        for block in [1, 7, 480, 4096] {
            let (_, chunked) = run(1, RATE, lufs_of(0.1), &signal, block);
            assert_eq!(whole, chunked, "block of {block} frames");
        }
    }

    #[test]
    fn a_louder_program_is_compressed_less() {
        let signal = testsig::tone(2.0, 1000.0, 0.9);
        let mut previous = f64::MAX;
        for program in [-40.0, -30.0, -20.0, -10.0, 0.0] {
            let (compressor, _) = run(1, RATE, program, &signal, 1024);
            assert!(compressor.reduction_db() <= previous + 1e-9, "program {program} LUFS");
            previous = compressor.reduction_db();
        }
        assert_eq!(previous, 0.0, "a program far louder than the signal leaves it alone");
    }

    #[test]
    fn the_curve_is_zero_below_the_knee_and_linear_above_it() {
        let compressor = Compressor::new(1, RATE, -20.0);
        let threshold = -20.0 + THRESHOLD_ABOVE_PROGRAM_DB;
        assert_eq!(compressor.wanted_reduction_db(threshold - KNEE_DB / 2.0 - 0.01), 0.0);
        assert_eq!(compressor.wanted_reduction_db(-90.0), 0.0);
        let slope = 1.0 - 1.0 / RATIO;
        for over in [KNEE_DB / 2.0 + 0.5, 10.0, 25.0] {
            let reduction = compressor.wanted_reduction_db(threshold + over);
            assert!((reduction - slope * over).abs() < 1e-9, "{over} dB over: {reduction}");
        }
    }

    #[test]
    fn the_curve_is_continuous_and_never_decreases() {
        let compressor = Compressor::new(1, RATE, -20.0);
        let mut previous = 0.0;
        let mut level = -80.0;
        while level < 6.0 {
            let reduction = compressor.wanted_reduction_db(level);
            assert!(reduction >= previous - 1e-12, "dropped at {level} dB");
            assert!(reduction - previous < 0.01, "jump at {level} dB: {previous} -> {reduction}");
            previous = reduction;
            level += 0.01;
        }
    }

    #[test]
    fn the_attack_catches_a_sudden_peak_within_milliseconds() {
        let mut compressor = Compressor::new(1, RATE, lufs_of(0.1));
        let mut quiet = testsig::tone(0.5, 1000.0, 0.05);
        compressor.process(&mut quiet);
        assert!(compressor.reduction_db() < 0.01);

        let mut shout = testsig::tone(0.05, 1000.0, 0.9);
        let wanted = compressor.wanted_reduction_db(linear_to_db(0.9));
        compressor.process(&mut shout[..(RATE as usize * 20) / 1000]);
        assert!(compressor.reduction_db() > wanted * 0.7, "{} of {wanted} dB after 20 ms", compressor.reduction_db());
    }

    #[test]
    fn the_release_is_gradual_not_a_jump() {
        let mut compressor = Compressor::new(1, RATE, lufs_of(0.1));
        compressor.process(&mut testsig::tone(1.0, 1000.0, 0.9));
        let held = compressor.reduction_db();
        assert!(held > 3.0);

        compressor.process(&mut testsig::tone(0.01, 1000.0, 0.02));
        let after_10ms = compressor.reduction_db();
        assert!(after_10ms > held * 0.8, "released too fast: {held} -> {after_10ms}");
        compressor.process(&mut testsig::tone(1.0, 1000.0, 0.02));
        assert!(compressor.reduction_db() < 0.05);
    }

    #[test]
    fn the_timing_follows_the_sample_rate() {
        let seconds = 0.02;
        let mut reductions = Vec::new();
        for rate in [22_050, 44_100, 48_000, 96_000] {
            let mut compressor = Compressor::new(1, rate, -30.0);
            let frames = (f64::from(rate) * seconds) as usize;
            let mut shout: Vec<f32> =
                (0..frames).map(|i| 0.9 * (std::f64::consts::TAU * 1000.0 * i as f64 / f64::from(rate)).sin() as f32).collect();
            compressor.process(&mut shout);
            reductions.push(compressor.reduction_db());
        }
        let (min, max) = reductions.iter().fold((f64::MAX, f64::MIN), |(lo, hi), r| (lo.min(*r), hi.max(*r)));
        assert!(max - min < 1.0, "same time, different reduction by rate: {reductions:?}");
    }

    #[test]
    fn many_channels_share_one_gain() {
        for channels in [1, 2, 3, 6, 8] {
            let mono = testsig::tone(1.0, 700.0, 0.9);
            let signal: Vec<f32> =
                mono.iter().flat_map(|s| (0..channels).map(move |c| s * (1.0 - 0.1 * c as f32))).collect();
            let (_, out) = run(channels, RATE, lufs_of(0.1), &signal, 512);
            for (before, after) in signal.chunks_exact(channels).zip(out.chunks_exact(channels)).skip(RATE as usize / 2) {
                if before[0].abs() < 0.2 {
                    continue;
                }
                let ratio = after[0] / before[0];
                for c in 1..channels {
                    assert!((after[c] / before[c] - ratio).abs() < 1e-5, "{channels} channels: channel {c} drifted");
                }
            }
        }
    }

    #[test]
    fn the_loudest_channel_drives_the_gain() {
        // Loud on the right only: the quiet left channel is turned down with it.
        let right = testsig::tone(1.0, 500.0, 0.9);
        let left = testsig::tone(1.0, 500.0, 0.02);
        let (compressor, out) = run(2, RATE, lufs_of(0.1), &testsig::stereo(&left, &right), 512);
        assert!(compressor.reduction_db() > 3.0);
        let last = out.len() - 2 * 100;
        let quiet_in = left[left.len() - 100..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let quiet_out = out[last..].chunks_exact(2).fold(0.0f32, |m, f| m.max(f[0].abs()));
        assert!(quiet_out < quiet_in, "left was not linked to the right");
    }

    #[test]
    fn extreme_input_stays_finite() {
        let mut loud = vec![1.0e6, -1.0e6, 0.0, 3.0e38, -3.0e38, f32::MIN_POSITIVE];
        let mut compressor = Compressor::new(1, RATE, -23.0);
        compressor.process(&mut loud);
        assert!(loud.iter().all(|s| s.is_finite()), "{loud:?}");
        assert!(compressor.reduction_db().is_finite() && compressor.reduction_db() >= 0.0);
    }

    #[test]
    fn real_speech_like_material_gets_a_smaller_peak_to_loudness_ratio() {
        // Loud bursts over a quiet bed: the peaks come down, the bed does not move.
        let signal = testsig::bursts(12.0);
        let (_, out) = run(1, RATE, lufs_of(0.2), &signal, 1024);
        let window = |s: &[f32], from: f64, to: f64| {
            let (a, b) = ((from * f64::from(RATE)) as usize, (to * f64::from(RATE)) as usize);
            s[a..b].iter().fold(0.0f32, |m, v| m.max(v.abs()))
        };
        assert!(window(&out, 1.0, 2.0) < window(&signal, 1.0, 2.0) * 0.9, "loud burst was not held back");
        let quiet_in = window(&signal, 2.5, 3.5);
        let quiet_out = window(&out, 8.5, 9.5);
        assert!((quiet_out - quiet_in).abs() < quiet_in * 0.05, "the quiet bed moved: {quiet_in} -> {quiet_out}");
    }
}
