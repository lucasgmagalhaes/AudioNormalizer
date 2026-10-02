//! Pre-processing: measure the current loudness and estimate how much
//! normalization would improve it.

use anyhow::{bail, Context, Result};
use ebur128::{EbuR128, Mode};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread;

use super::av::{self, AudioCache, MediaInfo, PcmDecoder};
use super::job::{Job, Progress, StageStopped, QUEUE_BLOCKS};
use super::{linear_to_db, Targets};

/// Largest gain change we are willing to apply in either direction.
pub const MAX_GAIN_DB: f64 = 24.0;

/// Most compressed audio kept in memory to decode the track again without
/// reading the file (30 minutes of 192 kb/s AAC is about 43 MB).
pub const CACHE_LIMIT_BYTES: u64 = 512 << 20;

/// Below this the gated integrated loudness is not meaningful.
const SILENCE_LUFS: f64 = -70.0;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Measurement {
    pub integrated_lufs: f64,
    pub loudness_range: f64,
    pub true_peak_db: f64,
    pub sample_peak_db: f64,
    /// Highest short-term (3 s window) loudness, when it was measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_short_term_lufs: Option<f64>,
    /// Highest momentary (400 ms window) loudness, when it was measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_momentary_lufs: Option<f64>,
    /// Left/right correlation of a stereo track, -1 (opposite phase) to 1
    /// (identical). Only measured, never changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stereo_correlation: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaSummary {
    pub duration: f64,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub has_video: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    None,
    Small,
    Moderate,
    Large,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Assessment {
    pub target_lufs: f64,
    pub true_peak_ceiling_db: f64,
    /// Gain that normalization will apply.
    pub gain_db: f64,
    /// The ideal gain was larger than `MAX_GAIN_DB`.
    pub gain_capped: bool,
    pub expected_lufs: f64,
    /// Where the true peak would land after the gain, before limiting.
    pub peak_after_gain_db: f64,
    /// How hard the limiter will have to work on the loudest peak.
    pub limiter_reduction_db: f64,
    /// Current loudness minus target (negative = too quiet).
    pub deviation_lu: f64,
    pub peak_over_ceiling: bool,
    pub clipping: bool,
    pub improvement_percent: f64,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisReport {
    pub media: MediaSummary,
    pub measurement: Measurement,
    pub assessment: Assessment,
}

pub struct Analysis {
    pub report: AnalysisReport,
    /// The track's compressed audio, when it fit under [`CACHE_LIMIT_BYTES`].
    pub cache: Option<AudioCache>,
}

/// Analyzes the first audio track.
#[cfg(test)]
pub fn run(targets: Targets, input: &Path, job: &Job) -> Result<Analysis> {
    run_track(targets, 0, input, job)
}

/// Analyzes the `track`-th audio track (0 = first).
pub fn run_track(targets: Targets, track: usize, input: &Path, job: &Job) -> Result<Analysis> {
    let info = av::probe_track(input, track)?;
    let mut progress = job.stage("analyze", 0.0, 100.0);
    let mut decoder = PcmDecoder::open(input, &info, CACHE_LIMIT_BYTES)?;
    let measurement = measure(&mut decoder, &info, job, &mut progress)?;
    progress.update(1.0);
    let cache = decoder.take_cache();
    let report = AnalysisReport {
        media: MediaSummary {
            duration: info.duration,
            codec: info.audio.codec.clone(),
            sample_rate: info.audio.sample_rate,
            channels: info.audio.channels,
            has_video: info.has_video,
        },
        measurement,
        assessment: assess(&measurement, targets),
    };
    Ok(Analysis { report, cache })
}

/// Decode the first audio track and measure it. The decoder feeds, on
/// separate threads, one meter for loudness/LRA/sample peak and one
/// true-peak meter per channel (true peak is the expensive part).
pub fn measure(decoder: &mut PcmDecoder, info: &MediaInfo, job: &Job, progress: &mut Progress) -> Result<Measurement> {
    let channels = info.audio.channels;
    let rate = info.audio.sample_rate;
    let stride = channels as usize;
    let total_frames = info.duration * rate as f64;

    thread::scope(|s| {
        let mut senders = Vec::with_capacity(stride + 1);

        let (tx, rx) = sync_channel::<Arc<Vec<f32>>>(QUEUE_BLOCKS);
        senders.push(tx);
        let loudness = s.spawn(move || -> Result<LoudnessReading> {
            let mut meter = EbuR128::new(channels, rate, Mode::I | Mode::LRA | Mode::SAMPLE_PEAK | Mode::M | Mode::S)
                .context("falha ao iniciar o medidor de loudness")?;
            // Momentary and short-term loudness are read every 100 ms, the
            // update rate the EBU specification uses for them.
            let step = (rate as usize / 10).max(1) * stride;
            let (mut max_short_term, mut max_momentary) = (None, None);
            let mut phase = PhaseMeter::default();
            for block in rx {
                if stride == 2 {
                    phase.add(&block);
                }
                for chunk in block.chunks(step) {
                    meter.add_frames_f32(chunk).context("falha ao medir loudness")?;
                    max_short_term = higher(max_short_term, meter.loudness_shortterm().ok());
                    max_momentary = higher(max_momentary, meter.loudness_momentary().ok());
                }
            }
            let integrated = meter.loudness_global().context("falha ao calcular loudness")?;
            let sample_peak = (0..channels).map(|ch| meter.sample_peak(ch).unwrap_or(0.0)).fold(0.0, f64::max);
            Ok(LoudnessReading {
                integrated,
                range: meter.loudness_range().unwrap_or(0.0),
                sample_peak,
                max_short_term,
                max_momentary,
                stereo_correlation: phase.correlation(),
            })
        });

        let mut peak_meters = Vec::with_capacity(stride);
        for ch in 0..stride {
            let (tx, rx) = sync_channel::<Arc<Vec<f32>>>(QUEUE_BLOCKS);
            senders.push(tx);
            peak_meters.push(s.spawn(move || -> Result<f64> {
                let mut meter = EbuR128::new(1, rate, Mode::TRUE_PEAK)
                    .context("falha ao iniciar o medidor de pico")?;
                let mut mono = Vec::new();
                for block in rx {
                    mono.clear();
                    mono.extend(block.iter().skip(ch).step_by(stride));
                    meter.add_frames_f32(&mono).context("falha ao medir pico")?;
                }
                Ok(meter.true_peak(0).unwrap_or(0.0))
            }));
        }

        let fed = (|| -> Result<u64> {
            let mut frames = 0u64;
            let mut block = Vec::new();
            while decoder.read(&mut block)? {
                job.check_cancelled()?;
                frames += (block.len() / stride) as u64;
                let shared = Arc::new(std::mem::take(&mut block));
                for tx in &senders {
                    tx.send(shared.clone()).map_err(|_| anyhow::Error::new(StageStopped))?;
                }
                if total_frames > 0.0 {
                    progress.update(frames as f64 / total_frames);
                }
            }
            Ok(frames)
        })();
        drop(senders);
        let loudness = loudness.join().expect("meter thread panicked");
        let peaks: Vec<Result<f64>> = peak_meters
            .into_iter()
            .map(|m| m.join().expect("meter thread panicked"))
            .collect();

        let frames = match fed {
            Err(e) if !e.is::<StageStopped>() => return Err(e),
            fed => fed,
        };
        let LoudnessReading { integrated, range: loudness_range, sample_peak, max_short_term, max_momentary, stereo_correlation } =
            loudness?;
        let true_peak = peaks.into_iter().collect::<Result<Vec<f64>>>()?.into_iter().fold(0.0, f64::max);
        if frames? == 0 {
            bail!("a faixa de áudio está vazia");
        }
        if !integrated.is_finite() || integrated < SILENCE_LUFS {
            bail!("o áudio é silencioso demais para ser normalizado");
        }
        Ok(Measurement {
            integrated_lufs: integrated,
            loudness_range,
            true_peak_db: linear_to_db(true_peak),
            sample_peak_db: linear_to_db(sample_peak),
            max_short_term_lufs: max_short_term,
            max_momentary_lufs: max_momentary,
            stereo_correlation,
        })
    })
}

/// What the loudness meter thread hands back.
struct LoudnessReading {
    integrated: f64,
    range: f64,
    sample_peak: f64,
    max_short_term: Option<f64>,
    max_momentary: Option<f64>,
    stereo_correlation: Option<f64>,
}

/// Running left/right correlation of interleaved stereo audio.
#[derive(Default)]
struct PhaseMeter {
    cross: f64,
    left: f64,
    right: f64,
}

impl PhaseMeter {
    fn add(&mut self, block: &[f32]) {
        for frame in block.as_chunks::<2>().0 {
            let (l, r) = (f64::from(frame[0]), f64::from(frame[1]));
            self.cross += l * r;
            self.left += l * l;
            self.right += r * r;
        }
    }

    /// None when a channel is silent: the correlation is then meaningless.
    fn correlation(&self) -> Option<f64> {
        let norm = (self.left * self.right).sqrt();
        (norm > 1e-9).then(|| (self.cross / norm).clamp(-1.0, 1.0))
    }
}

/// The higher of the two, ignoring values that are not real loudness yet
/// (the meters report -inf until their window has filled).
fn higher(current: Option<f64>, new: Option<f64>) -> Option<f64> {
    match (current, new.filter(|v| v.is_finite())) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

pub(crate) fn read_measurement(meter: &EbuR128, channels: u32) -> Result<Measurement> {
    let integrated = meter.loudness_global().context("falha ao calcular loudness")?;
    if !integrated.is_finite() || integrated < SILENCE_LUFS {
        bail!("o áudio é silencioso demais para ser normalizado");
    }
    let mut true_peak = 0.0f64;
    let mut sample_peak = 0.0f64;
    for ch in 0..channels {
        true_peak = true_peak.max(meter.true_peak(ch).unwrap_or(0.0));
        sample_peak = sample_peak.max(meter.sample_peak(ch).unwrap_or(0.0));
    }
    Ok(Measurement {
        integrated_lufs: integrated,
        loudness_range: meter.loudness_range().unwrap_or(0.0),
        true_peak_db: linear_to_db(true_peak),
        sample_peak_db: linear_to_db(sample_peak),
        max_short_term_lufs: None,
        max_momentary_lufs: None,
        stereo_correlation: None,
    })
}

/// Pure function: how far is the audio from the target and what will
/// normalization do about it.
pub fn assess(m: &Measurement, targets: Targets) -> Assessment {
    let ideal_gain = targets.target_lufs - m.integrated_lufs;
    let gain_db = ideal_gain.clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
    let peak_after_gain_db = m.true_peak_db + gain_db;
    let deviation_lu = m.integrated_lufs - targets.target_lufs;
    let peak_over_ceiling = m.true_peak_db > targets.true_peak_db + 0.1;

    // 12 LU off target counts as "as bad as it gets"; peaks over the
    // ceiling alone can account for up to half the bar.
    let loudness_part = (deviation_lu.abs() / 12.0).min(1.0);
    let peak_part = if peak_over_ceiling {
        ((m.true_peak_db - targets.true_peak_db) / 6.0).min(1.0) * 0.5
    } else {
        0.0
    };
    let improvement_percent = (loudness_part.max(peak_part) * 100.0).round();

    let verdict = match improvement_percent {
        p if p < 5.0 => Verdict::None,
        p if p < 17.0 => Verdict::Small,
        p if p < 50.0 => Verdict::Moderate,
        _ => Verdict::Large,
    };

    Assessment {
        target_lufs: targets.target_lufs,
        true_peak_ceiling_db: targets.true_peak_db,
        gain_db,
        gain_capped: (ideal_gain - gain_db).abs() > f64::EPSILON,
        expected_lufs: m.integrated_lufs + gain_db,
        peak_after_gain_db,
        limiter_reduction_db: (peak_after_gain_db - targets.true_peak_db).max(0.0),
        deviation_lu,
        peak_over_ceiling,
        clipping: m.sample_peak_db >= -0.1,
        improvement_percent,
        verdict,
    }
}

#[cfg(test)]
mod tests;
