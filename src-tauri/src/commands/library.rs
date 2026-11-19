use crate::config;
use crate::error::{AppError, AppResult};
use crate::library::indexer;
use crate::models::{AppPrefs, LibraryIndex, LibraryTotals};
use std::sync::Mutex;
use tauri::State;

fn snapshot(prefs: &State<'_, Mutex<AppPrefs>>) -> AppResult<AppPrefs> {
    prefs
        .lock()
        .map(|p| p.clone())
        .map_err(|_| AppError::internal("Preferences lock poisoned"))
}

/// Return the full library index (models + totals).
#[tauri::command]
pub fn list_models(prefs: State<'_, Mutex<AppPrefs>>) -> AppResult<LibraryIndex> {
    let prefs = snapshot(&prefs)?;
    let library = config::library_dir(&prefs)?;
    indexer::index_library(&library).map_err(AppError::internal)
}

/// Return just the aggregate totals for the sidebar storage card.
#[tauri::command]
pub fn library_stats(prefs: State<'_, Mutex<AppPrefs>>) -> AppResult<LibraryTotals> {
    Ok(list_models(prefs)?.totals)
}

/// Rescan the library folder and re-index all metadata.json files.
#[tauri::command]
pub fn rescan_library(prefs: State<'_, Mutex<AppPrefs>>) -> AppResult<LibraryIndex> {
    list_models(prefs)
}
