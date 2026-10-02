//! Performance benchmarks over a real media file (ignored by default).
//!
//! `bench_components` times each stage in isolation; `bench_pipeline` runs a
//! full analyze + normalize and reports per-stage wall time. The pipeline
//! benchmark modifies the file in place, so point it at a throwaway copy:
//!
//! BENCH_FILE=/tmp/copy.mp4 cargo test --release bench_ -- --ignored --nocapture
#![cfg(test)]

use super::av::{self, PcmDecoder};
use super::job::Job;
use super::limiter::Limiter;
use super::{analyze, normalize, Targets};
use ebur128::{EbuR128, Mode};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}

#[test]
#[ignore]
fn bench_components() {
    let path = env("BENCH_FILE");
    let path = Path::new(&path);
    let info = av::probe(path).unwrap();
    let ch = info.audio.channels;
    let sr = info.audio.sample_rate;
    println!("file {:.0}s, {} ch, {} Hz", info.duration, ch, sr);

    // 1. demux + decode the whole file.
    let t = Instant::now();
    let mut dec = PcmDecoder::open(path, &info, 0).unwrap();
    let mut block = Vec::new();
    let mut pcm: Vec<f32> = Vec::new();
    let keep = (sr as usize) * (ch as usize) * 300; // first 5 minutes kept in RAM
    let mut frames = 0u64;
    while dec.read(&mut block).unwrap() {
        frames += (block.len() / ch as usize) as u64;
        if pcm.len() < keep {
            pcm.extend_from_slice(&block);
        }
    }
    let full = t.elapsed().as_secs_f64();
    let scale = frames as f64 / (pcm.len() / ch as usize) as f64;
    println!("decode full file            {:>7.2}s", full);

    // 2. DSP pieces on in-memory PCM, extrapolated to the whole file.
    let run = |name: &str, f: &dyn Fn(&[f32])| {
        let t = Instant::now();
        f(&pcm);
        println!("{name:<28}{:>7.2}s (extrapolated)", t.elapsed().as_secs_f64() * scale);
    };
    let meter = |mode: Mode| {
        move |pcm: &[f32]| {
            let mut m = EbuR128::new(ch, sr, mode).unwrap();
            for c in pcm.chunks(8192 * ch as usize) {
                m.add_frames_f32(c).unwrap();
            }
            std::hint::black_box(m.loudness_global().unwrap());
        }
    };
    run("ebur128 I", &meter(Mode::I));
    run("ebur128 I|LRA", &meter(Mode::I | Mode::LRA));
    run("ebur128 SAMPLE_PEAK", &meter(Mode::I | Mode::SAMPLE_PEAK));
    run("ebur128 TRUE_PEAK", &meter(Mode::I | Mode::TRUE_PEAK));
    run("ebur128 analysis (all)", &meter(Mode::I | Mode::LRA | Mode::TRUE_PEAK | Mode::SAMPLE_PEAK));
    run("analysis loudness thread", &|pcm: &[f32]| {
        let mut m = EbuR128::new(ch, sr, Mode::I | Mode::LRA | Mode::SAMPLE_PEAK | Mode::M | Mode::S).unwrap();
        let step = (sr as usize / 10) * ch as usize;
        let mut acc = 0.0;
        for c in pcm.chunks(step) {
            m.add_frames_f32(c).unwrap();
            acc += m.loudness_shortterm().unwrap_or(0.0) + m.loudness_momentary().unwrap_or(0.0);
        }
        std::hint::black_box((acc, m.loudness_global().unwrap(), m.loudness_range().unwrap()));
    });
    for (name, momentary, short) in [("momentary only", true, false), ("short-term only", false, true)] {
        run(&format!("  loudness {name}"), &move |pcm: &[f32]| {
            let mut m = EbuR128::new(ch, sr, Mode::I | Mode::M | Mode::S).unwrap();
            let step = (sr as usize / 10) * ch as usize;
            let mut acc = 0.0;
            for c in pcm.chunks(step) {
                m.add_frames_f32(c).unwrap();
                if momentary {
                    acc += m.loudness_momentary().unwrap_or(0.0);
                }
                if short {
                    acc += m.loudness_shortterm().unwrap_or(0.0);
                }
            }
            std::hint::black_box(acc);
        });
    }
    run("analysis true peak (1 ch)", &|pcm: &[f32]| {
        let mut m = EbuR128::new(1, sr, Mode::TRUE_PEAK).unwrap();
        let mono: Vec<f32> = pcm.iter().step_by(ch as usize).copied().collect();
        for c in mono.chunks(8192) {
            m.add_frames_f32(c).unwrap();
        }
        std::hint::black_box(m.true_peak(0).unwrap());
    });
    run("gain + limiter", &|pcm: &[f32]| {
        let mut lim = Limiter::new(ch as usize, sr, 0.8);
        let mut out = Vec::new();
        let mut buf = Vec::new();
        for c in pcm.chunks(8192 * ch as usize) {
            buf.clear();
            buf.extend(c.iter().map(|s| s * 3.0));
            lim.process(&buf, &mut out);
            std::hint::black_box(&out);
        }
    });
}

#[test]
#[ignore]
fn bench_pipeline() {
    let path = env("BENCH_FILE");
    let path = Path::new(&path);
    let targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };
    let marks: Arc<Mutex<Vec<(&'static str, Instant)>>> = Arc::default();
    let sink = marks.clone();
    let job = Job::new(Arc::new(AtomicBool::new(false)), move |e| {
        let mut m = sink.lock().unwrap();
        if m.last().map(|(s, _)| *s) != Some(e.stage) {
            m.push((e.stage, Instant::now()));
        }
    });
    let t0 = Instant::now();
    let analysis = analyze::run(targets, path, &job).unwrap();
    let t1 = Instant::now();
    marks.lock().unwrap().clear();
    let measured = Some(analysis.report.measurement);
    let cache = analysis.cache.map(Arc::new);
    println!("cached audio                {:>7.1} MB", cache.as_ref().map_or(0.0, |c| c.bytes() as f64 / 1e6));
    let report = normalize::run(targets, normalize::Options::default(), path, measured, cache, &job).unwrap();
    let t2 = Instant::now();
    println!("analyze total               {:>7.2}s", (t1 - t0).as_secs_f64());
    let m = marks.lock().unwrap();
    for (i, (stage, at)) in m.iter().enumerate() {
        let end = m.get(i + 1).map(|(_, e)| *e).unwrap_or(t2);
        println!("  normalize/{stage:<16}{:>7.2}s", (end - *at).as_secs_f64());
    }
    println!("normalize total             {:>7.2}s  ({:.1} -> {:.1} LUFS, TP {:.1})",
        (t2 - t1).as_secs_f64(), report.input_lufs, report.output_lufs, report.output_true_peak_db);
}

#[test]
#[ignore]
fn bench_analyze() {
    let path = env("BENCH_FILE");
    let targets = Targets { target_lufs: -14.0, true_peak_db: -1.0 };
    let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});
    for _ in 0..3 {
        let t = Instant::now();
        let report = analyze::run(targets, Path::new(&path), &job).unwrap().report;
        println!(
            "analyze                     {:>7.2}s  (I {:.2}, TP {:.2}, LRA {:.2})",
            t.elapsed().as_secs_f64(),
            report.measurement.integrated_lufs,
            report.measurement.true_peak_db,
            report.measurement.loudness_range
        );
    }
}
