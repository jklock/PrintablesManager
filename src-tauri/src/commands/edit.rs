//! Editing a model locally.
//!
//! The model folder is the source of truth: name, description and tags live in
//! `metadata.json`, the printable files in `files/`, pictures in `images/`.
//! Every mutation here writes the folder and records a git commit, so an edit
//! is always reviewable and revertable from the History card.

use crate::config;
use crate::error::{AppError, AppResult};
use crate::git;
use crate::library::locator;
use crate::models::{AppPrefs, MetadataFile, MetadataV1};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::State;
use tauri_plugin_dialog::DialogExt;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelArgs {
    pub model_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditDetailsArgs {
    pub model_id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileArgs {
    pub model_id: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct ImportResult {
    pub imported: Vec<String>,
    pub skipped: Vec<String>,
    pub metadata: MetadataV1,
}

fn model_dir(prefs: &State<'_, Mutex<AppPrefs>>, model_id: &str) -> AppResult<PathBuf> {
    let snapshot = prefs
        .lock()
        .map(|p| p.clone())
        .map_err(|_| AppError::internal("Preferences lock poisoned"))?;
    let library = config::library_dir(&snapshot)?;
    locator::find_model_dir(&library, model_id)
}

fn commit(dir: &Path, message: &str) -> AppResult<()> {
    let _ = git::commit_all(dir, message)?;
    Ok(())
}

/// Trim, drop empties and de-duplicate tags case-insensitively, keeping the
/// order the user typed.
pub fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for tag in tags {
        let trimmed = tag.trim().trim_start_matches('#').trim().to_string();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            out.push(trimmed);
        }
    }
    out
}

/// `a.stl` -> `a.stl`, and a second `a.stl` -> `a (1).stl`.
pub fn unique_name(dir: &Path, wanted: &str) -> String {
    let base = crate::printables::clone::safe_filename(wanted);
    if !dir.join(&base).exists() {
        return base;
    }
    let path = Path::new(&base);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for i in 1..1000 {
        let candidate = format!("{stem} ({i}){ext}");
        if !dir.join(&candidate).exists() {
            return candidate;
        }
    }
    base
}

/// Everything the editor needs for one model.
#[tauri::command]
pub fn get_model_edit(prefs: State<'_, Mutex<AppPrefs>>, args: ModelArgs) -> AppResult<MetadataV1> {
    let dir = model_dir(&prefs, &args.model_id)?;
    locator::read_metadata(&dir)
}

/// Save the editable fields and commit. File remote ids and stats are kept.
pub fn apply_details(
    dir: &Path,
    name: &str,
    description: &str,
    tags: Vec<String>,
) -> AppResult<MetadataV1> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::invalid("A model needs a name."));
    }

    let mut metadata = locator::read_metadata(dir)?;
    metadata.name = name.to_string();
    metadata.description = description.trim().to_string();
    metadata.tags = normalize_tags(tags);

    locator::write_metadata(dir, &metadata)?;
    commit(dir, &format!("Edited details: {}", metadata.name))?;
    Ok(metadata)
}

/// Remove a file from the folder and from the manifest, then commit.
pub fn apply_delete_file(dir: &Path, name: &str) -> AppResult<MetadataV1> {
    let mut metadata = locator::read_metadata(dir)?;

    let index = metadata
        .files
        .iter()
        .position(|f| f.name == name)
        .ok_or_else(|| AppError::not_found(format!("No file called {name} in this model")))?;
    let entry = metadata.files.remove(index);

    let path = dir.join(&entry.local_path);
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|e| AppError::io(format!("Cannot delete {}: {e}", path.display())))?;
    }

    locator::write_metadata(dir, &metadata)?;
    commit(dir, &format!("Removed file {}", entry.name))?;
    Ok(metadata)
}

/// Mark one image as the cover, demoting any previous cover, then commit.
pub fn apply_cover(dir: &Path, name: &str) -> AppResult<MetadataV1> {
    let mut metadata = locator::read_metadata(dir)?;

    if !metadata.images.iter().any(|i| i.name == name) {
        return Err(AppError::not_found(format!(
            "No image called {name} in this model"
        )));
    }

    for image in metadata.images.iter_mut() {
        if image.name == name {
            image.kind = "cover".to_string();
        } else if image.kind == "cover" {
            image.kind = "photo".to_string();
        }
    }

    locator::write_metadata(dir, &metadata)?;
    commit(dir, &format!("Set cover image to {name}"))?;
    Ok(metadata)
}

#[tauri::command]
pub fn update_model_details(
    prefs: State<'_, Mutex<AppPrefs>>,
    args: EditDetailsArgs,
) -> AppResult<MetadataV1> {
    let dir = model_dir(&prefs, &args.model_id)?;
    apply_details(&dir, &args.name, &args.description, args.tags)
}

/// Copy user-chosen files into `files/` and register them as local-only
/// (no remote id), so the push planner offers to upload them.
#[tauri::command]
pub async fn import_model_files(
    app: tauri::AppHandle,
    prefs: State<'_, Mutex<AppPrefs>>,
    args: ModelArgs,
) -> AppResult<ImportResult> {
    let dir = model_dir(&prefs, &args.model_id)?;
    let files_dir = dir.join("files");
    fs::create_dir_all(&files_dir)?;

    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .set_title("Add files to this model")
        .pick_files(move |paths| {
            let _ = tx.send(paths);
        });
    let picked = rx
        .recv()
        .map_err(|e| AppError::internal(e.to_string()))?
        .unwrap_or_default();

    let mut metadata = locator::read_metadata(&dir)?;
    let mut imported = Vec::new();
    let mut skipped = Vec::new();

    for file in picked {
        let source = PathBuf::from(file.to_string());
        let Some(raw_name) = source.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let target_name = unique_name(&files_dir, &raw_name);
        let target = files_dir.join(&target_name);

        match fs::copy(&source, &target) {
            Ok(_) => {
                let size = target.metadata().map(|m| m.len()).unwrap_or(0);
                metadata.files.push(MetadataFile {
                    name: target_name.clone(),
                    kind: crate::printables::clone::kind_for_name(&target_name),
                    size_bytes: size,
                    local_path: format!("files/{target_name}"),
                    remote_id: None,
                });
                imported.push(target_name);
            }
            Err(err) => skipped.push(format!("{raw_name}: {err}")),
        }
    }

    if imported.is_empty() {
        return Ok(ImportResult {
            imported,
            skipped,
            metadata,
        });
    }

    // Keep the manifest ordered by filename for stable diffs.
    metadata.files.sort_by(|a, b| a.name.cmp(&b.name));
    locator::write_metadata(&dir, &metadata)?;
    commit(
        &dir,
        &format!(
            "Added {} file{}",
            imported.len(),
            if imported.len() == 1 { "" } else { "s" }
        ),
    )?;

    Ok(ImportResult {
        imported,
        skipped,
        metadata,
    })
}

/// Remove a file from the folder and from the manifest.
#[tauri::command]
pub fn delete_model_file(
    prefs: State<'_, Mutex<AppPrefs>>,
    args: FileArgs,
) -> AppResult<MetadataV1> {
    let dir = model_dir(&prefs, &args.model_id)?;
    apply_delete_file(&dir, &args.name)
}

/// Choose which downloaded picture is the cover.
#[tauri::command]
pub fn set_model_cover(prefs: State<'_, Mutex<AppPrefs>>, args: FileArgs) -> AppResult<MetadataV1> {
    let dir = model_dir(&prefs, &args.model_id)?;
    apply_cover(&dir, &args.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_model() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("po-editmodel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("files")).unwrap();
        std::fs::create_dir_all(dir.join("images")).unwrap();
        std::fs::write(dir.join("files/part.stl"), b"solid part").unwrap();
        std::fs::write(dir.join("images/cover.png"), b"png").unwrap();
        std::fs::write(dir.join("images/alt.png"), b"png").unwrap();

        let mut meta = crate::library::locator::empty_metadata("42");
        meta.name = "Original".into();
        meta.description = "old".into();
        meta.tags = vec!["old".into()];
        meta.files = vec![MetadataFile {
            name: "part.stl".into(),
            kind: "stl".into(),
            size_bytes: 10,
            local_path: "files/part.stl".into(),
            remote_id: Some("remote-1".into()),
        }];
        meta.images = vec![
            crate::models::MetadataImage {
                name: "cover.png".into(),
                kind: "cover".into(),
                local_path: "images/cover.png".into(),
                remote_id: None,
            },
            crate::models::MetadataImage {
                name: "alt.png".into(),
                kind: "photo".into(),
                local_path: "images/alt.png".into(),
                remote_id: None,
            },
        ];
        crate::library::locator::write_metadata(&dir, &meta).unwrap();
        crate::git::commit_all(&dir, "Initial").unwrap();
        dir
    }

    #[test]
    fn editing_details_writes_metadata_and_commits() {
        let dir = scratch_model();
        let updated = apply_details(
            &dir,
            "  New Name  ",
            "  ## Hi\n\n- a  ",
            vec!["a".into(), "A".into(), " b ".into()],
        )
        .unwrap();

        assert_eq!(updated.name, "New Name");
        assert_eq!(updated.description, "## Hi\n\n- a");
        assert_eq!(updated.tags, vec!["a".to_string(), "b".to_string()]);
        // Remote ids and file entries survive an edit.
        assert_eq!(updated.files.len(), 1);
        assert_eq!(updated.files[0].remote_id.as_deref(), Some("remote-1"));

        let commits = crate::git::log(&dir, 10).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].summary, "Edited details: New Name");
        assert!(crate::git::status(&dir).unwrap().clean);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn editing_rejects_a_blank_name() {
        let dir = scratch_model();
        let err = apply_details(&dir, "   ", "x", Vec::new()).unwrap_err();
        assert_eq!(err.code, "INVALID_INPUT");
        // Nothing was committed.
        assert_eq!(crate::git::log(&dir, 10).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_a_file_removes_it_from_disk_and_manifest() {
        let dir = scratch_model();
        let updated = apply_delete_file(&dir, "part.stl").unwrap();

        assert!(updated.files.is_empty());
        assert!(!dir.join("files/part.stl").exists());
        assert_eq!(
            crate::git::log(&dir, 10).unwrap()[0].summary,
            "Removed file part.stl"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_an_unknown_file_is_a_clear_error() {
        let dir = scratch_model();
        let err = apply_delete_file(&dir, "nope.stl").unwrap_err();
        assert_eq!(err.code, "NOT_FOUND");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setting_a_cover_demotes_the_previous_one() {
        let dir = scratch_model();
        let updated = apply_cover(&dir, "alt.png").unwrap();

        let cover = updated.images.iter().find(|i| i.kind == "cover").unwrap();
        assert_eq!(cover.name, "alt.png");
        assert_eq!(
            updated.images.iter().filter(|i| i.kind == "cover").count(),
            1,
            "exactly one cover"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tags_are_trimmed_deduped_and_hash_stripped() {
        let tags = normalize_tags(vec![
            "  #Vase ".into(),
            "vase".into(),
            "".into(),
            "   ".into(),
            "Planter".into(),
        ]);
        assert_eq!(tags, vec!["Vase".to_string(), "Planter".to_string()]);
    }

    #[test]
    fn duplicate_filenames_get_a_numeric_suffix() {
        let dir = std::env::temp_dir().join(format!("po-edit-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_name(&dir, "part.stl"), "part.stl");

        fs::write(dir.join("part.stl"), b"x").unwrap();
        assert_eq!(unique_name(&dir, "part.stl"), "part (1).stl");

        fs::write(dir.join("part (1).stl"), b"x").unwrap();
        assert_eq!(unique_name(&dir, "part.stl"), "part (2).stl");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsafe_characters_are_sanitised_when_importing() {
        let dir = std::env::temp_dir().join(format!("po-edit-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_name(&dir, "a:b/c?.stl"), "a_b_c_.stl");
        let _ = fs::remove_dir_all(&dir);
    }
}
