//! Dynamic leveling for speech: a slow gain rider that lifts quiet passages
//! and holds back loud ones toward the program loudness, before the final
//! gain and the true-peak limiter.
//!
//! It is deliberately gentle. The detector is a plain 400 ms mean-square, the
//! correction is only a fraction of the distance to the reference, it never
//! lifts anything near the noise floor, and it raises the level slowly but
//! lowers it quickly. The overall loudness still lands on the target because
//! the pipeline calibrates the final gain after leveling.

use super::db_to_linear;

/// Window of the level detector.
const DETECTOR_SECONDS: f64 = 0.4;
/// How fast the gain goes down when the audio gets louder than the reference.
const ATTACK_SECONDS: f64 = 0.15;
/// How fast the gain comes back up when the audio gets quieter.
const RELEASE_SECONDS: f64 = 1.5;
/// Most the rider will lift or cut, in dB.
const MAX_BOOST_DB: f64 = 9.0;
const MAX_CUT_DB: f64 = 6.0;
/// Fraction of the distance to the reference that is corrected.
const STRENGTH: f64 = 0.6;
/// Audio this far below the reference is silence or noise: the gain is held,
/// never raised, so the noise floor is not amplified.
const GATE_BELOW_DB: f64 = 30.0;
/// The gain is recomputed every this many frames (about 0.7 ms at 48 kHz).
const UPDATE_FRAMES: usize = 32;
/// BS.1770 loudness of a mono sine reads 0.691 LU below its mean-square level.
const LUFS_TO_MEAN_SQUARE_DB: f64 = 0.691;

pub struct Leveler {
    channels: usize,
    /// Level the audio is pulled toward, as a mean-square level in dB.
    reference_db: f64,
    detector_coef: f64,
    attack_coef: f64,
    release_coef: f64,
    energy: f64,
    gain_db: f64,
    gain: f32,
    countdown: usize,
}

impl Leveler {
    /// `reference_lufs` is the integrated loudness of the whole program.
    pub fn new(channels: usize, sample_rate: u32, reference_lufs: f64) -> Self {
        let rate = f64::from(sample_rate);
        let per_update = |seconds: f64| 1.0 - (-(UPDATE_FRAMES as f64) / (seconds * rate)).exp();
        Self {
            channels,
            reference_db: reference_lufs + LUFS_TO_MEAN_SQUARE_DB,
            detector_coef: 1.0 - (-1.0 / (DETECTOR_SECONDS * rate)).exp(),
            attack_coef: per_update(ATTACK_SECONDS),
            release_coef: per_update(RELEASE_SECONDS),
            energy: 0.0,
            gain_db: 0.0,
            gain: 1.0,
            countdown: 0,
        }
    }

    /// Levels interleaved `block` in place.
    pub fn process(&mut self, block: &mut [f32]) {
        for frame in block.chunks_exact_mut(self.channels) {
            let mean_square =
                frame.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>() / self.channels as f64;
            self.energy += self.detector_coef * (mean_square - self.energy);
            if self.countdown == 0 {
                self.update();
                self.countdown = UPDATE_FRAMES;
            }
            self.countdown -= 1;
            for sample in frame {
                *sample *= self.gain;
            }
        }
    }

    fn update(&mut self) {
        let level_db = 10.0 * (self.energy + 1e-12).log10();
        let desired = if level_db < self.reference_db - GATE_BELOW_DB {
            self.gain_db
        } else {
            (STRENGTH * (self.reference_db - level_db)).clamp(-MAX_CUT_DB, MAX_BOOST_DB)
        };
        let coef = if desired < self.gain_db { self.attack_coef } else { self.release_coef };
        self.gain_db += coef * (desired - self.gain_db);
        self.gain = db_to_linear(self.gain_db) as f32;
    }

    /// Current gain in dB (for tests and diagnostics).
    #[cfg(test)]
    pub fn gain_db(&self) -> f64 {
        self.gain_db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::testsig;

    const RATE: u32 = testsig::RATE;

    /// Loudness of a mono sine of the given amplitude.
    fn lufs_of(amplitude: f32) -> f64 {
        10.0 * (f64::from(amplitude).powi(2) / 2.0).log10() - LUFS_TO_MEAN_SQUARE_DB
    }

    fn run(leveler: &mut Leveler, signal: &mut [f32]) {
        for block in signal.chunks_mut(8192) {
            leveler.process(block);
        }
    }

    #[test]
    fn audio_at_the_reference_is_left_alone() {
        let mut leveler = Leveler::new(1, RATE, lufs_of(0.25));
        let mut signal = testsig::tone(10.0, 1000.0, 0.25);

        run(&mut leveler, &mut signal);

        assert!(leveler.gain_db().abs() < 0.3, "gain {}", leveler.gain_db());
    }

    #[test]
    fn quiet_passages_are_lifted_and_loud_ones_held_back() {
        let mut leveler = Leveler::new(1, RATE, lufs_of(0.25));
        let mut quiet = testsig::tone(12.0, 1000.0, 0.25 / 3.16); // 10 dB under the reference
        run(&mut leveler, &mut quiet);
        let lifted = leveler.gain_db();

        let mut leveler = Leveler::new(1, RATE, lufs_of(0.25));
        let mut loud = testsig::tone(12.0, 1000.0, 0.25 * 3.16); // 10 dB over
        run(&mut leveler, &mut loud);
        let cut = leveler.gain_db();

        // Strength 0.6 of 10 dB, once settled.
        assert!((lifted - 6.0).abs() < 0.8, "lifted by {lifted} dB");
        assert!((cut + 6.0).abs() < 0.8, "cut by {cut} dB");
    }

    #[test]
    fn the_noise_floor_is_never_amplified() {
        let mut leveler = Leveler::new(1, RATE, lufs_of(0.25));
        let mut floor = testsig::pink_noise(10.0, 0.0005, 3); // far under the gate
        run(&mut leveler, &mut floor);

        assert!(leveler.gain_db() <= 0.01, "gain {} dB on the noise floor", leveler.gain_db());
    }

    #[test]
    fn the_boost_is_capped() {
        let mut leveler = Leveler::new(1, RATE, lufs_of(0.5));
        let mut signal = testsig::tone(20.0, 1000.0, 0.5 * 0.05); // 26 dB under, but above the gate
        run(&mut leveler, &mut signal);

        assert!(leveler.gain_db() <= MAX_BOOST_DB + 0.01);
    }
}
