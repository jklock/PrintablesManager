//! `delete_model`: remove a model folder and forget its bookkeeping.

use crate::config;
use crate::error::{AppError, AppResult};
use crate::library::locator;
use crate::models::{AppPrefs, CollectionsFile};
use serde::Deserialize;
use std::fs;
use std::sync::Mutex;
use tauri::State;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteArgs {
    pub model_id: String,
}

#[tauri::command]
pub fn delete_model(args: DeleteArgs, prefs: State<'_, Mutex<AppPrefs>>) -> AppResult<()> {
    let snapshot = {
        let prefs = prefs
            .lock()
            .map_err(|_| AppError::internal("Preferences lock poisoned"))?;
        prefs.clone()
    };
    let library = config::library_dir(&snapshot)?;

    let dir = locator::find_model_dir(&library, &args.model_id)?;
    fs::remove_dir_all(&dir)
        .map_err(|e| AppError::io(format!("Failed to delete {}: {e}", dir.display())))?;

    // Drop the sync bookkeeping too, otherwise a later sync would think this
    // model is still known.
    let _ = fs::remove_file(config::model_state_path(&library, &args.model_id));

    prune_collections(&library, &args.model_id);
    Ok(())
}

fn prune_collections(library: &std::path::Path, model_id: &str) {
    let path = library.join("collections.json");
    let Ok(raw) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut data) = serde_json::from_str::<CollectionsFile>(&raw) else {
        return;
    };

    let mut changed = false;
    for collection in data.collections.iter_mut() {
        let before = collection.model_ids.len();
        collection.model_ids.retain(|id| id != model_id);
        if collection.model_ids.len() != before {
            changed = true;
        }
    }

    if changed {
        if let Ok(serialized) = serde_json::to_string_pretty(&data) {
            let _ = fs::write(&path, serialized);
        }
    }
}
