//! Builds the reviewable list of changes that would be pushed to printables.com.
//!
//! Nothing here writes anything. The plan is a pure diff between the model
//! folder on disk (which is under git) and the model as it exists on
//! printables.com right now.

use crate::error::{AppError, AppResult};
use crate::git;
use crate::library::locator;
use crate::models::{MetadataFile, MetadataV1};
use crate::printables::api::PrintablesClient;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct PushOperation {
    pub id: String,
    /// "add_file" | "delete_file" | "update_model" | "add_image"
    pub kind: String,
    pub summary: String,
    pub path: Option<String>,
    pub remote_id: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PushPlan {
    pub model_id: String,
    pub model_dir: String,
    pub plan_id: String,
    pub operations: Vec<PushOperation>,
    pub warnings: Vec<String>,
    /// True when printables.com has files this folder has never seen, so
    /// pushing could delete or duplicate someone else's work.
    pub remote_changed: bool,
    /// False when the user has not enabled writes; the UI shows a notice.
    pub write_enabled: bool,
}

fn op_id(kind: &str, key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update(b"|");
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())[..12].to_string()
}

pub fn build(
    client: &PrintablesClient,
    library: &Path,
    model_id: &str,
    write_enabled: bool,
) -> AppResult<PushPlan> {
    let model_dir = locator::find_model_dir(library, model_id)?;
    let meta = locator::read_metadata(&model_dir)?;
    let mut warnings = Vec::new();
    let mut operations: Vec<PushOperation> = Vec::new();

    // ---------------------------------------------------- remote vs local
    let remote_files = client.list_model_files(model_id)?;
    let remote_ids: HashSet<String> = remote_files.iter().map(|f| f.id.clone()).collect();
    let known_ids: HashSet<String> = meta
        .files
        .iter()
        .filter_map(|f| f.remote_id.clone())
        .collect();

    let unknown_remote: Vec<&String> = remote_ids.difference(&known_ids).collect();
    let remote_changed = !unknown_remote.is_empty();
    if remote_changed {
        warnings.push(format!(
            "printables.com has {} file(s) this folder has never downloaded. Pull before pushing.",
            unknown_remote.len()
        ));
    }

    let files_dir = model_dir.join("files");
    let on_disk: HashSet<String> = fs::read_dir(&files_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.ends_with(".part"))
                .collect()
        })
        .unwrap_or_default();

    // Deletions: we know a remote id, but the file is gone locally.
    for file in &meta.files {
        let Some(remote_id) = file.remote_id.clone() else {
            continue;
        };
        if on_disk.contains(&file.name) {
            continue;
        }
        operations.push(PushOperation {
            id: op_id("delete_file", &remote_id),
            kind: "delete_file".into(),
            summary: format!("Delete “{}” from printables.com", file.name),
            path: Some(file.local_path.clone()),
            remote_id: Some(remote_id),
            detail: Some(format!("File id {} · {}", file.id_hint(), file.kind)),
        });
    }

    // Additions: file present locally with no remote id.
    for file in &meta.files {
        if file.remote_id.is_some() || !on_disk.contains(&file.name) {
            continue;
        }
        operations.push(PushOperation {
            id: op_id("add_file", &file.name),
            kind: "add_file".into(),
            summary: format!("Upload “{}” as a new file", file.name),
            path: Some(file.local_path.clone()),
            remote_id: None,
            detail: Some(format!(
                "{:.1} MB · {}",
                file.size_bytes as f64 / 1_048_576.0,
                file.kind
            )),
        });
    }

    // Files on disk that metadata.json does not know about at all.
    let known_names: HashSet<&str> = meta.files.iter().map(|f| f.name.as_str()).collect();
    for name in &on_disk {
        if known_names.contains(name.as_str()) {
            continue;
        }
        operations.push(PushOperation {
            id: op_id("add_file", name),
            kind: "add_file".into(),
            summary: format!("Upload “{name}” as a new file"),
            path: Some(format!("files/{name}")),
            remote_id: None,
            detail: Some("Not listed in metadata.json".into()),
        });
        warnings.push(format!(
            "“{name}” exists on disk but is not in metadata.json; it will be uploaded and added to the manifest."
        ));
    }

    // -------------------------------------------------------- metadata diff
    if let Some(head) = git::head_file(&model_dir, "metadata.json") {
        if let Ok(head_meta) = serde_json::from_slice::<MetadataV1>(&head) {
            for (field, old, new) in [
                ("name", head_meta.name.clone(), meta.name.clone()),
                (
                    "description",
                    head_meta.description.clone(),
                    meta.description.clone(),
                ),
                ("tags", head_meta.tags.join(", "), meta.tags.join(", ")),
            ] {
                if old != new {
                    operations.push(PushOperation {
                        id: op_id("update_model", field),
                        kind: "update_model".into(),
                        summary: format!("Update the model's {field}"),
                        path: Some("metadata.json".into()),
                        remote_id: None,
                        detail: Some(format!("{} → {}", preview(&old), preview(&new))),
                    });
                }
            }
        }
    }

    // Images: local files with no remote id.
    for image in &meta.images {
        if image.remote_id.is_some() {
            continue;
        }
        let full = model_dir.join(&image.local_path);
        if !full.exists() {
            continue;
        }
        operations.push(PushOperation {
            id: op_id("add_image", &image.name),
            kind: "add_image".into(),
            summary: format!("Upload image “{}”", image.name),
            path: Some(image.local_path.clone()),
            remote_id: None,
            detail: Some(format!("kind: {}", image.kind)),
        });
    }

    let mut hasher = Sha256::new();
    for op in &operations {
        hasher.update(op.id.as_bytes());
    }
    let plan_id = format!("{:x}", hasher.finalize())[..16].to_string();

    Ok(PushPlan {
        model_id: model_id.to_string(),
        model_dir: model_dir.display().to_string(),
        plan_id,
        operations,
        warnings,
        remote_changed,
        write_enabled,
    })
}

fn preview(text: &str) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= 60 {
        flat
    } else {
        format!("{}…", flat.chars().take(60).collect::<String>())
    }
}

trait FileIdHint {
    fn id_hint(&self) -> &str;
}

impl FileIdHint for MetadataFile {
    fn id_hint(&self) -> &str {
        self.remote_id.as_deref().unwrap_or("local")
    }
}

/// Guard used by the executor: a plan is only applied when it still matches
/// what the user reviewed.
pub fn verify(plan: &PushPlan, library: &Path) -> AppResult<()> {
    if !Path::new(&plan.model_dir).exists() {
        return Err(AppError::not_found(
            "The model folder has moved or was deleted.",
        ));
    }
    if !library.exists() {
        return Err(AppError::not_found("The library folder is unavailable."));
    }
    Ok(())
}
