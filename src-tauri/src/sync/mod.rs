//! Sync engine: pulls a whole account library, one model at a time, with a
//! bounded number of parallel workers, progress events, and cancellation.

pub mod plan;
pub mod push;
pub mod watcher;

use crate::error::{AppError, AppResult};
use crate::jobs::{JobInfo, JobRegistry};
use crate::printables::api::PrintablesClient;
use crate::printables::clone::{pull_model, PullOptions, PullOutcome};
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize)]
pub struct SyncSummary {
    pub requested: usize,
    pub synced: usize,
    pub failed: usize,
    pub cancelled: bool,
    pub outcomes: Vec<PullOutcome>,
    pub errors: Vec<String>,
}

fn emit_job(app: &AppHandle, job: Option<JobInfo>) {
    if let Some(job) = job {
        let _ = app.emit("job://update", &job);
    }
}

/// Starts a background batch pull. Returns the batch job id immediately.
#[allow(clippy::too_many_arguments)]
pub fn start_sync_all(
    app: AppHandle,
    jobs: Arc<JobRegistry>,
    token: Option<String>,
    library: PathBuf,
    model_ids: Vec<String>,
    concurrency: usize,
    force: bool,
    git_enabled: bool,
) -> AppResult<String> {
    if model_ids.is_empty() {
        return Err(AppError::invalid("No models to sync."));
    }
    let client = Arc::new(PrintablesClient::with_token(token)?);
    let total = model_ids.len();
    let batch_label = format!("Syncing {total} model(s)");
    let batch_id = jobs.create("sync_all", &batch_label, None);
    emit_job(&app, jobs.start(&batch_id));

    let queue = Arc::new(Mutex::new(VecDeque::from(model_ids)));
    let completed = Arc::new(AtomicUsize::new(0));
    let outcomes: Arc<Mutex<Vec<PullOutcome>>> = Arc::new(Mutex::new(Vec::new()));
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let workers = concurrency.clamp(1, 8);

    let mut handles = Vec::new();
    for _ in 0..workers {
        let client = client.clone();
        let queue = queue.clone();
        let jobs = jobs.clone();
        let app = app.clone();
        let library = library.clone();
        let batch_id = batch_id.clone();
        let completed = completed.clone();
        let outcomes = outcomes.clone();
        let errors = errors.clone();

        handles.push(std::thread::spawn(move || loop {
            if jobs.is_cancelled(&batch_id) {
                break;
            }
            let next = { queue.lock().map(|mut q| q.pop_front()).ok().flatten() };
            let Some(model_id) = next else { break };

            let child = jobs.create("pull", &format!("Model {model_id}"), Some(&model_id));
            emit_job(&app, jobs.start(&child));

            let app_cb = app.clone();
            let jobs_cb = jobs.clone();
            let child_cb = child.clone();

            let result = pull_model(
                &client,
                &model_id,
                &library,
                &PullOptions {
                    force,
                    git: git_enabled,
                },
                |event| {
                    if let Some(percent) = event.get("percent").and_then(|v| v.as_f64()) {
                        let message = event
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        emit_job(&app_cb, jobs_cb.update(&child_cb, percent as f32, message));
                    }
                },
            );

            match result {
                Ok(outcome) => {
                    let summary = format!(
                        "{} · {} new, {} present{}",
                        outcome.name,
                        outcome.files_downloaded,
                        outcome.files_skipped,
                        if outcome.warnings.is_empty() {
                            String::new()
                        } else {
                            format!(" · {} warning(s)", outcome.warnings.len())
                        }
                    );
                    if let Ok(mut guard) = outcomes.lock() {
                        guard.push(outcome);
                    }
                    emit_job(&app, jobs.finish(&child, summary));
                }
                Err(err) => {
                    if let Ok(mut guard) = errors.lock() {
                        guard.push(format!("Model {model_id}: {}", err.message));
                    }
                    emit_job(&app, jobs.fail(&child, err.message.clone()));
                }
            }

            let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
            let pct = (done as f32 / total as f32) * 100.0;
            emit_job(
                &app,
                jobs.update(&batch_id, pct, format!("{done}/{total} models")),
            );
        }));
    }

    let app_done = app.clone();
    let jobs_done = jobs.clone();
    let batch_id_done = batch_id.clone();
    std::thread::spawn(move || {
        for handle in handles {
            let _ = handle.join();
        }
        let cancelled = jobs_done.is_cancelled(&batch_id_done);
        let synced = outcomes.lock().map(|o| o.len()).unwrap_or(0);
        let failed = errors.lock().map(|e| e.len()).unwrap_or(0);
        let summary = SyncSummary {
            requested: total,
            synced,
            failed,
            cancelled,
            outcomes: outcomes.lock().map(|o| o.clone()).unwrap_or_default(),
            errors: errors.lock().map(|e| e.clone()).unwrap_or_default(),
        };

        let message = if cancelled {
            format!("Cancelled after {synced} of {total}")
        } else {
            format!("{synced} synced, {failed} failed")
        };
        emit_job(&app_done, jobs_done.finish(&batch_id_done, message));
        let _ = app_done.emit("sync://batch", &summary);
    });

    Ok(batch_id)
}

/// Pulls a single model in the background. Returns the job id.
pub fn start_sync_one(
    app: AppHandle,
    jobs: Arc<JobRegistry>,
    token: Option<String>,
    library: PathBuf,
    model_id: String,
    force: bool,
    git_enabled: bool,
) -> AppResult<String> {
    let client = PrintablesClient::with_token(token)?;
    let job_id = jobs.create("pull", &format!("Model {model_id}"), Some(&model_id));
    emit_job(&app, jobs.start(&job_id));

    let app_clone = app.clone();
    let jobs_clone = jobs.clone();
    let job_clone = job_id.clone();

    std::thread::spawn(move || {
        let app_cb = app_clone.clone();
        let jobs_cb = jobs_clone.clone();
        let job_cb = job_clone.clone();

        let result = pull_model(
            &client,
            &model_id,
            &library,
            &PullOptions {
                force,
                git: git_enabled,
            },
            |event| {
                if let Some(percent) = event.get("percent").and_then(|v| v.as_f64()) {
                    let message = event
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    emit_job(&app_cb, jobs_cb.update(&job_cb, percent as f32, message));
                }
            },
        );

        match result {
            Ok(outcome) => {
                emit_job(
                    &app_clone,
                    jobs_clone.finish(
                        &job_clone,
                        format!(
                            "{} · {} new, {} present",
                            outcome.name, outcome.files_downloaded, outcome.files_skipped
                        ),
                    ),
                );
                let _ = app_clone.emit("sync://model", &outcome);
            }
            Err(err) => {
                emit_job(&app_clone, jobs_clone.fail(&job_clone, err.message.clone()));
                let _ = app_clone.emit(
                    "sync://error",
                    serde_json::json!({ "model_id": model_id, "message": err.message }),
                );
            }
        }
    });

    Ok(job_id)
}
