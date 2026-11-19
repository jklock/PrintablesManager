//! Sync commands: list the account library, pull one model or all of them,
//! and inspect/cancel background jobs.

use crate::auth::AuthState;
use crate::config;
use crate::error::{AppError, AppResult};
use crate::jobs::{JobInfo, JobRegistry};
use crate::library::locator;
use crate::models::AppPrefs;
use crate::printables::api::PrintablesClient;
use crate::sync::{start_sync_all, start_sync_one};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, State};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncArgs {
    pub model_id: Option<String>,
    pub force: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelArgs {
    pub job_id: String,
}

fn snapshot(prefs: &State<'_, Mutex<AppPrefs>>) -> AppResult<AppPrefs> {
    prefs
        .lock()
        .map(|p| p.clone())
        .map_err(|_| AppError::internal("Preferences lock poisoned"))
}

fn local_model_ids(library: &std::path::Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    if let Ok(dirs) = locator::model_dirs(library) {
        for dir in dirs {
            if let Ok(meta) = locator::read_metadata(&dir) {
                ids.insert(meta.model_id);
            }
        }
    }
    ids
}

/// Lists every model authored by the signed-in account, marking which ones are
/// already in the local library.
#[tauri::command]
pub fn account_models(
    prefs: State<'_, Mutex<AppPrefs>>,
    auth: State<'_, AuthState>,
) -> AppResult<Value> {
    let prefs = snapshot(&prefs)?;
    let library = config::library_dir(&prefs)?;
    let token = auth
        .token()
        .ok_or_else(|| AppError::not_authenticated("Sign in to see your Printables models."))?;

    let client = PrintablesClient::with_token(Some(token))?;
    let me = client.viewer()?;
    let user_id = me.user_id.clone().ok_or_else(|| {
        AppError::not_authenticated("Printables did not report a user id for this session.")
    })?;
    let username = me
        .public_username
        .clone()
        .or_else(|| me.handle.clone())
        .unwrap_or_else(|| "your account".to_string());

    let local = local_model_ids(&library);
    let mut models: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut truncated = false;

    for _ in 0..20 {
        let page = client.user_models(&user_id, 100, cursor.as_deref(), None)?;
        if page.models.is_empty() {
            break;
        }
        for item in &page.models {
            let id = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if id.is_empty() || models.iter().any(|m| m["id"] == id) {
                continue;
            }
            let slug = item.get("slug").and_then(|v| v.as_str()).unwrap_or("");
            let cover = item
                .pointer("/image/filePath")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|fp| {
                    format!(
                        "https://media.printables.com/{}",
                        fp.trim_start_matches('/')
                    )
                });
            models.push(json!({
                "id": id,
                "name": item.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "slug": slug,
                "url": format!("https://www.printables.com/model/{id}-{slug}"),
                "cover_url": cover,
                "local": local.contains(&id),
            }));
        }
        match page.cursor {
            Some(next) if !next.is_empty() && Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => {
                truncated = models.len() >= 100;
                break;
            }
        }
    }

    Ok(json!({
        "username": username,
        "total": models.len(),
        "truncated": truncated,
        "models": models,
    }))
}

/// Pulls every model the account authored. Returns the batch job id.
#[tauri::command]
pub fn sync_all(
    app: AppHandle,
    prefs: State<'_, Mutex<AppPrefs>>,
    jobs: State<'_, Arc<JobRegistry>>,
    auth: State<'_, AuthState>,
) -> AppResult<String> {
    let prefs = snapshot(&prefs)?;
    let library = config::library_dir(&prefs)?;
    let token = auth
        .token()
        .ok_or_else(|| AppError::not_authenticated("Sign in to sync your library."))?;

    let client = PrintablesClient::with_token(Some(token.clone()))?;
    let me = client.viewer()?;
    let user_id = me.user_id.clone().ok_or_else(|| {
        AppError::not_authenticated("Printables did not report a user id for this session.")
    })?;

    let mut ids: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..20 {
        let page = client.user_models(&user_id, 100, cursor.as_deref(), None)?;
        if page.models.is_empty() {
            break;
        }
        for item in &page.models {
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                if !ids.iter().any(|existing| existing == id) {
                    ids.push(id.to_string());
                }
            }
        }
        match page.cursor {
            Some(next) if !next.is_empty() && Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => break,
        }
    }

    if ids.is_empty() {
        return Err(AppError::not_found(
            "No models were found on your Printables account.",
        ));
    }

    start_sync_all(
        app,
        jobs.inner().clone(),
        Some(token),
        library,
        ids,
        prefs.sync_concurrency,
        false,
        prefs.git_enabled,
    )
}

/// Pulls one model. Returns its job id.
#[tauri::command]
pub fn sync_model(
    app: AppHandle,
    args: SyncArgs,
    prefs: State<'_, Mutex<AppPrefs>>,
    jobs: State<'_, Arc<JobRegistry>>,
    auth: State<'_, AuthState>,
) -> AppResult<String> {
    let model_id = args
        .model_id
        .ok_or_else(|| AppError::invalid("A model id is required."))?;
    let prefs = snapshot(&prefs)?;
    let library = config::library_dir(&prefs)?;

    start_sync_one(
        app,
        jobs.inner().clone(),
        auth.token(),
        library,
        model_id,
        args.force.unwrap_or(false),
        prefs.git_enabled,
    )
}

#[tauri::command]
pub fn sync_cancel(jobs: State<'_, Arc<JobRegistry>>, args: CancelArgs) -> bool {
    jobs.cancel(&args.job_id)
}

#[tauri::command]
pub fn jobs_list(jobs: State<'_, Arc<JobRegistry>>) -> Vec<JobInfo> {
    jobs.list()
}

#[tauri::command]
pub fn jobs_clear(jobs: State<'_, Arc<JobRegistry>>) {
    jobs.clear_finished();
}
