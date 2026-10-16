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
}
