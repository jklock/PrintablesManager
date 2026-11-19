//! On-disk locations and preference persistence.
//!
//! Preferences live at
//! `~/Library/Application Support/dev.printablesmanager/prefs.json`.
//! Two older locations are migrated on first launch:
//!
//!   * `~/Library/Application Support/dev.printablesoffline/` (the pre-rename 2.0 build)
//!   * `~/.printablesoffline/` (1.x)
//!
//! Migration copies the file and leaves the original behind renamed to
//! `prefs.json.migrated`, so nothing is destroyed.

use crate::error::{AppError, AppResult};
use crate::models::AppPrefs;
use std::fs;
use std::path::{Path, PathBuf};

pub const APP_DIR_NAME: &str = "dev.printablesmanager";
/// Application Support folder used by the build that was still called
/// "Printables Offline".
pub const PREVIOUS_DIR_NAME: &str = "dev.printablesoffline";
pub const LEGACY_DIR_NAME: &str = ".printablesoffline";

pub fn home_dir() -> AppResult<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| AppError::io("Cannot determine home directory"))
}

/// `~/Library/Application Support/dev.printablesmanager`
pub fn config_dir() -> AppResult<PathBuf> {
    let dir = home_dir()?
        .join("Library")
        .join("Application Support")
        .join(APP_DIR_NAME);
    fs::create_dir_all(&dir)
        .map_err(|e| AppError::io(format!("Cannot create config directory: {e}")))?;
    Ok(dir)
}

/// `~/Library/Application Support/dev.printablesoffline` (pre-rename 2.0 build).
pub fn previous_app_dir() -> AppResult<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("Application Support")
        .join(PREVIOUS_DIR_NAME))
}

pub fn prefs_path() -> AppResult<PathBuf> {
    Ok(config_dir()?.join("prefs.json"))
}

pub fn legacy_prefs_path() -> AppResult<PathBuf> {
    Ok(home_dir()?.join(LEGACY_DIR_NAME).join("prefs.json"))
}

pub fn token_path() -> AppResult<PathBuf> {
    Ok(config_dir()?.join("session.json"))
}

pub fn load_prefs() -> AppResult<AppPrefs> {
    let path = prefs_path()?;
    if !path.exists() {
        return Ok(AppPrefs::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| AppError::io(format!("Cannot read {}: {e}", path.display())))?;
    serde_json::from_str(&raw).map_err(|e| {
        AppError::io(format!(
            "Preferences at {} are not valid JSON: {e}",
            path.display()
        ))
    })
}

pub fn save_prefs(prefs: &AppPrefs) -> AppResult<()> {
    let path = prefs_path()?;
    let raw = serde_json::to_string_pretty(prefs)?;
    fs::write(&path, raw)
        .map_err(|e| AppError::io(format!("Cannot write {}: {e}", path.display())))?;
    Ok(())
}

/// Copy preferences from whichever older location exists into the new one.
/// Returns `true` when a migration actually happened.
pub fn migrate_legacy_prefs() -> AppResult<bool> {
    let new_path = prefs_path()?;
    if new_path.exists() {
        return Ok(false);
    }

    // Newest source first: the 2.0 build that still used the old name.
    let mut sources: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = previous_app_dir() {
        sources.push(dir.join("prefs.json"));
    }
    if let Ok(path) = legacy_prefs_path() {
        sources.push(path);
    }

    for old_path in sources {
        if !old_path.exists() {
            continue;
        }

        let raw = fs::read_to_string(&old_path)
            .map_err(|e| AppError::io(format!("Cannot read legacy prefs: {e}")))?;
        // Validate before writing; a corrupt legacy file must not brick startup.
        let prefs: AppPrefs = serde_json::from_str(&raw)
            .map_err(|e| AppError::io(format!("Legacy prefs are not valid JSON: {e}")))?;
        let serialized = serde_json::to_string_pretty(&prefs)?;
        fs::write(&new_path, serialized)
            .map_err(|e| AppError::io(format!("Cannot write migrated prefs: {e}")))?;

        let _ = fs::rename(&old_path, old_path.with_extension("json.migrated"));
        migrate_token_file(old_path.parent());
        return Ok(true);
    }

    Ok(false)
}

/// A fallback-file session from an older build moves with the preferences.
fn migrate_token_file(old_dir: Option<&Path>) {
    let (Some(old_dir), Ok(new_path)) = (old_dir, token_path()) else {
        return;
    };
    let old_path = old_dir.join("session.json");
    if old_path.exists() && !new_path.exists() {
        let _ = fs::copy(&old_path, &new_path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = fs::metadata(&new_path) {
                let mut perms = metadata.permissions();
                perms.set_mode(0o600);
                let _ = fs::set_permissions(&new_path, perms);
            }
        }
    }
}

/// `<library>/.manager` — app bookkeeping that is never part of a model repo.
pub fn manager_dir(library: &Path) -> PathBuf {
    library.join(".manager")
}

pub fn state_dir(library: &Path) -> PathBuf {
    manager_dir(library).join("state")
}

pub fn model_state_path(library: &Path, model_id: &str) -> PathBuf {
    state_dir(library).join(format!("{model_id}.json"))
}

pub fn push_log_path(library: &Path) -> PathBuf {
    manager_dir(library).join("push-log.jsonl")
}

#[allow(dead_code)] // kept for future capability caching; introspection is disabled
pub fn capabilities_path(library: &Path) -> PathBuf {
    manager_dir(library).join("capabilities.json")
}

/// Resolve the configured library folder, or a helpful error.
pub fn library_dir(prefs: &AppPrefs) -> AppResult<PathBuf> {
    let raw = prefs.library_folder.as_ref().ok_or_else(|| {
        AppError::invalid("Library folder not configured. Set it in Preferences first.")
    })?;
    Ok(PathBuf::from(raw))
}

pub fn ensure_manager_dirs(library: &Path) -> AppResult<()> {
    fs::create_dir_all(state_dir(library))
        .map_err(|e| AppError::io(format!("Cannot create bookkeeping directory: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_paths_live_outside_model_directories() {
        let lib = Path::new("/tmp/library");
        assert_eq!(manager_dir(lib), PathBuf::from("/tmp/library/.manager"));
        assert_eq!(
            model_state_path(lib, "123"),
            PathBuf::from("/tmp/library/.manager/state/123.json")
        );
    }
}
