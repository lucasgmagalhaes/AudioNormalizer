mod engine;

use engine::analyze::{self, AnalysisReport, Assessment, Measurement};
use engine::av::AudioCache;
use engine::job::{Cancelled, Job};
use engine::normalize::{self, NormalizeReport};
use engine::Targets;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

/// Only one analysis / normalization runs at a time.
#[derive(Default)]
struct JobSlot {
    running: AtomicBool,
    cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// The window was closed mid-job: exit once the job has cleaned up.
    exit_when_idle: AtomicBool,
}

impl JobSlot {
    fn cancel(&self) {
        if let Some(flag) = self.cancel.lock().unwrap().as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

/// Compressed audio recorded by the last analysis, reused by the following
/// normalization of the same, unchanged file.
#[derive(Default)]
struct CacheSlot(Mutex<Option<CachedAudio>>);

struct CachedAudio {
    key: FileKey,
    cache: Arc<AudioCache>,
}

/// Identifies a file's content well enough to detect it changed on disk.
#[derive(PartialEq, Eq)]
struct FileKey {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

impl FileKey {
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self { path: path.to_path_buf(), size: meta.len(), modified: meta.modified().ok() })
    }
}

impl CacheSlot {
    fn store(&self, path: &Path, cache: Option<AudioCache>) {
        let entry = cache.zip(FileKey::of(path)).map(|(cache, key)| CachedAudio { key, cache: Arc::new(cache) });
        *self.0.lock().unwrap() = entry;
    }

    fn take_for(&self, path: &Path) -> Option<Arc<AudioCache>> {
        let key = FileKey::of(path)?;
        let entry = self.0.lock().unwrap().take()?;
        (entry.key == key).then_some(entry.cache)
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum CommandError {
    Busy,
    Cancelled,
    Failed { message: String },
}

impl From<anyhow::Error> for CommandError {
    fn from(err: anyhow::Error) -> Self {
        if err.is::<Cancelled>() {
            CommandError::Cancelled
        } else {
            CommandError::Failed { message: format!("{err:#}") }
        }
    }
}

async fn run_job<T, F>(app: AppHandle, slot: Arc<JobSlot>, work: F) -> Result<T, CommandError>
where
    T: Send + 'static,
    F: FnOnce(&Job) -> anyhow::Result<T> + Send + 'static,
{
    if slot.running.swap(true, Ordering::SeqCst) {
        return Err(CommandError::Busy);
    }
    let cancel = Arc::new(AtomicBool::new(false));
    *slot.cancel.lock().unwrap() = Some(cancel.clone());

    let emitter = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let job = Job::new(cancel, move |event| {
            let _ = emitter.emit("job-progress", event);
        });
        work(&job)
    })
    .await;

    *slot.cancel.lock().unwrap() = None;
    slot.running.store(false, Ordering::SeqCst);
    if slot.exit_when_idle.load(Ordering::SeqCst) {
        app.exit(0);
    }

    match result {
        Ok(outcome) => outcome.map_err(CommandError::from),
        Err(err) => Err(CommandError::Failed { message: format!("falha interna: {err}") }),
    }
}

#[tauri::command]
async fn analyze_file(
    app: AppHandle,
    slot: State<'_, Arc<JobSlot>>,
    cache: State<'_, CacheSlot>,
    path: String,
    targets: Targets,
) -> Result<AnalysisReport, CommandError> {
    targets.validate()?;
    let path = PathBuf::from(path);
    let job_path = path.clone();
    let analysis = run_job(app, slot.inner().clone(), move |job| analyze::run(targets, &job_path, job)).await?;
    cache.store(&path, analysis.cache);
    Ok(analysis.report)
}

/// Re-evaluate an existing measurement against new targets (no decoding).
#[tauri::command]
fn assess(measurement: Measurement, targets: Targets) -> Result<Assessment, CommandError> {
    targets.validate()?;
    Ok(analyze::assess(&measurement, targets))
}

#[tauri::command]
async fn normalize_file(
    app: AppHandle,
    slot: State<'_, Arc<JobSlot>>,
    cache: State<'_, CacheSlot>,
    path: String,
    targets: Targets,
    measured: Option<Measurement>,
    options: Option<normalize::Options>,
) -> Result<NormalizeReport, CommandError> {
    targets.validate()?;
    let path = PathBuf::from(path);
    // Taken, not borrowed: the file is replaced, so the cache is stale after.
    let audio = measured.and(cache.take_for(&path));
    run_job(app, slot.inner().clone(), move |job| {
        normalize::run(targets, options.unwrap_or_default(), &path, measured, audio, job)
    })
    .await
}

#[tauri::command]
fn cancel_job(slot: State<'_, Arc<JobSlot>>) {
    slot.cancel();
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileInfo {
    path: String,
    name: String,
    directory: String,
    size: u64,
}

#[tauri::command]
fn inspect_file(path: String) -> Result<FileInfo, CommandError> {
    let p = Path::new(&path);
    let meta = std::fs::metadata(p).map_err(|e| CommandError::Failed {
        message: format!("não foi possível abrir o arquivo: {e}"),
    })?;
    if !meta.is_file() {
        return Err(CommandError::Failed { message: "selecione um arquivo, não uma pasta".into() });
    }
    Ok(FileInfo {
        name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        directory: p.parent().map(|d| d.display().to_string()).unwrap_or_default(),
        size: meta.len(),
        path,
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Arc::new(JobSlot::default()))
        .manage(CacheSlot::default())
        .on_window_event(|window, event| {
            // Closing mid-job would leave a half-written temp file behind:
            // cancel, let the job clean up, then exit (see run_job).
            if let WindowEvent::CloseRequested { api, .. } = event {
                let slot = window.state::<Arc<JobSlot>>();
                if slot.running.load(Ordering::SeqCst) {
                    api.prevent_close();
                    slot.exit_when_idle.store(true, Ordering::SeqCst);
                    slot.cancel();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            analyze_file,
            assess,
            normalize_file,
            cancel_job,
            inspect_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("audio-normalizer-{name}-{}", std::process::id()))
    }

    #[test]
    fn job_slot_cancels_the_active_job() {
        let slot = JobSlot::default();
        let flag = Arc::new(AtomicBool::new(false));
        *slot.cancel.lock().unwrap() = Some(flag.clone());

        slot.cancel();

        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn cache_slot_discards_missing_or_empty_entries() {
        let slot = CacheSlot::default();
        let path = temporary_path("missing-cache");

        slot.store(&path, None);

        assert!(slot.take_for(&path).is_none());
        assert!(FileKey::of(&path).is_none());
    }

    #[test]
    fn inspect_file_reports_file_details_and_rejects_directories() {
        let path = temporary_path("inspect.txt");
        fs::write(&path, b"audio").unwrap();

        let info = inspect_file(path.display().to_string()).unwrap();
        assert_eq!(info.name, path.file_name().unwrap().to_string_lossy());
        assert_eq!(info.size, 5);
        assert!(!info.directory.is_empty());

        let error = inspect_file(std::env::temp_dir().display().to_string()).unwrap_err();
        assert!(matches!(error, CommandError::Failed { .. }));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn command_errors_keep_cancelled_and_failed_causes_distinct() {
        assert!(matches!(CommandError::from(anyhow::Error::new(Cancelled)), CommandError::Cancelled));

        let error = CommandError::from(anyhow::anyhow!("codec unavailable"));
        let value = serde_json::to_value(error).unwrap();
        assert_eq!(value["kind"], "failed");
        assert_eq!(value["message"], "codec unavailable");
    }

    #[test]
    fn assess_rejects_invalid_targets_before_evaluating() {
        let measurement = Measurement {
            integrated_lufs: -20.0,
            loudness_range: 4.0,
            true_peak_db: -3.0,
            sample_peak_db: -3.0,
        };
        let valid = Targets { target_lufs: -16.0, true_peak_db: -1.0 };

        assert!(assess(measurement, valid).is_ok());
        assert!(assess(measurement, Targets { target_lufs: -60.0, true_peak_db: -1.0 }).is_err());
    }
}
