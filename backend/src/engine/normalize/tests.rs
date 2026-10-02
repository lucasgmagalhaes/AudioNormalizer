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

/// Re-encodes `from` into `to` through our own bridge; the container is
/// chosen by the extension of `to` and the codec follows the source.
fn remux_audio(from: &Path, to: &Path) {
    let info = av::probe(from).unwrap();
    let mut decoder = PcmDecoder::open(from, &info, 0).unwrap();
    let mut remuxer = Remuxer::open(from, to, &info, "", false).unwrap();
    let mut block = Vec::new();
    while decoder.read(&mut block).unwrap() {
        remuxer.write(&block).unwrap();
    }
    remuxer.finish().unwrap();
    // Windows will not delete a file that is still open.
    drop(remuxer);
    drop(decoder);
}

/// Encodes a tone to AAC through our own bridge (WAV in, .m4a out), so
/// no ffmpeg executable is needed.
fn write_test_aac(path: &Path) {
    let wav = path.with_extension("fixture.wav");
    testsig::write_wav(&wav, 1, &testsig::tone(2.0, 1000.0, 0.25));
    remux_audio(&wav, path);
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
fn leveling_narrows_the_loudness_range_and_still_hits_the_target() {
    // Four cycles of 6 s loud (0.4) and 6 s quiet (0.08): 14 dB apart.
    let mut samples = Vec::new();
    for _ in 0..4 {
        samples.extend(testsig::tone(6.0, 440.0, 0.4));
        samples.extend(testsig::tone(6.0, 440.0, 0.08));
    }
    let path = std::env::temp_dir().join(format!("audio-normalizer-level-{}.wav", std::process::id()));
    testsig::write_wav(&path, 1, &samples);
    let targets = Targets { target_lufs: -16.0, true_peak_db: -1.0 };
    let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
    let measure = |file: &Path| analyze::run_track(targets, 0, file, &job).unwrap().report.measurement;

    let plain = run(targets, Options { output: OutputMode::Copy, ..Options::default() }, &path, None, None, &job).unwrap();
    let leveled_options = Options { output: OutputMode::Copy, leveling: true, ..Options::default() };
    let leveled = run(targets, leveled_options, &path, None, None, &job).unwrap();
    let (before, plain_after, after) =
        (measure(&path), measure(Path::new(&plain.output_path)), measure(Path::new(&leveled.output_path)));

    assert!(leveled.leveled && !plain.leveled);
    println!(
        "loudness range {:.1} LU -> {:.1} LU leveled ({:.1} LU plain); integrated {:.1} LUFS",
        before.loudness_range, after.loudness_range, plain_after.loudness_range, after.integrated_lufs
    );
    assert!((plain_after.loudness_range - before.loudness_range).abs() < 1.0, "plain gain must not change the range");
    assert!(
        after.loudness_range < before.loudness_range - 3.0,
        "range {:.1} -> {:.1} LU",
        before.loudness_range,
        after.loudness_range
    );
    assert!((after.integrated_lufs - targets.target_lufs).abs() < 1.0, "target missed: {:.1}", after.integrated_lufs);
    assert!(after.true_peak_db <= targets.true_peak_db + 0.5);
    for file in [path.as_path(), Path::new(&plain.output_path), Path::new(&leveled.output_path)] {
        fs::remove_file(file).unwrap();
    }
}

#[test]
fn lossless_saves_flac_when_the_container_allows_it() {
    let id = std::process::id();
    let m4a = std::env::temp_dir().join(format!("audio-normalizer-lossless-{id}.m4a"));
    let mkv = std::env::temp_dir().join(format!("audio-normalizer-lossless-{id}.mkv"));
    write_test_aac(&m4a);
    remux_audio(&m4a, &mkv); // AAC inside Matroska
    let targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };
    let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
    let copy = |lossless: bool| Options { output: OutputMode::Copy, lossless, ..Options::default() };

    let lossy = run(targets, copy(false), &mkv, None, None, &job).unwrap();
    let flac = run(targets, copy(true), &mkv, None, None, &job).unwrap();
    // An .m4a cannot hold FLAC: the request falls back to the source codec.
    let kept = run(targets, copy(true), &m4a, None, None, &job).unwrap();

    assert_eq!(lossy.output_media.codec, "aac");
    assert_eq!(flac.output_media.codec, "flac");
    assert_eq!(kept.output_media.codec, "aac");
    let measured = analyze::run_track(targets, 0, Path::new(&flac.output_path), &job).unwrap().report.measurement;
    assert!((measured.integrated_lufs - targets.target_lufs).abs() < 1.0, "FLAC output missed the target");
    assert!(measured.true_peak_db < targets.true_peak_db + 0.5);
    for file in [&m4a, &mkv, Path::new(&lossy.output_path), Path::new(&flac.output_path), Path::new(&kept.output_path)] {
        fs::remove_file(file).unwrap();
    }
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
    let mut chain = Chain::new(&info, 6.0, -1.5, None, CleanupSettings::default()).unwrap();
    let mut output = Vec::new();
    let mut block = vec![0.1; info.audio.sample_rate as usize];
    chain.process(&mut block, &mut output).unwrap();
    chain.flush(&mut output).unwrap();
    assert!(chain.result().unwrap().output_lufs.is_finite());

    let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
    let mut progress = job.stage("calibrate", 0.0, 1.0);
    let plan = Calibration { base_gain_db: 6.0, ceiling_db: -1.5, target_lufs: -14.0, leveler_reference: None, cleanup: CleanupSettings::default() };
    let gain = calibrate(&AudioSource::File(&path), &info, plan, &job, &mut progress).unwrap();
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

#[test]
fn cleanup_is_off_unless_asked_and_never_changes_the_duration() {
    // A 220 Hz tone clipped flat on every peak, riding on a DC offset.
    let samples: Vec<f32> = testsig::tone(8.0, 220.0, 1.2).iter().map(|s| (s * 0.8 + 0.05).clamp(-1.0, 1.0)).collect();
    let path = std::env::temp_dir().join(format!("audio-normalizer-cleanup-{}.wav", std::process::id()));
    testsig::write_wav(&path, 1, &samples);
    let targets = Targets { target_lufs: -16.0, true_peak_db: -1.0 };
    let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
    let copy = |cleanup: CleanupSettings| Options { output: OutputMode::Copy, cleanup, ..Options::default() };

    let plain = run(targets, copy(CleanupSettings::default()), &path, None, None, &job).unwrap();
    let both = CleanupSettings { highpass: true, declip: true };
    let cleaned = run(targets, copy(both), &path, None, None, &job).unwrap();

    assert_eq!(plain.declipped_samples, None);
    assert!(!plain.cleanup.any());
    assert!(cleaned.declipped_samples.unwrap() > 100, "clipped peaks were rebuilt");
    assert!((cleaned.output_media.duration - cleaned.input_media.duration).abs() < 0.01, "duration changed");
    assert_eq!(cleaned.output_media.sample_rate, cleaned.input_media.sample_rate);
    assert!((cleaned.output_lufs - targets.target_lufs).abs() < 1.0, "target missed: {:.1}", cleaned.output_lufs);
    assert!(cleaned.output_true_peak_db <= targets.true_peak_db + 0.5);
    for file in [path.as_path(), Path::new(&plain.output_path), Path::new(&cleaned.output_path)] {
        fs::remove_file(file).unwrap();
    }
}

#[test]
fn flushing_with_clean_up_keeps_every_frame() {
    let info = MediaInfo {
        duration: 3.0,
        has_video: false,
        audio: av::AudioInfo { codec: "pcm".into(), sample_rate: 48_000, channels: 2 },
        track: 0,
    };
    let cleanup = CleanupSettings { highpass: true, declip: true };
    let mut chain = Chain::new(&info, 0.0, -1.5, Some(-20.0), cleanup).unwrap();
    let (mut all, mut out) = (Vec::new(), Vec::new());
    let mut total = 0;
    for _ in 0..3 {
        // Ends on a clipped run so the declipper holds frames back.
        let mut block: Vec<f32> = (0..2 * 4800).map(|i| if i < 2 * 4790 { 0.2 } else { 1.0 }).collect();
        total += block.len();
        chain.process(&mut block, &mut out).unwrap();
        all.extend_from_slice(&out);
    }
    out.clear();
    chain.flush(&mut out).unwrap();
    all.extend_from_slice(&out);
    assert_eq!(all.len(), total);
}
