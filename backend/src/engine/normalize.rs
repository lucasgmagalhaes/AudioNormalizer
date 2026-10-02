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
use super::job::{Cancelled, Job, Progress, StageStopped, QUEUE_BLOCKS};
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
    pub input_lufs: f64,
    pub input_true_peak_db: f64,
    pub output_lufs: f64,
    pub output_true_peak_db: f64,
    pub target_lufs: f64,
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
        let done = normalize_track(targets, track, input, current, pass, measured, cache, &window)?;
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
    let (input_media, output_media) = (media_details(&done.info), media_details(&done.encoded.media));

    // Only the last pass output survives; earlier ones are deleted on drop.
    let last = finished.pop().expect("at least one track pass");
    let output_path = finish_output(last.temp, input, options.output)?;

    Ok(NormalizeReport {
        path: input.display().to_string(),
        output_path: output_path.display().to_string(),
        replaced: options.output == OutputMode::Replace,
        tracks_processed,
        input_lufs: measurement.integrated_lufs,
        input_true_peak_db: measurement.true_peak_db,
        output_lufs,
        output_true_peak_db,
        target_lufs: targets.target_lufs,
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
    if predicted_limiting > CALIBRATE_ABOVE_DB && gain_db < MAX_GAIN_DB {
        let mut progress = job.stage("calibrate", cursor, 15.0);
        cursor += 15.0;
        gain_db = calibrate(&source, &info, gain_db, ceiling_db, targets.target_lufs, job, &mut progress)?;
    }

    let settings = EncodeSettings { gain_db, ceiling_db, encoder_options: "" };
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
    encoder_options: &'a str,
}

/// What ended up in the written file.
struct Encoded {
    encoder: String,
    output_lufs: f64,
    true_peak_db: f64,
    limiter_reduction_db: f64,
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
    let remuxer = Remuxer::open(input, output, info, settings.encoder_options)?;
    let encoder = remuxer.encoder();
    let monitor = remuxer.monitor_format();
    let decoder = source.open(info)?;
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
        media: written,
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

/// Dry run that finds the gain which, after limiting, lands on `target`.
/// One decode feeds a chain per candidate gain, each on its own thread; the
/// answer is interpolated between the candidates that bracket the target.
fn calibrate(
    source: &AudioSource,
    info: &MediaInfo,
    base_gain_db: f64,
    ceiling_db: f64,
    target_lufs: f64,
    job: &Job,
    progress: &mut Progress,
) -> Result<f64> {
    let mut gains: Vec<f64> = CALIBRATION_STEPS_DB
        .iter()
        .map(|step| (base_gain_db + step).min(MAX_GAIN_DB))
        .collect();
    gains.dedup();
    let chains = gains
        .iter()
        .map(|&gain| Chain::new(info, gain, ceiling_db))
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
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn interpolates_calibration_gain() {
        let gains = [6.0, 7.5, 9.0];
        let loudness = [-16.0, -15.0, -13.5];
        assert_eq!(interpolate_gain(&gains, &loudness, -16.5), 6.0);
        assert!((interpolate_gain(&gains, &loudness, -15.5) - 6.75).abs() < 1e-9);
        assert!((interpolate_gain(&gains, &loudness, -14.0) - 8.5).abs() < 1e-9);
        assert_eq!(interpolate_gain(&gains, &loudness, -12.0), 9.0);
    }

    fn write_test_wav(path: &Path) {
        let rate = 48_000u32;
        let frames = rate * 2;
        let samples: Vec<i16> = (0..frames)
            .map(|i| ((i as f32 * std::f32::consts::TAU * 1_000.0 / rate as f32).sin() * 3_276.0) as i16)
            .collect();
        let data_len = (samples.len() * 2) as u32;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples { wav.extend_from_slice(&sample.to_le_bytes()); }
        fs::write(path, wav).unwrap();
    }

    /// Encodes a tone to AAC through our own bridge (WAV in, .m4a out), so
    /// no ffmpeg executable is needed.
    fn write_test_aac(path: &Path) {
        let wav = path.with_extension("fixture.wav");
        testsig::write_wav(&wav, 1, &testsig::tone(2.0, 1000.0, 0.25));
        let info = av::probe(&wav).unwrap();
        let mut decoder = PcmDecoder::open(&wav, &info, 0).unwrap();
        let mut remuxer = Remuxer::open(&wav, path, &info, "").unwrap();
        let mut block = Vec::new();
        while decoder.read(&mut block).unwrap() {
            remuxer.write(&block).unwrap();
        }
        remuxer.finish().unwrap();
        // Windows will not delete a file that is still open.
        drop(remuxer);
        drop(decoder);
        fs::remove_file(wav).unwrap();
    }

    #[test]
    fn e2e_normalizes_generated_wav() {
        let path = std::env::temp_dir().join(format!("audio-normalizer-{}.wav", std::process::id()));
        write_test_wav(&path);
        let targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };
        let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
        let analysis = analyze::run(targets, &path, &job).unwrap();
        let cache = analysis.cache.map(Arc::new).unwrap();
        assert!(cache.bytes() > 0);
        let before = analysis.report;
        let report = run(targets, Options::default(), &path, Some(before.measurement), Some(cache), &job).unwrap();
        let after = analyze::run(targets, &path, &job).unwrap().report;
        assert!((after.measurement.integrated_lufs - before.assessment.expected_lufs).abs() < 1.0);
        assert!(after.measurement.true_peak_db < targets.true_peak_db + 0.5);
        assert!((after.media.duration - before.media.duration).abs() < 0.5);
        assert_eq!(report.path, path.display().to_string());
        assert_eq!(report.input_media.channels, report.output_media.channels);
        assert_eq!(report.input_media.has_video, report.output_media.has_video);
        assert!((report.input_media.duration - report.output_media.duration).abs() < 0.5);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn e2e_normalizes_and_calibrates_without_preanalysis_cache() {
        let path = std::env::temp_dir().join(format!("audio-normalizer-uncached-{}.wav", std::process::id()));
        write_test_wav(&path);
        let targets = Targets { target_lufs: -5.0, true_peak_db: -9.0 };
        let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});

        let report = run(targets, Options::default(), &path, None, None, &job).unwrap();

        assert!(report.gain_db.is_finite());
        assert!(report.limiter_max_reduction_db >= 0.0);
        assert_eq!(report.input_media.codec, report.output_media.codec);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn e2e_normalizes_aac_input() {
        let path = std::env::temp_dir().join(format!("audio-normalizer-aac-{}.m4a", std::process::id()));
        write_test_aac(&path);
        let targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };
        let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});

        let report = run(targets, Options::default(), &path, None, None, &job).unwrap();

        assert_eq!(report.input_media.codec, "aac");
        assert_eq!(report.output_media.codec, "aac");
        fs::remove_file(path).unwrap();
    }

    /// Loudness of one audio track of `path`.
    fn track_lufs(path: &Path, track: usize) -> f64 {
        let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
        let targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };
        analyze::run_track(targets, track, path, &job).unwrap().report.measurement.integrated_lufs
    }

    #[test]
    fn lists_audio_tracks_with_their_metadata() {
        let path = std::env::temp_dir().join(format!("audio-normalizer-list-{}.mkv", std::process::id()));
        av::write_test_tracks(&path, &[0.1, 0.05, 0.2], 1.0, 48_000).unwrap();

        let tracks = av::audio_tracks(&path).unwrap();

        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[1].index, 1);
        assert_eq!((tracks[0].language.as_str(), tracks[1].language.as_str()), ("eng", "por"));
        assert!(tracks.iter().all(|t| t.channels == 1 && t.sample_rate == 48_000));
        assert!(av::probe_track(&path, 3).is_err(), "a missing track must be an error");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn normalizes_only_the_chosen_audio_track() {
        let path = std::env::temp_dir().join(format!("audio-normalizer-pick-{}.mkv", std::process::id()));
        av::write_test_tracks(&path, &[0.05, 0.02], 8.0, 48_000).unwrap();
        let (first, second) = (track_lufs(&path, 0), track_lufs(&path, 1));
        let targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };
        let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});

        let options = Options { track: 1, ..Options::default() };
        let report = run(targets, options, &path, None, None, &job).unwrap();

        assert_eq!(report.tracks_processed, 1);
        assert!((track_lufs(&path, 1) - targets.target_lufs).abs() < 1.0, "the chosen track reaches the target");
        assert!((track_lufs(&path, 0) - first).abs() < 0.3, "the other track is left alone");
        assert!(second < targets.target_lufs - 5.0, "the fixture is quiet enough to need gain");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn normalizes_every_audio_track_when_asked() {
        let path = std::env::temp_dir().join(format!("audio-normalizer-all-{}.mkv", std::process::id()));
        av::write_test_tracks(&path, &[0.05, 0.02, 0.1], 8.0, 48_000).unwrap();
        let targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };
        let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});

        let options = Options { all_tracks: true, ..Options::default() };
        let report = run(targets, options, &path, None, None, &job).unwrap();

        assert_eq!(report.tracks_processed, 3);
        for track in 0..3 {
            assert!((track_lufs(&path, track) - targets.target_lufs).abs() < 1.0, "track {track}");
        }
        assert_eq!(av::audio_tracks(&path).unwrap().len(), 3, "no track may be lost");
        let leftovers = fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".") && name.contains("audio-normalizer-all"))
            .count();
        assert_eq!(leftovers, 0, "temporary pass files must be cleaned up");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn copy_paths_are_numbered_and_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("audio-normalizer-copies-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let input = dir.join("Clip.MP4");
        assert_eq!(copy_path(&input).unwrap(), dir.join("Clip (normalized).MP4"));
        fs::write(dir.join("Clip (normalized).MP4"), b"x").unwrap();
        assert_eq!(copy_path(&input).unwrap(), dir.join("Clip (normalized 2).MP4"));
        assert!(copy_path(Path::new("no-extension")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn e2e_copy_keeps_the_original_untouched() {
        let path = std::env::temp_dir().join(format!("audio-normalizer-copy-{}.wav", std::process::id()));
        write_test_wav(&path);
        let original = fs::read(&path).unwrap();
        let targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };
        let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
        let options = Options { output: OutputMode::Copy, ..Options::default() };

        let first = run(targets, options, &path, None, None, &job).unwrap();
        let second = run(targets, options, &path, None, None, &job).unwrap();

        assert!(!first.replaced);
        assert_eq!(fs::read(&path).unwrap(), original, "the source file must not change");
        assert_ne!(first.output_path, second.output_path, "a second copy must not overwrite the first");
        let measured = analyze::run(targets, Path::new(&first.output_path), &job).unwrap().report.measurement;
        assert!((measured.integrated_lufs - targets.target_lufs).abs() < 1.0);
        assert!(first.size_after > 0);
        fs::remove_file(&path).unwrap();
        fs::remove_file(&first.output_path).unwrap();
        fs::remove_file(&second.output_path).unwrap();
    }

    #[test]
    fn temp_paths_are_hidden_and_require_an_extension() {
        assert!(temp_path(Path::new("input"), "normalizing").is_err());
        assert_eq!(
            temp_path(Path::new("C:/media/clip.MP4"), "normalizing").unwrap(),
            PathBuf::from("C:/media/.clip.normalizing.mp4"),
        );
    }

    #[test]
    fn describes_media_and_rejects_invalid_encoded_output() {
        let path = std::env::temp_dir().join(format!("audio-normalizer-verify-{}.wav", std::process::id()));
        write_test_wav(&path);
        let written = av::probe(&path).unwrap();

        let details = media_details(&written);
        assert_eq!(details.codec, written.audio.codec);
        assert_eq!(details.sample_rate, 48_000);
        assert_eq!(details.channels, 1);
        assert!(!details.has_video);
        assert!(verify(&path, &MediaInfo { has_video: true, ..written.clone() }).is_err());
        assert!(verify(&path, &MediaInfo { duration: 100.0, ..written }).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn chain_and_calibration_process_decoded_audio() {
        let path = std::env::temp_dir().join(format!("audio-normalizer-calibrate-{}.wav", std::process::id()));
        write_test_wav(&path);
        let info = av::probe(&path).unwrap();
        let mut chain = Chain::new(&info, 6.0, -1.5).unwrap();
        let mut output = Vec::new();
        let mut block = vec![0.1; info.audio.sample_rate as usize];
        chain.process(&mut block, &mut output).unwrap();
        chain.flush(&mut output).unwrap();
        assert!(chain.result().unwrap().output_lufs.is_finite());

        let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
        let mut progress = job.stage("calibrate", 0.0, 1.0);
        let gain = calibrate(&AudioSource::File(&path), &info, 6.0, -1.5, -14.0, &job, &mut progress).unwrap();
        assert!((6.0..=MAX_GAIN_DB).contains(&gain));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn temporary_file_is_removed_when_not_replaced() {
        let path = std::env::temp_dir().join(format!("audio-normalizer-temp-{}.tmp", std::process::id()));
        fs::write(&path, b"temporary").unwrap();
        drop(TempFile(path.clone()));
        assert!(!path.exists());
    }

    #[test]
    fn replacement_is_atomic_and_missing_output_fails_verification() {
        let base = std::env::temp_dir().join(format!("audio-normalizer-replace-{}", std::process::id()));
        let original = base.with_extension("wav");
        let replacement = base.with_extension("tmp");
        fs::write(&original, b"old").unwrap();
        fs::write(&replacement, b"new").unwrap();
        replace_original(TempFile(replacement), &original).unwrap();
        assert_eq!(fs::read(&original).unwrap(), b"new");
        let info = MediaInfo { duration: 1.0, has_video: false, audio: av::AudioInfo { codec: "pcm".into(), sample_rate: 48_000, channels: 1 }, track: 0 };
        assert!(verify(&base.with_extension("missing.wav"), &info).is_err());
        fs::remove_file(original).unwrap();
    }
    use super::super::testsig;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// End-to-end run over real media. The files are modified in place, so
    /// point NORMALIZER_E2E_FILES (`;`-separated) at throwaway copies.
    #[test]
    #[ignore]
    #[cfg(not(coverage))]
    fn e2e_normalize_files() {
        let files = std::env::var("NORMALIZER_E2E_FILES").expect("set NORMALIZER_E2E_FILES");
        let targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };
        for file in files.split(';').filter(|f| !f.is_empty()) {
            let path = Path::new(file);
            let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
            let before = analyze::run(targets, path, &job).unwrap_or_else(|e| panic!("{file}: {e:#}")).report;
            let report = run(targets, Options::default(), path, None, None, &job).unwrap_or_else(|e| panic!("{file}: {e:#}"));
            let after = analyze::run(targets, path, &job).unwrap_or_else(|e| panic!("{file}: {e:#}")).report;
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
