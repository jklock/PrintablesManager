//! One place that knows where a model lives on disk.
//!
//! 1.x duplicated this scan in `commands/delete.rs` and `commands/export.rs`,
//! and the frontend re-derived the folder from the model URL — which breaks
//! whenever the on-disk slug differs from the URL slug. Everything now goes
//! through here.

use crate::error::{AppError, AppResult};
use crate::models::{MetadataV1, ModelStats};
use std::fs;
use std::path::{Path, PathBuf};

pub fn model_dirs(library: &Path) -> AppResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = fs::read_dir(library)
        .map_err(|e| AppError::io(format!("Cannot read {}: {e}", library.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| AppError::io(e.to_string()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path
            .file_name()
            .map(|n| n.to_string_lossy().starts_with('.'))
            .unwrap_or(false)
        {
            continue;
        }
        if path.join("metadata.json").exists() {
            out.push(path);
        }
    }
    Ok(out)
}

pub fn read_metadata(model_dir: &Path) -> AppResult<MetadataV1> {
    let path = model_dir.join("metadata.json");
    let raw = fs::read_to_string(&path)
        .map_err(|e| AppError::io(format!("Cannot read {}: {e}", path.display())))?;
    serde_json::from_str(&raw).map_err(|e| {
        AppError::io(format!(
            "{} is not valid model metadata: {e}",
            path.display()
        ))
    })
}

pub fn write_metadata(model_dir: &Path, metadata: &MetadataV1) -> AppResult<()> {
    let path = model_dir.join("metadata.json");
    let raw = serde_json::to_string_pretty(metadata)?;
    fs::write(&path, raw)
        .map_err(|e| AppError::io(format!("Cannot write {}: {e}", path.display())))?;
    Ok(())
}

/// Find a model folder by the `model_id` inside its metadata.
pub fn try_find_model_dir(library: &Path, model_id: &str) -> Option<PathBuf> {
    for dir in model_dirs(library).ok()? {
        if let Ok(meta) = read_metadata(&dir) {
            if meta.model_id == model_id {
                return Some(dir);
            }
        }
    }
    None
}

pub fn find_model_dir(library: &Path, model_id: &str) -> AppResult<PathBuf> {
    try_find_model_dir(library, model_id)
        .ok_or_else(|| AppError::not_found(format!("Model {model_id} is not in this library")))
}

/// `{model_id}-{slug}` with a filesystem-safe slug.
pub fn new_model_dir(library: &Path, model_id: &str, slug: &str) -> PathBuf {
    let slug = if slug.trim().is_empty() {
        "model"
    } else {
        slug.trim()
    };
    let slug = crate::printables::clone::safe_filename(slug);
    library.join(format!("{model_id}-{slug}"))
}

/// Used when metadata.json is missing or unreadable.
pub fn empty_metadata(model_id: &str) -> MetadataV1 {
    MetadataV1 {
        schema_version: 2,
        source: "printables.com".to_string(),
        model_id: model_id.to_string(),
        slug: String::new(),
        name: model_id.to_string(),
        url: format!("https://www.printables.com/model/{model_id}"),
        author: String::new(),
        tags: Vec::new(),
        stats: ModelStats {
            likes: None,
            downloads: None,
            rating: None,
            published_date: None,
        },
        description: String::new(),
        cloned_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        files: Vec::new(),
        images: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_model_dir_sanitises_the_slug() {
        let lib = Path::new("/tmp/lib");
        assert_eq!(
            new_model_dir(lib, "42", "a/b:c"),
            PathBuf::from("/tmp/lib/42-a_b_c")
        );
        assert_eq!(
            new_model_dir(lib, "42", "  "),
            PathBuf::from("/tmp/lib/42-model")
        );
    }
}
