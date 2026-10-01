//! Loudness analysis and normalization engine (EBU R128 / ITU-R BS.1770).
//!
//! libav* (through the C bridge in `av`) only decodes and muxes; loudness
//! measurement, gain and peak limiting all happen here on raw PCM.

pub mod analyze;
pub mod av;
mod bench;
pub mod job;
mod limiter;
pub mod normalize;

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
