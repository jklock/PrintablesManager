//! Preferences: read, merge, persist.
//!
//! 2.0 stores prefs at `~/Library/Application Support/dev.printablesoffline/`
//! and migrates a 1.x `~/.printablesoffline/prefs.json` on first run.

use crate::config;
use crate::error::{AppError, AppResult};
use crate::models::AppPrefs;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};

/// Field names stay snake_case: that is what the existing UI already sends.
#[derive(Debug, Deserialize, Default)]
pub struct SetPrefsArgs {
    pub theme: Option<String>,
    pub slicer_key: Option<String>,
    pub slicer_executable: Option<String>,
    pub library_folder: Option<String>,
    pub git_enabled: Option<bool>,
    pub auto_commit_local_edits: Option<bool>,
    pub sync_concurrency: Option<usize>,
    pub auto_sync_on_launch: Option<bool>,
    pub remote_write_enabled: Option<bool>,
}

#[tauri::command]
pub fn get_prefs(_app: AppHandle) -> AppResult<AppPrefs> {
    let _ = config::migrate_legacy_prefs();
    config::load_prefs()
}

#[tauri::command]
pub fn set_prefs(
    app: AppHandle,
    args: SetPrefsArgs,
    prefs_state: State<'_, Mutex<AppPrefs>>,
) -> AppResult<AppPrefs> {
    let mut prefs = config::load_prefs()?;

    if let Some(theme) = args.theme {
        prefs.theme = theme;
    }
    if let Some(key) = args.slicer_key {
        prefs.slicer_key = key;
    }
    if let Some(exe) = args.slicer_executable {
        prefs.slicer_executable = Some(exe);
    }
    if let Some(flag) = args.git_enabled {
        prefs.git_enabled = flag;
    }
    if let Some(flag) = args.auto_commit_local_edits {
        prefs.auto_commit_local_edits = flag;
    }
    if let Some(n) = args.sync_concurrency {
        prefs.sync_concurrency = n.clamp(1, 8);
    }
    if let Some(flag) = args.auto_sync_on_launch {
        prefs.auto_sync_on_launch = flag;
    }
    if let Some(flag) = args.remote_write_enabled {
        prefs.remote_write_enabled = flag;
    }

    if let Some(selected) = args.library_folder {
        let lib_path = normalize_library_folder(&selected)?;
        prefs.library_folder = Some(lib_path.display().to_string());
        widen_asset_scope(&app, &lib_path);
    }

    config::save_prefs(&prefs)?;

    let mut managed = prefs_state
        .lock()
        .map_err(|_| AppError::internal("Preferences lock poisoned"))?;
    *managed = prefs.clone();

    Ok(prefs)
}

/// Chosen folder becomes `<folder>/Printables Offline Library` unless it is
/// already named that. Kept from 1.x so existing libraries keep working.
fn normalize_library_folder(selected: &str) -> AppResult<PathBuf> {
    let path = PathBuf::from(selected);
    let is_library = path
        .file_name()
        .map(|n| n.to_string_lossy() == "Printables Offline Library")
        .unwrap_or(false);

    let lib = if is_library {
        path
    } else {
        path.join("Printables Offline Library")
    };

    std::fs::create_dir_all(&lib)
        .map_err(|e| AppError::io(format!("Cannot create {}: {e}", lib.display())))?;
    Ok(lib)
}

fn widen_asset_scope(app: &AppHandle, path: &std::path::Path) {
    if let Ok(canonical) = path.canonicalize() {
        let _ = app.asset_protocol_scope().allow_directory(&canonical, true);
    }
}

/// Reveal the preferences file in Finder (Settings → Advanced).
#[tauri::command]
pub fn open_config_folder(app: AppHandle) -> AppResult<String> {
    use tauri_plugin_opener::OpenerExt;
    let dir = config::config_dir()?;
    app.opener()
        .reveal_item_in_dir(&dir)
        .map_err(|e| AppError::io(format!("Cannot open config folder: {e}")))?;
    Ok(dir.display().to_string())
}
