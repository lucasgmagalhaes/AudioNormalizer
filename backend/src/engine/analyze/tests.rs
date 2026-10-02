use super::*;

const TARGETS: Targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };

fn measurement(i: f64, tp: f64) -> Measurement {
    Measurement {
        integrated_lufs: i,
        loudness_range: 6.0,
        true_peak_db: tp,
        sample_peak_db: tp - 0.5,
        max_short_term_lufs: None,
        max_momentary_lufs: None,
        stereo_correlation: None,
    }
}

#[test]
fn on_target_needs_nothing() {
    let a = assess(&measurement(-14.2, -3.0), TARGETS);
    assert_eq!(a.verdict, Verdict::None);
    assert!((a.gain_db - 0.2).abs() < 1e-9);
    assert_eq!(a.limiter_reduction_db, 0.0);
}

#[test]
fn quiet_audio_gets_gain_and_limiting() {
    let a = assess(&measurement(-26.0, -4.0), TARGETS);
    assert_eq!(a.verdict, Verdict::Large);
    assert!((a.gain_db - 12.0).abs() < 1e-9);
    assert!((a.limiter_reduction_db - 9.0).abs() < 1e-9);
}

#[test]
fn gain_is_capped() {
    let a = assess(&measurement(-60.0, -30.0), TARGETS);
    assert!(a.gain_capped);
    assert_eq!(a.gain_db, MAX_GAIN_DB);
}

#[test]
fn hot_peaks_alone_are_an_improvement() {
    let a = assess(&measurement(-14.0, 0.5), TARGETS);
    assert!(a.peak_over_ceiling);
    assert_ne!(a.verdict, Verdict::None);
}

#[test]
fn classifies_small_and_moderate_improvements() {
    assert_eq!(assess(&measurement(-15.0, -3.0), TARGETS).verdict, Verdict::Small);
    assert_eq!(assess(&measurement(-17.5, -3.0), TARGETS).verdict, Verdict::Moderate);
}

#[test]
fn the_loudest_window_ignores_values_that_are_not_ready() {
    assert_eq!(higher(None, Some(f64::NEG_INFINITY)), None);
    assert_eq!(higher(None, Some(-20.0)), Some(-20.0));
    assert_eq!(higher(Some(-20.0), Some(-18.0)), Some(-18.0));
    assert_eq!(higher(Some(-20.0), Some(-25.0)), Some(-20.0));
    assert_eq!(higher(Some(-20.0), None), Some(-20.0));
}

#[test]
fn rejects_a_silent_meter() {
    let meter = EbuR128::new(1, 48_000, Mode::I | Mode::LRA | Mode::TRUE_PEAK | Mode::SAMPLE_PEAK).unwrap();
    assert!(read_measurement(&meter, 1).is_err());
}

/// Our measurement must agree with FFmpeg's own `ebur128` filter, run
/// in-process through the bridge (no ffmpeg process), on signals that
/// exercise channel weighting, noise, gating and loudness range.
#[test]
fn measurement_matches_the_ffmpeg_ebur128_reference() {
    let signals: [(&str, u16, Vec<f32>); 4] = [
        ("tone", 1, testsig::tone(12.0, 1000.0, 0.25)),
        (
            "stereo-tones",
            2,
            testsig::stereo(&testsig::tone(12.0, 1000.0, 0.25), &testsig::tone(12.0, 440.0, 0.1)),
        ),
        ("pink-noise", 1, testsig::pink_noise(12.0, 0.2, 7)),
        ("bursts-and-silence", 1, testsig::bursts(12.0)),
    ];
    for (name, channels, samples) in signals {
        let path = std::env::temp_dir().join(format!("audio-normalizer-ref-{name}-{}.wav", std::process::id()));
        testsig::write_wav(&path, channels, &samples);
        let expected = av::reference_loudness(&path).unwrap();
        let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
        let measured = run(TARGETS, &path, &job).unwrap().report.measurement;
        let _ = std::fs::remove_file(&path);

        let near = |label: &str, ours: f64, theirs: f64, tolerance: f64| {
            assert!(
                (ours - theirs).abs() <= tolerance,
                "{name}: {label} ours {ours:.2} vs FFmpeg {theirs:.2} (tolerance {tolerance})"
            );
        };
        near("integrated loudness", measured.integrated_lufs, expected.integrated, 0.1);
        // EBU Tech 3342 allows +-1 LU on loudness range; short synthetic
        // signals have few short-term blocks, so the percentiles move.
        near("loudness range", measured.loudness_range, expected.range, 1.0);
        near("true peak", measured.true_peak_db, linear_to_db(expected.true_peak), 0.3);
        // The reference reads at frame boundaries, we read every 100 ms.
        near("max momentary", measured.max_momentary_lufs.unwrap(), expected.max_momentary, 0.5);
        near("max short-term", measured.max_short_term_lufs.unwrap(), expected.max_short_term, 0.5);
    }
}

use super::super::testsig;
use std::sync::atomic::AtomicBool;

#[test]
fn stereo_correlation_tells_in_phase_from_opposite_phase() {
    let tone = testsig::tone(1.0, 440.0, 0.3);
    let inverted: Vec<f32> = tone.iter().map(|s| -s).collect();
    let correlation = |left: &[f32], right: &[f32]| {
        let mut meter = PhaseMeter::default();
        meter.add(&testsig::stereo(left, right));
        meter.correlation()
    };

    assert!((correlation(&tone, &tone).unwrap() - 1.0).abs() < 1e-6);
    assert!((correlation(&tone, &inverted).unwrap() + 1.0).abs() < 1e-6);
    assert_eq!(correlation(&tone, &vec![0.0; tone.len()]), None, "a silent channel has no correlation");
}
