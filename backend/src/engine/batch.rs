//! Normalizes several files at once.
//!
//! One file keeps a single core busy for most of its time (the audio encoder
//! is serial), so a queue is worked by a few files in parallel. Every file
//! still goes through the exact same pipeline as a lone one; only the
//! scheduling differs.

use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use super::job::{Cancelled, Job, ProgressEvent};
use super::normalize::{self, NormalizeReport, Options};
use super::Targets;

/// Most files worked on at once: each one also runs several threads of its
/// own while it analyzes, and the disk is shared.
const MAX_PARALLEL_FILES: usize = 4;
/// Cores that one file in flight is given.
const CORES_PER_FILE: usize = 4;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum Outcome {
    Running,
    Done { report: Box<NormalizeReport> },
    Failed { message: String },
    /// Cancelled before it finished, or never started because of that.
    Skipped,
}

/// What happened to the `index`-th file of the batch.
#[derive(Debug, Clone, Serialize)]
pub struct FileEvent {
    pub index: usize,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// How many files are worked on at the same time.
fn parallelism(files: usize) -> usize {
    let cores = thread::available_parallelism().map_or(1, usize::from);
    (cores / CORES_PER_FILE).clamp(1, MAX_PARALLEL_FILES).min(files.max(1))
}

/// Overall progress of the batch: every file counts the same.
struct Overall {
    percents: Mutex<Vec<f64>>,
    report: Box<dyn Fn(ProgressEvent) + Send + Sync>,
}

impl Overall {
    fn update(&self, index: usize, stage: &'static str, percent: f64) {
        let mut percents = self.percents.lock().unwrap();
        percents[index] = percent.clamp(0.0, 100.0);
        let mean = percents.iter().sum::<f64>() / percents.len() as f64;
        (self.report)(ProgressEvent { stage, percent: (mean * 10.0).round() / 10.0 });
    }
}

/// Normalizes `paths` with `options`, reporting each file's outcome through
/// `on_file` and the overall progress through `report`. A file that fails
/// does not stop the others; cancelling `job` skips what has not finished.
pub fn run(
    paths: &[PathBuf],
    targets: Targets,
    options: Options,
    job: &Job,
    on_file: impl Fn(FileEvent) + Sync,
    report: impl Fn(ProgressEvent) + Send + Sync + 'static,
) {
    let overall = Arc::new(Overall { percents: Mutex::new(vec![0.0; paths.len()]), report: Box::new(report) });
    let next = AtomicUsize::new(0);

    thread::scope(|s| {
        for _ in 0..parallelism(paths.len()) {
            s.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::SeqCst);
                let Some(path) = paths.get(index) else {
                    return;
                };
                let outcome = if job.check_cancelled().is_err() {
                    Outcome::Skipped
                } else {
                    on_file(FileEvent { index, outcome: Outcome::Running });
                    let progress = overall.clone();
                    let file_job = job.fork(move |event| progress.update(index, event.stage, event.percent));
                    match normalize::run(targets, options, path, None, None, &file_job) {
                        Ok(report) => Outcome::Done { report: Box::new(report) },
                        Err(e) if e.is::<Cancelled>() => Outcome::Skipped,
                        Err(e) => Outcome::Failed { message: format!("{e:#}") },
                    }
                };
                overall.update(index, "finalize", 100.0);
                on_file(FileEvent { index, outcome });
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::normalize::OutputMode;
    use crate::engine::testsig;
    use std::sync::atomic::AtomicBool;

    const TARGETS: Targets = Targets { target_lufs: -20.0, true_peak_db: -2.0 };

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("audio-normalizer-batch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wavs(dir: &std::path::Path, amplitudes: &[f32]) -> Vec<PathBuf> {
        amplitudes
            .iter()
            .enumerate()
            .map(|(i, &amplitude)| {
                let path = dir.join(format!("clip{i}.wav"));
                testsig::write_wav(&path, 1, &testsig::tone(4.0, 440.0, amplitude));
                path
            })
            .collect()
    }

    fn collect(paths: &[PathBuf], options: Options, job: &Job) -> (Vec<FileEvent>, Vec<ProgressEvent>) {
        let events = Mutex::new(Vec::new());
        let progress: Arc<Mutex<Vec<ProgressEvent>>> = Arc::default();
        let sink = progress.clone();
        run(paths, TARGETS, options, job, |e| events.lock().unwrap().push(e), move |p| sink.lock().unwrap().push(p));
        let progress = progress.lock().unwrap().clone();
        (events.into_inner().unwrap(), progress)
    }

    #[test]
    fn every_file_is_normalized_and_reported() {
        let dir = folder("all");
        let paths = wavs(&dir, &[0.05, 0.2, 0.4, 0.1, 0.3]);
        let options = Options { output: OutputMode::Copy, ..Options::default() };
        let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});

        let (events, progress) = collect(&paths, options, &job);

        for index in 0..paths.len() {
            let mine: Vec<_> = events.iter().filter(|e| e.index == index).collect();
            assert_eq!(mine.len(), 2, "file {index}: {mine:?}");
            assert!(matches!(mine[0].outcome, Outcome::Running));
            let Outcome::Done { report } = &mine[1].outcome else { panic!("file {index}: {:?}", mine[1]) };
            assert!((report.output_lufs - TARGETS.target_lufs).abs() < 1.0, "file {index}: {}", report.output_lufs);
        }
        assert_eq!(progress.last().map(|p| p.percent), Some(100.0));
        assert!(progress.iter().all(|p| (0.0..=100.0).contains(&p.percent)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failing_file_does_not_stop_the_others() {
        let dir = folder("failing");
        let mut paths = wavs(&dir, &[0.1, 0.2]);
        paths.insert(1, dir.join("missing.wav"));
        let options = Options { output: OutputMode::Copy, ..Options::default() };
        let job = Job::new(Arc::new(AtomicBool::new(false)), |_| {});

        let (events, _) = collect(&paths, options, &job);

        let last = |index| events.iter().rfind(|e| e.index == index).unwrap().outcome.clone();
        assert!(matches!(last(0), Outcome::Done { .. }));
        assert!(matches!(last(1), Outcome::Failed { .. }));
        assert!(matches!(last(2), Outcome::Done { .. }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancelling_skips_every_file_and_touches_nothing() {
        let dir = folder("cancelled");
        let paths = wavs(&dir, &[0.1, 0.2, 0.3]);
        let before: Vec<_> = paths.iter().map(|p| std::fs::metadata(p).unwrap().len()).collect();
        let job = Job::new(Arc::new(AtomicBool::new(true)), |_| {});

        let (events, _) = collect(&paths, Options::default(), &job);

        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| matches!(e.outcome, Outcome::Skipped)));
        let after: Vec<_> = paths.iter().map(|p| std::fs::metadata(p).unwrap().len()).collect();
        assert_eq!(before, after);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn events_serialize_with_a_flat_status() {
        let value = serde_json::to_value(FileEvent { index: 2, outcome: Outcome::Failed { message: "x".into() } }).unwrap();
        assert_eq!(value["index"], 2);
        assert_eq!(value["status"], "failed");
        assert_eq!(value["message"], "x");
        assert_eq!(serde_json::to_value(FileEvent { index: 0, outcome: Outcome::Skipped }).unwrap()["status"], "skipped");
    }

    #[test]
    fn parallelism_is_bounded_by_files_and_cores() {
        assert_eq!(parallelism(0), 1);
        assert_eq!(parallelism(1), 1);
        assert!((1..=MAX_PARALLEL_FILES).contains(&parallelism(100)));
    }
}
