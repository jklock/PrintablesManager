//! Backup of every model's git history to iCloud Drive.
//!
//! The history is the valuable part: `files/` and `images/` are already inside
//! `.git`, compressed, so a backup is a copy of each `.git` directory plus its
//! `metadata.json`. Re-running overwrites the mirror instead of piling up
//! timestamped copies, so iCloud only uploads changed objects.

use crate::error::{AppError, AppResult};
use crate::library::locator;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct BackupSummary {
    pub destination: String,
    pub models: usize,
    pub bytes: u64,
    /// Models without a repository (git disabled or never initialised).
    pub skipped: Vec<String>,
}

/// `~/Library/Mobile Documents/com~apple~CloudDocs`
pub fn icloud_root() -> AppResult<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| AppError::io("Cannot determine home directory"))?;
    let root = home
        .join("Library")
        .join("Mobile Documents")
        .join("com~apple~CloudDocs");
    if !root.is_dir() {
        return Err(AppError::unsupported(
            "iCloud Drive is not set up for this account (no com~apple~CloudDocs folder).",
        ));
    }
    Ok(root)
}

fn copy_tree(src: &Path, dst: &Path) -> AppResult<u64> {
    fs::create_dir_all(dst)?;
    let mut bytes = 0u64;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = entry.metadata()?;
        if meta.is_dir() {
            bytes += copy_tree(&from, &to)?;
        } else if meta.is_file() {
            // Git objects are read-only; a second backup must be able to
            // replace them instead of failing with EACCES.
            if to.exists() {
                let _ = fs::remove_file(&to);
            }
            fs::copy(&from, &to)?;
            bytes += meta.len();
        }
    }
    Ok(bytes)
}

/// Mirror every model's `.git` and `metadata.json` under `root`.
pub fn backup_to(library: &Path, root: &Path) -> AppResult<BackupSummary> {
    let destination = root.join("Printables Manager Backups");
    fs::create_dir_all(&destination)?;

    let mut models = 0usize;
    let mut bytes = 0u64;
    let mut skipped = Vec::new();

    for dir in locator::model_dirs(library)? {
        let Ok(meta) = locator::read_metadata(&dir) else {
            continue;
        };
        if !crate::git::is_repo(&dir) {
            skipped.push(meta.model_id.clone());
            continue;
        }

        let target = destination.join(&meta.model_id);
        fs::create_dir_all(&target)?;
        bytes += copy_tree(&dir.join(".git"), &target.join(".git"))?;
        fs::copy(dir.join("metadata.json"), target.join("metadata.json"))?;
        bytes += fs::metadata(dir.join("metadata.json"))
            .map(|m| m.len())
            .unwrap_or(0);
        models += 1;
    }

    let manifest = serde_json::json!({
        "created_at": chrono::Utc::now().to_rfc3339(),
        "library": library.display().to_string(),
        "models": models,
        "bytes": bytes,
    });
    fs::write(
        destination.join("backup.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    Ok(BackupSummary {
        destination: destination.display().to_string(),
        models,
        bytes,
        skipped,
    })
}

/// Backup the configured library into iCloud Drive.
#[tauri::command]
pub fn backup_to_icloud(
    prefs: tauri::State<'_, std::sync::Mutex<crate::models::AppPrefs>>,
) -> AppResult<BackupSummary> {
    let library = {
        let snapshot = prefs
            .lock()
            .map_err(|_| AppError::internal("Preferences lock poisoned"))?
            .clone();
        crate::config::library_dir(&snapshot)?
    };
    backup_to(&library, &icloud_root()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MetadataFile;

    fn scratch(library: &Path) {
        let dir = library.join("42-demo");
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::write(dir.join("files/a.stl"), b"solid a").unwrap();
        let mut meta = locator::empty_metadata("42");
        meta.name = "Demo".into();
        meta.files = vec![MetadataFile {
            name: "a.stl".into(),
            kind: "stl".into(),
            size_bytes: 7,
            local_path: "files/a.stl".into(),
            remote_id: None,
        }];
        locator::write_metadata(&dir, &meta).unwrap();
        crate::git::commit_all(&dir, "Initial").unwrap();
    }

    #[test]
    fn backup_copies_git_and_metadata_and_is_repeatable() {
        let base = std::env::temp_dir().join(format!("po-backup-{}", uuid::Uuid::new_v4()));
        let library = base.join("library");
        let cloud = base.join("cloud");
        fs::create_dir_all(&library).unwrap();
        fs::create_dir_all(&cloud).unwrap();
        scratch(&library);

        let first = backup_to(&library, &cloud).unwrap();
        assert_eq!(first.models, 1);
        assert!(first.bytes > 0);
        assert!(cloud.join("Printables Manager Backups/42/.git").is_dir());
        assert!(cloud
            .join("Printables Manager Backups/42/metadata.json")
            .is_file());
        assert!(cloud
            .join("Printables Manager Backups/backup.json")
            .is_file());

        let second = backup_to(&library, &cloud).unwrap();
        assert_eq!(second.models, 1, "re-running must not duplicate models");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn models_without_a_repository_are_reported_not_fatal() {
        let base = std::env::temp_dir().join(format!("po-backup-{}", uuid::Uuid::new_v4()));
        let library = base.join("library");
        let cloud = base.join("cloud");
        let dir = library.join("7-plain");
        fs::create_dir_all(&dir).unwrap();
        locator::write_metadata(&dir, &locator::empty_metadata("7")).unwrap();
        fs::create_dir_all(&cloud).unwrap();

        let summary = backup_to(&library, &cloud).unwrap();
        assert_eq!(summary.models, 0);
        assert_eq!(summary.skipped, vec!["7".to_string()]);
        let _ = fs::remove_dir_all(&base);
    }
}
