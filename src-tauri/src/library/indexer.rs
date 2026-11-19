//! Library indexer: walks the library folder, parses `metadata.json`, and
//! produces the `ModelSummary` list the UI renders.
//!
//! Each summary now carries the model's real on-disk directory plus its git
//! state, so the frontend never has to reconstruct paths from a URL.

use crate::git;
use crate::library::locator;
use crate::models::{FileEntry, ImageAsset, LibraryIndex, LibraryTotals, ModelSummary};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

pub fn index_library(library_dir: &Path) -> Result<LibraryIndex, String> {
    index_library_with(library_dir, true)
}

/// `with_git_state = false` skips status computation (used by fast paths).
pub fn index_library_with(
    library_dir: &Path,
    with_git_state: bool,
) -> Result<LibraryIndex, String> {
    if !library_dir.exists() {
        return Err(format!(
            "Library directory does not exist: {}",
            library_dir.display()
        ));
    }

    let dirs = locator::model_dirs(library_dir).map_err(|e| {
        // An external/removable library needs one macOS consent click. Say so
        // instead of surfacing a bare "Operation not permitted".
        let message = e.message;
        if message.contains("Operation not permitted") || message.contains("Permission denied") {
            format!(
                "macOS is blocking access to {}. Answer Allow on the system prompt \
                 (or grant it in System Settings -> Privacy & Security -> Files and Folders), \
                 then rescan.",
                library_dir.display()
            )
        } else {
            message
        }
    })?;
    let mut models: Vec<ModelSummary> = Vec::new();
    let mut total_files: usize = 0;
    let mut total_bytes: u64 = 0;

    for dir in dirs {
        match parse_model(&dir, with_git_state) {
            Ok(summary) => {
                total_files += summary.files.len();
                total_bytes += (summary.size_mb * 1_048_576.0) as u64;
                models.push(summary);
            }
            Err(e) => {
                eprintln!("[indexer] skipping {:?}: {}", dir, e.message);
            }
        }
    }

    models.sort_by(|a, b| b.added.cmp(&a.added));

    let model_count = models.len();
    Ok(LibraryIndex {
        models,
        totals: LibraryTotals {
            models: model_count,
            files: total_files,
            bytes_used: total_bytes,
            bytes_capacity: 4_294_967_296,
        },
    })
}

fn parse_model(
    model_dir: &Path,
    with_git_state: bool,
) -> Result<ModelSummary, crate::error::AppError> {
    let meta = locator::read_metadata(model_dir)?;

    let files: Vec<FileEntry> = meta
        .files
        .iter()
        .map(|f| {
            let full = model_dir.join(&f.local_path);
            FileEntry {
                n: f.name.clone(),
                mb: f.size_bytes as f64 / 1_048_576.0,
                modified: file_modified_iso(&full),
            }
        })
        .collect();

    // Prefer the real file extension over the recorded kind.
    let kinds: Vec<String> = meta
        .files
        .iter()
        .map(|f| {
            f.name
                .rsplit('.')
                .next()
                .unwrap_or("unknown")
                .to_lowercase()
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let size_mb: f64 = meta
        .files
        .iter()
        .map(|f| f.size_bytes as f64 / 1_048_576.0)
        .sum();

    let cover_asset_url = meta
        .images
        .iter()
        .find(|img| img.kind == "cover")
        .or_else(|| meta.images.first())
        .and_then(|img| {
            let full = model_dir.join(&img.local_path);
            if full.exists() {
                Some(asset_url(&full))
            } else {
                None
            }
        });

    let images: Vec<ImageAsset> = meta
        .images
        .iter()
        .filter_map(|img| {
            let full = model_dir.join(&img.local_path);
            if full.exists() {
                Some(ImageAsset {
                    url: asset_url(&full),
                    kind: img.kind.clone(),
                    name: img.name.clone(),
                })
            } else {
                None
            }
        })
        .collect();

    let (has_git, dirty) = if with_git_state {
        let has_git = git::is_repo(model_dir);
        let dirty = if has_git {
            git::status(model_dir).map(|s| !s.clean).unwrap_or(false)
        } else {
            false
        };
        (has_git, dirty)
    } else {
        (git::is_repo(model_dir), false)
    };

    let sync_state = if !has_git {
        "no-git"
    } else if dirty {
        "dirty"
    } else {
        "clean"
    };

    Ok(ModelSummary {
        id: meta.model_id.clone(),
        name: meta.name.clone(),
        creator: meta.author.clone(),
        tags: meta.tags.clone(),
        added: meta.cloned_at.clone(),
        source: meta.url.clone(),
        description: meta.description.clone(),
        files,
        kinds,
        size_mb,
        cover_asset_url,
        images,
        dir: model_dir.display().to_string(),
        has_git,
        dirty,
        sync_state: sync_state.to_string(),
    })
}

fn asset_url(path: &Path) -> String {
    format!("asset://localhost/{}", path.display())
}

fn file_modified_iso(path: &Path) -> String {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(d.as_secs() as i64, 0)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "unknown".into())
        })
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MetadataFile, MetadataV1, ModelStats};

    fn write_model(dir: &Path, id: &str) {
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join("images")).unwrap();
        fs::write(dir.join("files/part.stl"), b"solid part").unwrap();
        let meta = MetadataV1 {
            schema_version: 2,
            source: "printables.com".into(),
            model_id: id.into(),
            slug: "demo".into(),
            name: "Demo".into(),
            url: format!("https://www.printables.com/model/{id}-demo"),
            author: "someone".into(),
            tags: vec!["tag".into()],
            stats: ModelStats {
                likes: None,
                downloads: None,
                rating: None,
                published_date: None,
            },
            description: "desc".into(),
            remote_description_html: String::new(),
            cloned_at: "2026-01-01T00:00:00Z".into(),
            files: vec![MetadataFile {
                name: "part.stl".into(),
                kind: "stl".into(),
                size_bytes: 10,
                local_path: "files/part.stl".into(),
                remote_id: Some("999".into()),
            }],
            images: Vec::new(),
        };
        fs::write(
            dir.join("metadata.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn summary_exposes_real_directory_and_git_state() {
        let lib = std::env::temp_dir().join(format!("po-index-{}", uuid::Uuid::new_v4()));
        let dir = lib.join("42-demo");
        write_model(&dir, "42");

        let index = index_library(&lib).unwrap();
        assert_eq!(index.models.len(), 1);
        assert_eq!(index.models[0].dir, dir.display().to_string());
        assert_eq!(index.models[0].sync_state, "no-git");

        crate::git::commit_all(&dir, "Initial").unwrap();
        let index = index_library(&lib).unwrap();
        assert!(index.models[0].has_git);
        assert_eq!(index.models[0].sync_state, "clean");

        fs::write(dir.join("files/part.stl"), b"solid changed").unwrap();
        let index = index_library(&lib).unwrap();
        assert_eq!(index.models[0].sync_state, "dirty");

        let _ = fs::remove_dir_all(&lib);
    }
}
