//! Push commands: preview the changes that would go to printables.com and
//! apply them (dry-run by default).

use crate::auth::AuthState;
use crate::config;
use crate::error::{AppError, AppResult};
use crate::models::AppPrefs;
use crate::printables::api::PrintablesClient;
use crate::sync::plan::{self, PushPlan};
use crate::sync::push::{self, PushReport};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Mutex;
use tauri::State;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushArgs {
    pub model_id: String,
    /// `plan_id` from the preview the user reviewed.
    pub plan_id: Option<String>,
    pub dry_run: Option<bool>,
}

fn context(prefs: &State<'_, Mutex<AppPrefs>>) -> AppResult<(AppPrefs, std::path::PathBuf)> {
    let snapshot = prefs
        .lock()
        .map(|p| p.clone())
        .map_err(|_| AppError::internal("Preferences lock poisoned"))?;
    let library = config::library_dir(&snapshot)?;
    Ok((snapshot, library))
}

/// Returns the reviewable plan. Never writes anything.
#[tauri::command]
pub fn push_preview(
    prefs: State<'_, Mutex<AppPrefs>>,
    auth: State<'_, AuthState>,
    args: PushArgs,
) -> AppResult<PushPlan> {
    let (prefs, library) = context(&prefs)?;
    let client = PrintablesClient::with_token(auth.token())?;
    plan::build(
        &client,
        &library,
        &args.model_id,
        prefs.remote_write_enabled,
    )
}

/// Applies a reviewed plan. `dry_run` defaults to true.
#[tauri::command]
pub fn push_apply(
    prefs: State<'_, Mutex<AppPrefs>>,
    auth: State<'_, AuthState>,
    args: PushArgs,
) -> AppResult<PushReport> {
    let (prefs, library) = context(&prefs)?;
    let client = PrintablesClient::with_token(auth.token())?;

    // Rebuild so we never apply a stale plan.
    let current = plan::build(
        &client,
        &library,
        &args.model_id,
        prefs.remote_write_enabled,
    )?;
    if let Some(expected) = args.plan_id.as_deref() {
        if expected != current.plan_id {
            return Err(AppError::conflict(
                "The folder changed since you reviewed the push. Review the changes again.",
            ));
        }
    }

    push::apply(&client, &library, &current, args.dry_run.unwrap_or(true))
}

/// Recent push attempts, newest first.
#[tauri::command]
pub fn push_log(prefs: State<'_, Mutex<AppPrefs>>) -> AppResult<Vec<Value>> {
    let (_, library) = context(&prefs)?;
    Ok(push::recent_entries(&library, 50))
}
