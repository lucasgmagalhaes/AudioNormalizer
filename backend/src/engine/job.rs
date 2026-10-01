//! Progress reporting and cooperative cancellation for a running job.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    /// `analyze`, `calibrate`, `normalize`, `retry`, `verify` or `finalize`.
    pub stage: &'static str,
    /// Overall job progress, 0..=100.
    pub percent: f64,
}

/// Marker error: the job was cancelled by the user.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("processamento cancelado")
    }
}

impl std::error::Error for Cancelled {}

/// Blocks in flight between pipeline stages (8192 frames each).
pub(crate) const QUEUE_BLOCKS: usize = 8;

/// Marker error: a downstream pipeline stage stopped; its own error wins.
#[derive(Debug)]
pub(crate) struct StageStopped;

impl std::fmt::Display for StageStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("etapa de processamento interrompida")
    }
}

impl std::error::Error for StageStopped {}

type Sink = Box<dyn Fn(ProgressEvent) + Send + Sync>;

pub struct Job {
    cancel: Arc<AtomicBool>,
    sink: Sink,
}

impl Job {
    pub fn new(cancel: Arc<AtomicBool>, sink: impl Fn(ProgressEvent) + Send + Sync + 'static) -> Self {
        Self { cancel, sink: Box::new(sink) }
    }

    pub fn check_cancelled(&self) -> anyhow::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    /// Start a stage that fills `span` percent of the bar starting at `start`.
    pub fn stage(&self, stage: &'static str, start: f64, span: f64) -> Progress<'_> {
        (self.sink)(ProgressEvent { stage, percent: round(start) });
        Progress {
            job: self,
            stage,
            start,
            span,
            last_emit: Instant::now(),
            last_percent: start,
        }
    }
}

/// Maps a stage's own 0..1 fraction onto its slice of the overall bar,
/// throttled so the UI is not flooded with events.
pub struct Progress<'a> {
    job: &'a Job,
    stage: &'static str,
    start: f64,
    span: f64,
    last_emit: Instant,
    last_percent: f64,
}

impl Progress<'_> {
    pub fn update(&mut self, fraction: f64) {
        let percent = self.start + self.span * fraction.clamp(0.0, 1.0);
        let moved = percent - self.last_percent;
        if moved >= 0.5 || (moved > 0.0 && self.last_emit.elapsed() >= Duration::from_millis(250)) {
            self.last_emit = Instant::now();
            self.last_percent = percent;
            (self.job.sink)(ProgressEvent { stage: self.stage, percent: round(percent) });
        }
    }
}

fn round(p: f64) -> f64 {
    (p * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn reports_progress_and_honors_cancellation() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let cancel = Arc::new(AtomicBool::new(false));
        let job = Job::new(Arc::clone(&cancel), move |event| captured.lock().unwrap().push(event));

        let mut progress = job.stage("analyze", 10.0, 50.0);
        progress.update(-1.0);
        progress.update(1.0);
        assert_eq!(events.lock().unwrap().iter().map(|e| e.percent).collect::<Vec<_>>(), [10.0, 60.0]);

        cancel.store(true, Ordering::Relaxed);
        assert_eq!(job.check_cancelled().unwrap_err().to_string(), "processamento cancelado");
    }

    #[test]
    fn marker_errors_describe_their_failure() {
        assert_eq!(Cancelled.to_string(), "processamento cancelado");
        assert_eq!(StageStopped.to_string(), "etapa de processamento interrompida");
        assert_eq!(round(12.34), 12.3);
    }
}
