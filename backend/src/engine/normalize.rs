//! Normalization: gain + look-ahead limiter on the decoded audio, re-encoded
//! and remuxed next to the source, then swapped in place of the original.

use anyhow::{bail, Context, Result};
use ebur128::{EbuR128, Mode};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use super::analyze::{self, Measurement, CACHE_LIMIT_BYTES, MAX_GAIN_DB};
use super::av::{self, AudioCache, AudioSource, MediaInfo, PcmDecoder, Remuxer};
use super::cleanup::{Cleanup, CleanupSettings};
use super::job::{Cancelled, Job, Progress, StageStopped, QUEUE_BLOCKS};
use super::leveler::Leveler;
use super::limiter::Limiter;
use super::{db_to_linear, linear_to_db, Targets};

/// Headroom kept below the true-peak ceiling for lossy-codec overshoot.
const LIMITER_MARGIN_DB: f64 = 0.5;

/// Limiting removes loudness. When the analysis predicts at least this much
/// gain reduction, a decode-only pass measures the real result first and the
/// gain is corrected so the output still lands on the target.
const CALIBRATE_ABOVE_DB: f64 = 1.0;
/// Extra gains rendered side by side during calibration; the last one is the
/// most calibration may add (more gain means more limiting).
const CALIBRATION_STEPS_DB: [f64; 5] = [0.0, 1.5, 3.0, 4.5, 6.0];
/// With leveling on, the loudness after the rider can land on either side of
/// the plain estimate, so calibration also tries lower gains.
const LEVELING_CALIBRATION_STEPS_DB: [f64; 8] = [-4.5, -3.0, -1.5, 0.0, 1.5, 3.0, 4.5, 6.0];
/// Encoded true peak allowed above the ceiling before re-encoding.
const PEAK_TOLERANCE_DB: f64 = 0.5;

/// What happens to the original file once the new one is verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutputMode {
    /// Swap the verified file in place of the original.
    #[default]
    Replace,
    /// Keep the original and save the result next to it as "name (normalized).ext".
    Copy,
}

/// User choices that shape a normalization beyond the loudness targets.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Options {
    pub output: OutputMode,
    /// Audio track to normalize, by order among the audio tracks (0 = first).
    pub track: usize,
    /// Normalize every audio track instead of just `track`.
    pub all_tracks: bool,
    /// Level the dynamics first (speech): quiet passages up, loud ones down.
    pub leveling: bool,
    /// Save the audio as FLAC (no second lossy generation) when the
    /// container accepts it.
    pub lossless: bool,
    /// Optional audio clean-up; the video and the duration are never touched.
    pub cleanup: CleanupSettings,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizeReport {
    pub path: String,
    /// Where the result was written (the source path when it was replaced).
    pub output_path: String,
    pub replaced: bool,
    /// How many audio tracks were normalized.
    pub tracks_processed: usize,
    /// The dynamics were leveled before the final gain.
    pub leveled: bool,
    /// Clean-up stages that ran, and how many clipped samples were rebuilt.
    pub cleanup: CleanupSettings,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declipped_samples: Option<u64>,
    pub input_lufs: f64,
    pub input_true_peak_db: f64,
    pub input_loudness_range: f64,
    /// Loudest short-term / momentary reading of the source, when measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_max_short_term_lufs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_max_momentary_lufs: Option<f64>,
    pub output_lufs: f64,
    pub output_true_peak_db: f64,
    pub target_lufs: f64,
    pub true_peak_ceiling_db: f64,
    pub gain_db: f64,
    pub limiter_max_reduction_db: f64,
    pub elapsed_seconds: f64,
    pub size_before: u64,
    pub size_after: u64,
    pub input_media: ProcessedMedia,
    pub output_media: ProcessedMedia,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessedMedia {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub duration: f64,
    pub has_video: bool,
}

fn media_details(info: &MediaInfo) -> ProcessedMedia {
    ProcessedMedia {
        codec: info.audio.codec.clone(), sample_rate: info.audio.sample_rate,
        channels: info.audio.channels, duration: info.duration, has_video: info.has_video,
    }
}

/// `measured` and `cache` come from a previous analysis of the same file and
/// track: the first skips the measuring pass, the second lets every decode run
/// from memory so the file itself is read only once (by the remuxer).
///
/// With `all_tracks` every audio track is normalized in turn, each pass
/// reading the previous pass output, and only the last file replaces (or
/// sits next to) the original.
pub fn run(
    targets: Targets,
    options: Options,
    input: &Path,
    measured: Option<Measurement>,
    cache: Option<Arc<AudioCache>>,
    job: &Job,
) -> Result<NormalizeReport> {
    let started = Instant::now();
    let size_before = fs::metadata(input).context("não foi possível ler o arquivo")?.len();
    let tracks: Vec<usize> = if options.all_tracks {
        (0..av::audio_tracks(input)?.len().max(1)).collect()
    } else {
        vec![options.track]
    };
    let passes = tracks.len() as f64;

    let mut reuse = Some((measured, cache));
    let mut finished: Vec<TrackPass> = Vec::with_capacity(tracks.len());
    for (pass, &track) in tracks.iter().enumerate() {
        // The analysis belongs to one track: only that pass may reuse it.
        let (measured, cache) = if track == options.track { reuse.take().unwrap_or((None, None)) } else { (None, None) };
        let window = job.window(pass as f64 / passes, 1.0 / passes);
        let current: &Path = finished.last().map_or(input, |previous| previous.temp.0.as_path());
        let done = normalize_track(targets, track, input, current, pass, measured, cache, options.leveling, options.lossless, options.cleanup, &window)?;
        finished.push(done);
    }

    job.stage("finalize", 99.0, 1.0);
    job.check_cancelled()?;
    // The report describes the track the user chose (the first one for "all").
    let primary = finished.iter().position(|done| done.track == options.track).unwrap_or(0);
    let tracks_processed = finished.len();
    let done = &finished[primary];
    let (measurement, gain_db) = (done.measurement, done.gain_db);
    let (output_lufs, output_true_peak_db) = (done.encoded.output_lufs, done.encoded.true_peak_db);
    let limiter_max_reduction_db = done.encoded.limiter_reduction_db;
    let declipped_samples = done.encoded.declipped_samples;
    let (input_media, output_media) = (media_details(&done.info), media_details(&done.encoded.media));

    // Only the last pass output survives; earlier ones are deleted on drop.
    let last = finished.pop().expect("at least one track pass");
    let output_path = finish_output(last.temp, input, options.output)?;

    Ok(NormalizeReport {
        path: input.display().to_string(),
        output_path: output_path.display().to_string(),
        replaced: options.output == OutputMode::Replace,
        tracks_processed,
        leveled: options.leveling,
        cleanup: options.cleanup,
        declipped_samples,
        input_lufs: measurement.integrated_lufs,
        input_true_peak_db: measurement.true_peak_db,
        input_loudness_range: measurement.loudness_range,
        input_max_short_term_lufs: measurement.max_short_term_lufs,
        input_max_momentary_lufs: measurement.max_momentary_lufs,
        output_lufs,
        output_true_peak_db,
        target_lufs: targets.target_lufs,
        true_peak_ceiling_db: targets.true_peak_db,
        gain_db,
        limiter_max_reduction_db,
        elapsed_seconds: started.elapsed().as_secs_f64(),
        size_before,
        size_after: fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0),
        input_media,
        output_media,
    })
}

/// What normalizing one audio track produced.
struct TrackPass {
    track: usize,
    temp: TempFile,
    info: MediaInfo,
    measurement: Measurement,
    gain_db: f64,
    encoded: Encoded,
}

/// Normalizes `track` of `source`, writing a hidden temporary file next to
/// `original`. `pass` keeps the temporary names of several passes apart.
#[allow(clippy::too_many_arguments)]
fn normalize_track(
    targets: Targets,
    track: usize,
    original: &Path,
    source_file: &Path,
    pass: usize,
    measured: Option<Measurement>,
    mut cache: Option<Arc<AudioCache>>,
    leveling: bool,
    lossless: bool,
    cleanup: CleanupSettings,
    job: &Job,
) -> Result<TrackPass> {
    let info = av::probe_track(source_file, track)?;

    // Progress budget: each decode-only pass counts 1, the encoding pass 2.
    let mut cursor = 0.0;
    let measurement = match measured {
        Some(m) => m,
        None => {
            let mut progress = job.stage("analyze", cursor, 30.0);
            cursor += 30.0;
            let mut decoder = PcmDecoder::open(source_file, &info, CACHE_LIMIT_BYTES)?;
            let measurement = analyze::measure(&mut decoder, &info, job, &mut progress)?;
            cache = cache.or_else(|| decoder.take_cache().map(Arc::new));
            measurement
        }
    };
    let source = match cache {
        Some(cache) => AudioSource::Cache(cache),
        None => AudioSource::File(source_file),
    };
    let ceiling_db = targets.true_peak_db - LIMITER_MARGIN_DB;
    let mut gain_db = (targets.target_lufs - measurement.integrated_lufs).clamp(-MAX_GAIN_DB, MAX_GAIN_DB);

    let predicted_limiting = measurement.true_peak_db + gain_db - ceiling_db;
    // The rider moves the loudness, so leveling always needs the dry run.
    let leveler_reference = leveling.then_some(measurement.integrated_lufs);
    // Rebuilt peaks also move the loudness and the peak, so declipping calibrates too.
    if leveling || cleanup.declip || (predicted_limiting > CALIBRATE_ABOVE_DB && gain_db < MAX_GAIN_DB) {
        let mut progress = job.stage("calibrate", cursor, 15.0);
        cursor += 15.0;
        let plan = Calibration { base_gain_db: gain_db, ceiling_db, target_lufs: targets.target_lufs, leveler_reference, cleanup };
        gain_db = calibrate(&source, &info, plan, job, &mut progress)?;
    }

    let settings = EncodeSettings { gain_db, ceiling_db, leveler_reference, lossless, cleanup, encoder_options: "" };
    let temp = TempFile(temp_path(original, &format!("normalizing-{pass}"))?);
    let mut encoded = encode(source_file, &source, &temp.0, &info, &settings, job, ("normalize", cursor, 98.0))?;

    // Lossy encoders can overshoot the ceiling. The native AAC encoder default
    // coder occasionally does so badly (hard transients right at the start of
    // the stream); one retry with its simpler coder fixes it.
    if encoded.true_peak_db > targets.true_peak_db + PEAK_TOLERANCE_DB && encoded.encoder == "aac" {
        let retry = TempFile(temp_path(original, &format!("normalizing-{pass}-retry"))?);
        let settings = EncodeSettings { encoder_options: "aac_coder=fast", ..settings };
        let second = encode(source_file, &source, &retry.0, &info, &settings, job, ("retry", cursor, 98.0))?;
        if second.true_peak_db < encoded.true_peak_db {
            fs::rename(&retry.0, &temp.0).context("falha ao preparar o arquivo final")?;
            std::mem::forget(retry);
            encoded = second;
        }
    }
    Ok(TrackPass { track, temp, info, measurement, gain_db, encoded })
}

#[derive(Clone, Copy)]
struct EncodeSettings<'a> {
    gain_db: f64,
    ceiling_db: f64,
    /// Program loudness the leveler pulls toward; None leaves it off.
    leveler_reference: Option<f64>,
    /// Prefer FLAC over the source codec.
    lossless: bool,
    cleanup: CleanupSettings,
    encoder_options: &'a str,
}

/// What ended up in the written file.
struct Encoded {
    encoder: String,
    output_lufs: f64,
    true_peak_db: f64,
    limiter_reduction_db: f64,
    declipped_samples: Option<u64>,
    media: MediaInfo,
}

enum ToEncoder {
    Block(Vec<f32>),
    Finish,
}

/// Render into `output` and measure what was encoded. Runs as a pipeline:
/// decode -> [gain, limiter, meter] -> encode + mux -> meter of the encoded
/// audio decoded back, each stage on its own thread.
#[allow(clippy::too_many_arguments)]
fn encode(
    input: &Path,
    source: &AudioSource,
    output: &Path,
    info: &MediaInfo,
    settings: &EncodeSettings,
    job: &Job,
    (stage, start, end): (&'static str, f64, f64),
) -> Result<Encoded> {
    let remuxer = Remuxer::open(input, output, info, settings.encoder_options, settings.lossless)?;
    let encoder = remuxer.encoder();
    let monitor = remuxer.monitor_format();
    let decoder = source.open(info)?;
    let mut chain = Chain::new(info, settings.gain_db, settings.ceiling_db, settings.leveler_reference, settings.cleanup)?;
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
        None => {
            let mut decoder = PcmDecoder::open(output, &written, 0)?;
            analyze::measure(&mut decoder, &written, job, &mut job.stage("verify", end, 0.0))?
        }
    };
    Ok(Encoded {
        encoder,
        output_lufs: actual.integrated_lufs,
        true_peak_db: actual.true_peak_db,
        limiter_reduction_db: rendered.limiter_reduction_db,
        declipped_samples: rendered.declipped_samples,
        media: written,
    })
}

struct Rendered {
    output_lufs: f64,
    limiter_reduction_db: f64,
    declipped_samples: Option<u64>,
}

/// [Cleanup] -> [Leveler] -> gain -> true-peak limiter -> integrated-loudness meter.
struct Chain {
    cleanup: Option<Cleanup>,
    /// Cleaned audio of the block being processed.
    scratch: Vec<f32>,
    leveler: Option<Leveler>,
    gain: f32,
    limiter: Limiter,
    meter: EbuR128,
}

impl Chain {
    fn new(
        info: &MediaInfo,
        gain_db: f64,
        ceiling_db: f64,
        leveler_reference: Option<f64>,
        cleanup: CleanupSettings,
    ) -> Result<Self> {
        let channels = info.audio.channels;
        let sample_rate = info.audio.sample_rate;
        Ok(Self {
            cleanup: cleanup.any().then(|| Cleanup::new(cleanup, channels as usize, sample_rate)),
            scratch: Vec::new(),
            leveler: leveler_reference.map(|lufs| Leveler::new(channels as usize, sample_rate, lufs)),
            gain: db_to_linear(gain_db) as f32,
            limiter: Limiter::new(channels as usize, sample_rate, db_to_linear(ceiling_db) as f32),
            // Only integrated loudness: peaks are measured on the encoded audio.
            meter: EbuR128::new(channels, sample_rate, Mode::I).context("falha ao iniciar o medidor de loudness")?,
        })
    }

    /// Cleans, levels and applies the gain to `block` and writes the limited
    /// audio to `out`. `block` is used as scratch space.
    fn process(&mut self, block: &mut [f32], out: &mut Vec<f32>) -> Result<()> {
        match self.cleanup.as_mut() {
            Some(cleanup) => {
                let mut cleaned = std::mem::take(&mut self.scratch);
                cleaned.clear();
                cleanup.process(block, &mut cleaned);
                let done = self.shape(&mut cleaned, out);
                self.scratch = cleaned;
                done
            }
            None => self.shape(block, out),
        }
    }

    /// Leveler, gain and limiter over already cleaned audio.
    fn shape(&mut self, block: &mut [f32], out: &mut Vec<f32>) -> Result<()> {
        if let Some(leveler) = self.leveler.as_mut() {
            leveler.process(block);
        }
        for sample in block.iter_mut() {
            *sample *= self.gain;
        }
        self.limiter.process(block, out);
        self.meter.add_frames_f32(out).context("falha ao medir loudness")
    }

    /// Drains the clean-up and limiter look-ahead into `out`.
    fn flush(&mut self, out: &mut Vec<f32>) -> Result<()> {
        let mut tail = Vec::new();
        if let Some(cleanup) = self.cleanup.as_mut() {
            let mut held = Vec::new();
            cleanup.flush(&mut held);
            self.shape(&mut held, &mut tail)?;
        }
        self.limiter.flush(&mut tail);
        self.meter.add_frames_f32(&tail).context("falha ao medir loudness")?;
        out.append(&mut tail);
        Ok(())
    }

    fn result(&self) -> Result<Rendered> {
        Ok(Rendered {
            output_lufs: self.meter.loudness_global().context("falha ao calcular loudness")?,
            limiter_reduction_db: -linear_to_db(self.limiter.min_gain),
            declipped_samples: self.cleanup.as_ref().and_then(Cleanup::declipped_samples),
        })
    }
}

/// What a calibration run needs to know about the chain it renders.
#[derive(Clone, Copy)]
struct Calibration {
    base_gain_db: f64,
    ceiling_db: f64,
    target_lufs: f64,
    leveler_reference: Option<f64>,
    cleanup: CleanupSettings,
}

/// Dry run that finds the gain which, after limiting, lands on `target`.
/// One decode feeds a chain per candidate gain, each on its own thread; the
/// answer is interpolated between the candidates that bracket the target.
fn calibrate(
    source: &AudioSource,
    info: &MediaInfo,
    plan: Calibration,
    job: &Job,
    progress: &mut Progress,
) -> Result<f64> {
    let Calibration { base_gain_db, ceiling_db, target_lufs, leveler_reference, cleanup } = plan;
    let steps: &[f64] = if leveler_reference.is_some() { &LEVELING_CALIBRATION_STEPS_DB } else { &CALIBRATION_STEPS_DB };
    let mut gains: Vec<f64> = steps
        .iter()
        .map(|step| (base_gain_db + step).clamp(-MAX_GAIN_DB, MAX_GAIN_DB))
        .collect();
    gains.dedup();
    let chains = gains
        .iter()
        .map(|&gain| Chain::new(info, gain, ceiling_db, leveler_reference, cleanup))
        .collect::<Result<Vec<_>>>()?;
    let mut decoder = source.open(info)?;
    let channels = info.audio.channels as usize;
    let total_frames = info.duration * info.audio.sample_rate as f64;

    let loudness = thread::scope(|s| -> Result<Vec<f64>> {
        let mut senders = Vec::with_capacity(chains.len());
        let mut workers = Vec::with_capacity(chains.len());
        for mut chain in chains {
            let (tx, rx) = sync_channel::<Arc<Vec<f32>>>(QUEUE_BLOCKS);
            senders.push(tx);
            workers.push(s.spawn(move || -> Result<f64> {
                let mut block = Vec::new();
                let mut out = Vec::new();
                for shared in rx {
                    block.clear();
                    block.extend_from_slice(&shared);
                    chain.process(&mut block, &mut out)?;
                }
                chain.flush(&mut out)?;
                Ok(chain.result()?.output_lufs)
            }));
        }

        let fed = (|| -> Result<()> {
            let mut frames = 0u64;
            let mut block = Vec::new();
            while decoder.read(&mut block)? {
                job.check_cancelled()?;
                frames += (block.len() / channels) as u64;
                let shared = Arc::new(std::mem::take(&mut block));
                for tx in &senders {
                    tx.send(shared.clone()).map_err(|_| anyhow::Error::new(StageStopped))?;
                }
                if total_frames > 0.0 {
                    progress.update(frames as f64 / total_frames);
                }
            }
            Ok(())
        })();
        drop(senders);
        let results: Vec<Result<f64>> = workers
            .into_iter()
            .map(|w| w.join().expect("calibration thread panicked"))
            .collect();
        match fed {
            Err(e) if !e.is::<StageStopped>() => Err(e),
            _ => results.into_iter().collect(),
        }
    })?;

    Ok(interpolate_gain(&gains, &loudness, target_lufs))
}

/// Gain whose loudness hits `target`, by linear interpolation over the
/// measured (gain, loudness) pairs; loudness grows with gain.
fn interpolate_gain(gains: &[f64], loudness: &[f64], target: f64) -> f64 {
    if target <= loudness[0] {
        return gains[0];
    }
    for i in 1..gains.len() {
        if target <= loudness[i] {
            let span = (loudness[i] - loudness[i - 1]).max(1e-9);
            let t = (target - loudness[i - 1]) / span;
            return gains[i - 1] + t * (gains[i] - gains[i - 1]);
        }
    }
    gains[gains.len() - 1]
}

/// Sanity-check the new file before it replaces the original.
fn verify(output: &Path, original: &MediaInfo) -> Result<MediaInfo> {
    let written = av::probe_track(output, original.track)
        .context("o arquivo gerado não pôde ser lido; o original foi mantido")?;
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

/// Puts the verified temporary file where the user asked for it and returns
/// its final path.
fn finish_output(temp: TempFile, input: &Path, mode: OutputMode) -> Result<PathBuf> {
    match mode {
        OutputMode::Replace => {
            replace_original(temp, input)?;
            Ok(input.to_path_buf())
        }
        OutputMode::Copy => {
            let target = copy_path(input)?;
            fs::rename(&temp.0, &target).context("não foi possível salvar a cópia normalizada")?;
            std::mem::forget(temp);
            Ok(target)
        }
    }
}

/// "clip.mp4" -> "clip (normalized).mp4", then "(normalized 2)" and so on:
/// an existing file is never overwritten.
fn copy_path(input: &Path) -> Result<PathBuf> {
    let Some(ext) = input.extension().map(|e| e.to_string_lossy().into_owned()) else {
        bail!("o arquivo precisa ter extensão (ex.: .mp4) para identificar o formato");
    };
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video".into());
    let dir = input.parent().unwrap_or_else(|| Path::new("."));
    for n in 1..1000 {
        let tag = if n == 1 { "normalized".to_string() } else { format!("normalized {n}") };
        let candidate = dir.join(format!("{stem} ({tag}).{ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("já existem cópias normalizadas demais ao lado do arquivo original")
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
mod tests;
