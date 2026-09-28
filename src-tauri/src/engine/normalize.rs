//! Normalization: gain + look-ahead limiter on the decoded audio, re-encoded
//! and remuxed next to the source, then swapped in place of the original.

use anyhow::{bail, Context, Result};
use ebur128::{EbuR128, Mode};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::Instant;

use super::analyze::{self, Measurement, MAX_GAIN_DB};
use super::av::{self, MediaInfo, PcmDecoder, Remuxer};
use super::job::{Cancelled, Job, Progress};
use super::limiter::Limiter;
use super::{db_to_linear, linear_to_db, Targets};

/// Headroom kept below the true-peak ceiling for lossy-codec overshoot.
const LIMITER_MARGIN_DB: f64 = 0.5;

/// Limiting removes loudness. When the analysis predicts at least this much
/// gain reduction, a decode-only pass measures the real result first and the
/// gain is corrected so the output still lands on the target.
const CALIBRATE_ABOVE_DB: f64 = 1.0;
const MAX_CALIBRATION_PASSES: usize = 2;
/// Upper bound on the extra gain calibration may add (more means more limiting).
const MAX_CALIBRATION_BOOST_DB: f64 = 6.0;
/// Close enough to the target to stop calibrating.
const CALIBRATION_TOLERANCE_LU: f64 = 0.3;
/// Encoded true peak allowed above the ceiling before re-encoding.
const PEAK_TOLERANCE_DB: f64 = 0.5;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizeReport {
    pub path: String,
    pub input_lufs: f64,
    pub output_lufs: f64,
    pub output_true_peak_db: f64,
    pub target_lufs: f64,
    pub gain_db: f64,
    pub limiter_max_reduction_db: f64,
    pub elapsed_seconds: f64,
    pub size_before: u64,
    pub size_after: u64,
}

/// `measured` comes from a previous analysis of the same file and lets us
/// skip the measuring pass.
pub fn run(targets: Targets, input: &Path, measured: Option<Measurement>, job: &Job) -> Result<NormalizeReport> {
    let started = Instant::now();
    let info = av::probe(input)?;
    let size_before = fs::metadata(input).context("não foi possível ler o arquivo")?.len();

    // Progress budget: each decode-only pass counts 1, the encoding pass 2.
    let mut cursor = 0.0;
    let measurement = match measured {
        Some(m) => m,
        None => {
            let mut progress = job.stage("analyze", cursor, 30.0);
            cursor += 30.0;
            analyze::measure(input, &info, job, &mut progress)?
        }
    };
    let ceiling_db = targets.true_peak_db - LIMITER_MARGIN_DB;
    let mut gain_db = (targets.target_lufs - measurement.integrated_lufs).clamp(-MAX_GAIN_DB, MAX_GAIN_DB);

    let predicted_limiting = measurement.true_peak_db + gain_db - ceiling_db;
    if predicted_limiting > CALIBRATE_ABOVE_DB && gain_db < MAX_GAIN_DB {
        let base_gain = gain_db;
        let span = 10.0;
        for _ in 0..MAX_CALIBRATION_PASSES {
            let mut progress = job.stage("calibrate", cursor, span);
            cursor += span;
            let pass = simulate(input, &info, gain_db, ceiling_db, job, &mut progress)?;
            let miss = targets.target_lufs - pass.output_lufs;
            if miss.abs() < CALIBRATION_TOLERANCE_LU {
                break;
            }
            gain_db = (gain_db + miss)
                .clamp(base_gain, base_gain + MAX_CALIBRATION_BOOST_DB)
                .min(MAX_GAIN_DB);
        }
    }

    let settings = EncodeSettings { gain_db, ceiling_db, encoder_options: "" };
    let temp = TempFile(temp_path(input, "normalizing")?);
    let mut encoded = encode(input, &temp.0, &info, &settings, job, ("normalize", cursor, 98.0))?;

    // Lossy encoders can overshoot the ceiling. The native AAC encoder's
    // default coder occasionally does so badly (hard transients right at
    // the start of the stream); one retry with its simpler coder fixes it.
    if encoded.true_peak_db > targets.true_peak_db + PEAK_TOLERANCE_DB && encoded.encoder == "aac" {
        let retry = TempFile(temp_path(input, "normalizing-retry")?);
        let settings = EncodeSettings { encoder_options: "aac_coder=fast", ..settings };
        let second = encode(input, &retry.0, &info, &settings, job, ("retry", cursor, 98.0))?;
        if second.true_peak_db < encoded.true_peak_db {
            fs::rename(&retry.0, &temp.0).context("falha ao preparar o arquivo final")?;
            std::mem::forget(retry);
            encoded = second;
        }
    }

    job.stage("finalize", 99.0, 1.0);
    job.check_cancelled()?;
    replace_original(temp, input)?;

    Ok(NormalizeReport {
        path: input.display().to_string(),
        input_lufs: measurement.integrated_lufs,
        output_lufs: encoded.output_lufs,
        output_true_peak_db: encoded.true_peak_db,
        target_lufs: targets.target_lufs,
        gain_db,
        limiter_max_reduction_db: encoded.limiter_reduction_db,
        elapsed_seconds: started.elapsed().as_secs_f64(),
        size_before,
        size_after: fs::metadata(input).map(|m| m.len()).unwrap_or(0),
    })
}

#[derive(Clone, Copy)]
struct EncodeSettings<'a> {
    gain_db: f64,
    ceiling_db: f64,
    encoder_options: &'a str,
}

/// What ended up in the written file.
struct Encoded {
    encoder: String,
    output_lufs: f64,
    true_peak_db: f64,
    limiter_reduction_db: f64,
}

/// Blocks in flight between pipeline stages (8192 frames each).
const QUEUE_BLOCKS: usize = 8;

enum ToEncoder {
    Block(Vec<f32>),
    Finish,
}

/// Marker error: a downstream pipeline stage stopped; its own error wins.
#[derive(Debug)]
struct StageStopped;

impl std::fmt::Display for StageStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("etapa de codificação interrompida")
    }
}

impl std::error::Error for StageStopped {}

/// Render into `output` and measure what was encoded. Runs as a pipeline:
/// decode -> [gain, limiter, meter] -> encode + mux -> meter of the encoded
/// audio decoded back, each stage on its own thread.
fn encode(
    input: &Path,
    output: &Path,
    info: &MediaInfo,
    settings: &EncodeSettings,
    job: &Job,
    (stage, start, end): (&'static str, f64, f64),
) -> Result<Encoded> {
    let remuxer = Remuxer::open(input, output, info, settings.encoder_options)?;
    let encoder = remuxer.encoder();
    let monitor = remuxer.monitor_format();
    let decoder = PcmDecoder::open(input, info)?;
    let mut chain = Chain::new(info, settings.gain_db, settings.ceiling_db)?;
    let mut progress = job.stage(stage, start, end - start);
    let channels = info.audio.channels as usize;
    let total_frames = info.duration * info.audio.sample_rate as f64;

    let (rendered, finished, measured) = thread::scope(|s| {
        let (pcm_tx, pcm_rx) = sync_channel::<Vec<f32>>(QUEUE_BLOCKS);
        let (enc_tx, enc_rx) = sync_channel::<ToEncoder>(QUEUE_BLOCKS);
        let (mon_tx, mon_rx) = sync_channel::<Vec<f32>>(QUEUE_BLOCKS);

        let reader = s.spawn(move || -> Result<()> {
            let mut decoder = decoder;
            loop {
                let mut block = Vec::new();
                if !decoder.read(&mut block)? || pcm_tx.send(block).is_err() {
                    return Ok(());
                }
            }
        });

        // Ok(false): upstream stopped before asking to finish.
        let writer = s.spawn(move || -> Result<bool> {
            let mut remuxer = remuxer;
            let forward = |remuxer: &mut Remuxer| {
                let mut block = Vec::new();
                while remuxer.read_monitor(&mut block) {
                    if mon_tx.send(std::mem::take(&mut block)).is_err() {
                        break;
                    }
                }
            };
            for message in enc_rx {
                match message {
                    ToEncoder::Block(block) => {
                        remuxer.write(&block)?;
                        forward(&mut remuxer);
                    }
                    ToEncoder::Finish => {
                        remuxer.finish()?;
                        forward(&mut remuxer);
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        });

        let verifier = s.spawn(move || -> Result<Option<Measurement>> {
            let Some(format) = monitor else {
                return Ok(None);
            };
            let mut meter = EbuR128::new(format.channels, format.sample_rate, Mode::I | Mode::TRUE_PEAK)
                .context("falha ao iniciar o medidor de loudness")?;
            for block in mon_rx {
                meter.add_frames_f32(&block).context("falha ao medir loudness")?;
            }
            analyze::read_measurement(&meter, format.channels).map(Some)
        });

        let rendered = (move || -> Result<Rendered> {
            let send = |message: ToEncoder| enc_tx.send(message).map_err(|_| anyhow::Error::new(StageStopped));
            let mut frames = 0u64;
            for mut block in pcm_rx {
                job.check_cancelled()?;
                frames += (block.len() / channels) as u64;
                let mut out = Vec::with_capacity(block.len());
                chain.process(&mut block, &mut out)?;
                send(ToEncoder::Block(out))?;
                if total_frames > 0.0 {
                    progress.update(frames as f64 / total_frames);
                }
            }
            reader.join().expect("decoder thread panicked")?;
            job.check_cancelled()?;
            let mut out = Vec::new();
            chain.flush(&mut out)?;
            send(ToEncoder::Block(out))?;
            send(ToEncoder::Finish)?;
            chain.result()
        })();
        let finished = writer.join().expect("encoder thread panicked");
        let measured = verifier.join().expect("meter thread panicked");
        (rendered, finished, measured)
    });

    let rendered = match (rendered, finished) {
        (Err(e), _) if e.is::<Cancelled>() => return Err(e),
        (_, Err(e)) => return Err(e),
        (Err(e), _) => return Err(e),
        (Ok(_), Ok(false)) => bail!("a codificação terminou antes do fim do áudio"),
        (Ok(r), Ok(true)) => r,
    };

    let written = verify(output, info)?;
    let actual = match measured? {
        Some(m) => m,
        // No decoder for the encoded codec: read the file back instead.
        None => analyze::measure(output, &written, job, &mut job.stage("verify", end, 0.0))?,
    };
    Ok(Encoded {
        encoder,
        output_lufs: actual.integrated_lufs,
        true_peak_db: actual.true_peak_db,
        limiter_reduction_db: rendered.limiter_reduction_db,
    })
}

struct Rendered {
    output_lufs: f64,
    limiter_reduction_db: f64,
}

/// Gain -> true-peak limiter -> integrated-loudness meter.
struct Chain {
    gain: f32,
    limiter: Limiter,
    meter: EbuR128,
}

impl Chain {
    fn new(info: &MediaInfo, gain_db: f64, ceiling_db: f64) -> Result<Self> {
        let channels = info.audio.channels;
        let sample_rate = info.audio.sample_rate;
        Ok(Self {
            gain: db_to_linear(gain_db) as f32,
            limiter: Limiter::new(channels as usize, sample_rate, db_to_linear(ceiling_db) as f32),
            // Only integrated loudness: peaks are measured on the encoded audio.
            meter: EbuR128::new(channels, sample_rate, Mode::I).context("falha ao iniciar o medidor de loudness")?,
        })
    }

    /// Applies the gain to `block` in place and writes the limited audio to `out`.
    fn process(&mut self, block: &mut [f32], out: &mut Vec<f32>) -> Result<()> {
        for sample in block.iter_mut() {
            *sample *= self.gain;
        }
        self.limiter.process(block, out);
        self.meter.add_frames_f32(out).context("falha ao medir loudness")
    }

    /// Drains the limiter's look-ahead into `out`.
    fn flush(&mut self, out: &mut Vec<f32>) -> Result<()> {
        self.limiter.flush(out);
        self.meter.add_frames_f32(out).context("falha ao medir loudness")
    }

    fn result(&self) -> Result<Rendered> {
        Ok(Rendered {
            output_lufs: self.meter.loudness_global().context("falha ao calcular loudness")?,
            limiter_reduction_db: -linear_to_db(self.limiter.min_gain),
        })
    }
}

/// Dry run used to calibrate the gain: decode -> chain, nothing is written.
fn simulate(
    input: &Path,
    info: &MediaInfo,
    gain_db: f64,
    ceiling_db: f64,
    job: &Job,
    progress: &mut Progress,
) -> Result<Rendered> {
    let mut chain = Chain::new(info, gain_db, ceiling_db)?;
    let mut decoder = PcmDecoder::open(input, info)?;
    let channels = info.audio.channels as usize;
    let total_frames = info.duration * info.audio.sample_rate as f64;
    let mut frames = 0u64;
    let mut block = Vec::new();
    let mut out = Vec::new();
    while decoder.read(&mut block)? {
        job.check_cancelled()?;
        frames += (block.len() / channels) as u64;
        chain.process(&mut block, &mut out)?;
        if total_frames > 0.0 {
            progress.update(frames as f64 / total_frames);
        }
    }
    chain.flush(&mut out)?;
    chain.result()
}

/// Sanity-check the new file before it replaces the original.
fn verify(output: &Path, original: &MediaInfo) -> Result<MediaInfo> {
    let written = av::probe(output).context("o arquivo gerado não pôde ser lido; o original foi mantido")?;
    if original.has_video && !written.has_video {
        bail!("o vídeo não foi copiado corretamente; o original foi mantido");
    }
    if original.duration > 0.0 {
        let tolerance = (original.duration * 0.02).max(1.0);
        if (written.duration - original.duration).abs() > tolerance {
            bail!(
                "duração inesperada no arquivo gerado ({:.1}s vs {:.1}s); o original foi mantido",
                written.duration,
                original.duration
            );
        }
    }
    Ok(written)
}

fn temp_path(input: &Path, tag: &str) -> Result<PathBuf> {
    let ext = av::extension(input);
    if ext.is_empty() {
        bail!("o arquivo precisa ter extensão (ex.: .mp4) para identificar o formato");
    }
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video".into());
    let dir = input.parent().unwrap_or_else(|| Path::new("."));
    Ok(dir.join(format!(".{stem}.{tag}.{ext}")))
}

fn replace_original(temp: TempFile, input: &Path) -> Result<()> {
    // std::fs::rename replaces the destination atomically on the same
    // volume (MoveFileExW with MOVEFILE_REPLACE_EXISTING on Windows).
    fs::rename(&temp.0, input).context(
        "não foi possível substituir o arquivo original (ele está aberto em outro programa?)",
    )?;
    std::mem::forget(temp);
    Ok(())
}

/// Deletes the temporary output unless it was moved over the original.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// End-to-end run over real media. The files are modified in place, so
    /// point NORMALIZER_E2E_FILES (`;`-separated) at throwaway copies.
    #[test]
    #[ignore]
    fn e2e_normalize_files() {
        let files = std::env::var("NORMALIZER_E2E_FILES").expect("set NORMALIZER_E2E_FILES");
        let targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };
        for file in files.split(';').filter(|f| !f.is_empty()) {
            let path = Path::new(file);
            let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
            let before = analyze::run(targets, path, &job).unwrap_or_else(|e| panic!("{file}: {e:#}"));
            let report = run(targets, path, None, &job).unwrap_or_else(|e| panic!("{file}: {e:#}"));
            let after = analyze::run(targets, path, &job).unwrap_or_else(|e| panic!("{file}: {e:#}"));
            println!(
                "{file}: {:.1} -> {:.1} LUFS (report {:.1}, TP {:.1}), TP {:.1} dBTP, limiter {:.1} dB, {:.2}s -> {:.2}s",
                before.measurement.integrated_lufs,
                after.measurement.integrated_lufs,
                report.output_lufs,
                report.output_true_peak_db,
                after.measurement.true_peak_db,
                report.limiter_max_reduction_db,
                before.media.duration,
                after.media.duration,
            );
            // Equals the target unless the gain was capped at MAX_GAIN_DB.
            let expected = before.assessment.expected_lufs;
            assert!((after.measurement.integrated_lufs - expected).abs() < 1.0, "{file}: loudness");
            assert!(after.measurement.true_peak_db < targets.true_peak_db + 0.5, "{file}: true peak");
            assert!((after.media.duration - before.media.duration).abs() < 0.5, "{file}: duration");
            assert_eq!(after.media.has_video, before.media.has_video, "{file}: video");
        }
    }
}
