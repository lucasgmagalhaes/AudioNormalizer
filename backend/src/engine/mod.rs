//! Loudness analysis and normalization engine (EBU R128 / ITU-R BS.1770).
//!
//! libav* (through the C bridge in `av`) only decodes and muxes; loudness
//! measurement, gain and peak limiting all happen here on raw PCM.

pub mod analyze;
pub mod av;
#[cfg(all(test, not(coverage)))]
mod bench;
pub mod job;
mod limiter;
pub mod normalize;
#[cfg(test)]
mod testsig;

use anyhow::{bail, Result};
use serde::Deserialize;

/// What the user wants the audio to end up at.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Targets {
    /// Integrated loudness target in LUFS.
    pub target_lufs: f64,
    /// True-peak ceiling in dBTP.
    pub true_peak_db: f64,
}

impl Targets {
    pub fn validate(&self) -> Result<()> {
        if !(-40.0..=-5.0).contains(&self.target_lufs) {
            bail!("alvo de loudness inválido: {} LUFS", self.target_lufs);
        }
        if !(-9.0..=0.0).contains(&self.true_peak_db) {
            bail!("teto de true peak inválido: {} dBTP", self.true_peak_db);
        }
        Ok(())
    }
}

pub(crate) fn db_to_linear(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

pub(crate) fn linear_to_db(linear: f64) -> f64 {
    20.0 * linear.max(1e-10).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_target_ranges() {
        assert!(Targets { target_lufs: -16.0, true_peak_db: -1.0 }.validate().is_ok());
        assert!(Targets { target_lufs: -41.0, true_peak_db: -1.0 }.validate().is_err());
        assert!(Targets { target_lufs: -16.0, true_peak_db: 0.1 }.validate().is_err());
    }

    #[test]
    fn converts_decibels_round_trip() {
        for db in [-60.0, -16.0, 0.0] {
            assert!((linear_to_db(db_to_linear(db)) - db).abs() < 1e-9);
        }
    }
}
