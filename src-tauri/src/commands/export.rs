//! `export_files`: copy a model's files out of the library.

use crate::config;
use crate::error::{AppError, AppResult};
use crate::library::locator;
use crate::models::AppPrefs;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::State;
use tauri_plugin_dialog::DialogExt;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportArgs {
    pub model_id: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ExportResult {
    pub destination: String,
    pub copied: usize,
}

#[tauri::command]
pub async fn export_files(
    app: tauri::AppHandle,
    args: ExportArgs,
    prefs: State<'_, Mutex<AppPrefs>>,
) -> AppResult<ExportResult> {
    use tauri_plugin_opener::OpenerExt;

    let snapshot = {
        let prefs = prefs
            .lock()
            .map_err(|_| AppError::internal("Preferences lock poisoned"))?;
        prefs.clone()
    };
    let library = config::library_dir(&snapshot)?;
    let src_dir = locator::find_model_dir(&library, &args.model_id)?;
    let model_name = src_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| args.model_id.clone());

    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .set_title("Export model files to…")
        .pick_folder(move |path| {
            let _ = tx.send(path);
        });
    let dest = rx
        .recv()
        .map_err(|e| AppError::internal(e.to_string()))?
        .ok_or_else(|| AppError::invalid("Export cancelled."))?;
    let dest_path = PathBuf::from(dest.to_string());

    let target_dir = dest_path.join(&model_name);
    fs::create_dir_all(&target_dir)
        .map_err(|e| AppError::io(format!("Cannot create export folder: {e}")))?;

    let mut copied = 0usize;
    for sub in ["files", "images"] {
        let src_sub = src_dir.join(sub);
        if !src_sub.is_dir() {
            continue;
        }
        let dst_sub = target_dir.join(sub);
        fs::create_dir_all(&dst_sub)?;
        for entry in fs::read_dir(&src_sub)?.flatten() {
            let path = entry.path();
            if path.is_file() {
                fs::copy(&path, dst_sub.join(entry.file_name()))?;
                copied += 1;
            }
        }
    }

    let _ = fs::copy(
        src_dir.join("metadata.json"),
        target_dir.join("metadata.json"),
    );

    let _ = app.opener().reveal_item_in_dir(&target_dir);

    Ok(ExportResult {
        destination: target_dir.display().to_string(),
        copied,
    })
}
