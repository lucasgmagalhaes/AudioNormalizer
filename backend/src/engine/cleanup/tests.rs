//! Tests of the optional audio clean-up stages.

use super::*;
use crate::engine::testsig;

/// Runs `signal` through `cleanup` in blocks of `block` frames.
fn run(cleanup: &mut Cleanup, signal: &[f32], channels: usize, block: usize) -> Vec<f32> {
    let mut out = Vec::new();
    for chunk in signal.chunks(block * channels) {
        cleanup.process(chunk, &mut out);
    }
    cleanup.flush(&mut out);
    out
}

fn only(settings: CleanupSettings) -> Cleanup {
    Cleanup::new(settings, 1, testsig::RATE)
}

fn declip_only() -> CleanupSettings {
    CleanupSettings { declip: true, ..CleanupSettings::default() }
}

fn highpass_only() -> CleanupSettings {
    CleanupSettings { highpass: true, ..CleanupSettings::default() }
}

/// Loud enough to clip, short enough (about 40 samples a peak) to be repairable.
const CLIPPED_AMPLITUDE: f32 = 1.2;

fn clipped_tone(seconds: f64, hz: f64) -> Vec<f32> {
    testsig::tone(seconds, hz, CLIPPED_AMPLITUDE).iter().map(|s| s.clamp(-1.0, 1.0)).collect()
}

#[test]
fn nothing_is_on_by_default() {
    assert!(!CleanupSettings::default().any());
    let signal = testsig::tone(1.0, 440.0, 0.3);
    let mut off = only(CleanupSettings::default());
    assert_eq!(run(&mut off, &signal, 1, 1000), signal);
    assert_eq!(off.declipped_samples(), None);
}

#[test]
fn the_length_never_changes_whatever_the_block_size() {
    let signal = clipped_tone(1.0, 220.0);
    let both = CleanupSettings { highpass: true, declip: true };
    for block in [1, 7, 100, 4096, 100_000] {
        let out = run(&mut only(both), &signal, 1, block);
        assert_eq!(out.len(), signal.len(), "block size {block}");
    }
    let stereo = testsig::stereo(&signal, &signal);
    let out = run(&mut Cleanup::new(both, 2, testsig::RATE), &stereo, 2, 333);
    assert_eq!(out.len(), stereo.len());
}

#[test]
fn clipped_peaks_are_rebuilt_above_the_clip_level() {
    let original = testsig::tone(1.0, 220.0, CLIPPED_AMPLITUDE);
    let clipped = clipped_tone(1.0, 220.0);
    let mut cleanup = only(declip_only());

    let out = run(&mut cleanup, &clipped, 1, 4096);

    let peak = out.iter().fold(0.0f32, |p, s| p.max(s.abs()));
    assert!(peak > 1.1, "peak {peak}");
    assert!(cleanup.declipped_samples().unwrap() > 100);
    let error = |signal: &[f32]| signal.iter().zip(&original).map(|(a, b)| f64::from((a - b).abs())).sum::<f64>();
    assert!(error(&out) < error(&clipped) * 0.5, "rebuilt {} vs clipped {}", error(&out), error(&clipped));
}

#[test]
fn a_run_that_spans_two_blocks_is_rebuilt_the_same_way() {
    let clipped = clipped_tone(1.0, 220.0);
    let whole = run(&mut only(declip_only()), &clipped, 1, 1_000_000);
    for block in [1, 5, 37, 1000] {
        assert_eq!(run(&mut only(declip_only()), &clipped, 1, block), whole, "block size {block}");
    }
}

#[test]
fn audio_without_clipping_is_untouched() {
    let signal = testsig::tone(1.0, 440.0, 0.8);
    let mut cleanup = only(declip_only());
    assert_eq!(run(&mut cleanup, &signal, 1, 999), signal);
    assert_eq!(cleanup.declipped_samples(), Some(0));
}

#[test]
fn two_samples_at_the_ceiling_are_not_clipping() {
    let signal = [0.0, 0.5, 0.99, 1.0, 0.6, 0.2, 0.0];
    assert_eq!(run(&mut only(declip_only()), &signal, 1, 3), signal);
}

#[test]
fn channels_are_repaired_independently() {
    let clipped = clipped_tone(0.5, 220.0);
    let quiet = testsig::tone(0.5, 330.0, 0.2);
    let stereo = testsig::stereo(&clipped, &quiet);
    let mut cleanup = Cleanup::new(declip_only(), 2, testsig::RATE);

    let out = run(&mut cleanup, &stereo, 2, 500);

    let right: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
    assert_eq!(right, quiet, "the clean channel must not change");
    assert!(out.iter().step_by(2).fold(0.0f32, |p, s| p.max(s.abs())) > 1.1);
}

#[test]
fn the_high_pass_removes_dc_and_keeps_the_audible_range() {
    let tone = testsig::tone(4.0, 1000.0, 0.3);
    let offset: Vec<f32> = tone.iter().map(|s| s + 0.2).collect();

    let out = run(&mut only(highpass_only()), &offset, 1, 4096);

    let settled = &out[out.len() / 2..];
    let mean = settled.iter().map(|s| f64::from(*s)).sum::<f64>() / settled.len() as f64;
    assert!(mean.abs() < 0.001, "DC left: {mean}");
    let rms = |s: &[f32]| (s.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / s.len() as f64).sqrt();
    let kept = rms(settled) / rms(&tone[tone.len() / 2..]);
    assert!((kept - 1.0).abs() < 0.01, "1 kHz changed by {kept}");
}

#[test]
fn the_high_pass_cuts_rumble() {
    let rumble = testsig::tone(6.0, 5.0, 0.5);

    let out = run(&mut only(highpass_only()), &rumble, 1, 4096);

    let peak = out[out.len() / 2..].iter().fold(0.0f32, |p, s| p.max(s.abs()));
    assert!(peak < 0.15, "5 Hz still at {peak}");
}
