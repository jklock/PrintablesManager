//! `clone_model`: pull one model from a pasted printables.com URL.
//!
//! Keeps the 1.x `clone://{job}/…` event contract so the existing clone view
//! keeps working, while going through the new idempotent pull pipeline (git
//! commit included).

use crate::auth::AuthState;
use crate::config;
use crate::error::{AppError, AppResult};
use crate::models::AppPrefs;
use crate::printables::api::PrintablesClient;
use crate::printables::clone::{parse_model_url, pull_model, PullOptions};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, State};

#[derive(Debug, Deserialize)]
pub struct CloneArgs {
    pub url: String,
}

#[tauri::command]
pub fn clone_model(
    app: AppHandle,
    args: CloneArgs,
    prefs: State<'_, Mutex<AppPrefs>>,
    auth: State<'_, AuthState>,
) -> AppResult<Value> {
    let parsed = parse_model_url(&args.url).ok_or_else(|| {
        AppError::invalid(format!(
            "Not a valid printables.com model URL: {}",
            args.url
        ))
    })?;

    let snapshot = {
        let prefs = prefs
            .lock()
            .map_err(|_| AppError::internal("Preferences lock poisoned"))?;
        prefs.clone()
    };
    let library = config::library_dir(&snapshot)?;
    let git_enabled = snapshot.git_enabled;
    let token = auth.token();

    let job_id = uuid::Uuid::new_v4().to_string();
    let model_id = parsed.id.clone();
    let app_clone = app.clone();
    let job_clone = job_id.clone();

    std::thread::spawn(move || {
        let client = match PrintablesClient::with_token(token) {
            Ok(client) => client,
            Err(err) => {
                let _ = app_clone.emit(
                    &format!("clone://{job_clone}/error"),
                    &json!({ "job_id": job_clone, "code": err.code, "message": err.message }),
                );
                return;
            }
        };

        let app_cb = app_clone.clone();
        let job_cb = job_clone.clone();

        let result = pull_model(
            &client,
            &model_id,
            &library,
            &PullOptions {
                force: false,
                git: git_enabled,
            },
            |record| {
                if let Some(kind) = record.get("kind").and_then(|v| v.as_str()) {
                    let _ = app_cb.emit(&format!("clone://{job_cb}/progress/{kind}"), &record);
                }
            },
        );

        match result {
            Ok(outcome) => {
                let _ = app_clone.emit(
                    &format!("clone://{job_clone}/done"),
                    &json!({
                        "job_id": job_clone,
                        "model_id": outcome.model_id,
                        "model_dir": outcome.model_dir,
                        "name": outcome.name,
                        "files_downloaded": outcome.files_downloaded,
                        "files_skipped": outcome.files_skipped,
                        "warnings": outcome.warnings,
                    }),
                );
            }
            Err(err) => {
                let _ = app_clone.emit(
                    &format!("clone://{job_clone}/error"),
                    &json!({ "job_id": job_clone, "code": err.code, "message": err.message }),
                );
            }
        }
    });

    Ok(json!({ "job_id": job_id, "model_id": parsed.id }))
}
