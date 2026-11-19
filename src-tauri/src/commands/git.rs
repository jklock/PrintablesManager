//! Git commands: per-model history, diffs, manual commits, restore, compaction.

use crate::config;
use crate::error::{AppError, AppResult};
use crate::git;
use crate::library::locator;
use crate::models::AppPrefs;
use serde::Deserialize;
use std::sync::Mutex;
use tauri::State;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelArgs {
    pub model_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogArgs {
    pub model_id: String,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitArgs {
    pub model_id: String,
    pub commit_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitMessageArgs {
    pub model_id: String,
    pub message: Option<String>,
}

fn model_dir(prefs: &State<'_, Mutex<AppPrefs>>, model_id: &str) -> AppResult<std::path::PathBuf> {
    let snapshot = prefs
        .lock()
        .map(|p| p.clone())
        .map_err(|_| AppError::internal("Preferences lock poisoned"))?;
    let library = config::library_dir(&snapshot)?;
    locator::find_model_dir(&library, model_id)
}

#[tauri::command]
pub fn git_status(prefs: State<'_, Mutex<AppPrefs>>, args: ModelArgs) -> AppResult<git::GitStatus> {
    let dir = model_dir(&prefs, &args.model_id)?;
    git::status(&dir)
}

#[tauri::command]
pub fn git_log(
    prefs: State<'_, Mutex<AppPrefs>>,
    args: LogArgs,
) -> AppResult<Vec<git::CommitInfo>> {
    let dir = model_dir(&prefs, &args.model_id)?;
    git::log(&dir, args.limit.unwrap_or(100).clamp(1, 1000))
}

#[tauri::command]
pub fn git_show(
    prefs: State<'_, Mutex<AppPrefs>>,
    args: CommitArgs,
) -> AppResult<git::CommitDetail> {
    let dir = model_dir(&prefs, &args.model_id)?;
    git::show(&dir, &args.commit_id)
}

/// Manual commit of the current working tree.
#[tauri::command]
pub fn git_commit(
    prefs: State<'_, Mutex<AppPrefs>>,
    args: CommitMessageArgs,
) -> AppResult<Option<String>> {
    let dir = model_dir(&prefs, &args.model_id)?;
    let message = args
        .message
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "Manual commit".to_string());
    git::commit_all(&dir, &message)
}

/// Restore a previous revision into the folder and record it as a new commit.
#[tauri::command]
pub fn git_restore(
    prefs: State<'_, Mutex<AppPrefs>>,
    args: CommitArgs,
) -> AppResult<Option<String>> {
    let dir = model_dir(&prefs, &args.model_id)?;
    git::restore(&dir, &args.commit_id)
}

/// Collapse history to a single commit (a `pre-compact-*` tag keeps it recoverable).
#[tauri::command]
pub fn git_compact(prefs: State<'_, Mutex<AppPrefs>>, args: ModelArgs) -> AppResult<usize> {
    let dir = model_dir(&prefs, &args.model_id)?;
    git::compact(&dir)
}

/// Start tracking a model folder that predates 2.0.
#[tauri::command]
pub fn git_init(prefs: State<'_, Mutex<AppPrefs>>, args: ModelArgs) -> AppResult<Option<String>> {
    let dir = model_dir(&prefs, &args.model_id)?;
    git::ensure_initialised(&dir)
}

/// Adopt every untracked model folder in the library.
#[tauri::command]
pub fn git_adopt_library(prefs: State<'_, Mutex<AppPrefs>>) -> AppResult<usize> {
    let snapshot = prefs
        .lock()
        .map(|p| p.clone())
        .map_err(|_| AppError::internal("Preferences lock poisoned"))?;
    let library = config::library_dir(&snapshot)?;
    crate::printables::clone::adopt_existing_models(&library)
}
