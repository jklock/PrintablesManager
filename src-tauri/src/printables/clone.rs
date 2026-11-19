//! Model pull pipeline: download one model from printables.com into the library.
//!
//! 2.0 differences from the 1.x clone:
//!   * idempotent — files already present at the expected size are skipped,
//!   * resumable — downloads land in `.part` files and are renamed on success,
//!   * no duplicate folders — an existing folder for the same `model_id` is
//!     reused even if the slug in the URL differs,
//!   * per-file failures are reported instead of silently dropped,
//!   * local-only files are preserved and recorded with `remote_id: None`,
//!   * local edits to `metadata.json` survive a pull,
//!   * every pull is committed to the model's git repository.

use crate::error::AppResult;
use crate::git;
use crate::library::locator;
use crate::models::{MetadataFile, MetadataImage, MetadataV1, ModelStats};
use crate::printables::api::{PrintablesClient, RemoteImage};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedModelUrl {
    pub id: String,
    pub slug: String,
}

#[derive(Debug, Clone)]
pub struct PullOptions {
    /// Re-download everything even when a file looks unchanged.
    pub force: bool,
    /// Commit the result to the model's git repository.
    pub git: bool,
}

impl Default for PullOptions {
    fn default() -> Self {
        Self {
            force: false,
            git: true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PullOutcome {
    pub model_id: String,
    pub model_dir: String,
    pub name: String,
    pub created: bool,
    pub files_downloaded: usize,
    pub files_skipped: usize,
    pub files_local_only: usize,
    pub images_downloaded: usize,
    pub images_skipped: usize,
    pub warnings: Vec<String>,
    pub committed: Option<String>,
}

/// Parses a Printables model URL into `{ id, slug }`.
pub fn parse_model_url(url: &str) -> Option<ParsedModelUrl> {
    let re =
        Regex::new(r"^https?://(?:www\.)?printables\.com/model/(?P<id>\d+)(?:-(?P<slug>[^/?#]+))?")
            .ok()?;
    let caps = re.captures(url.trim())?;
    let id = caps.name("id")?.as_str().to_string();
    let slug = caps
        .name("slug")
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    Some(ParsedModelUrl { id, slug })
}

/// Strips characters unsafe on macOS filesystems and caps length at 120 chars.
pub fn safe_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            _ => c,
        })
        .collect();
    sanitized.chars().take(120).collect()
}

/// Extracts a safe 1..=4 character lowercase image extension from a URL.
pub fn image_extension_from_url(url: &str) -> String {
    let without_query = url.split('?').next().unwrap_or(url);
    let candidate = without_query.rsplit('.').next().unwrap_or("");
    let clean: String = candidate
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(4)
        .collect::<String>()
        .to_lowercase();
    if clean.is_empty() || clean == "com" {
        "jpg".to_string()
    } else {
        clean
    }
}

fn kind_from_extension(name: &str) -> String {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "stl" => "stl",
        "gcode" | "bgcode" | "gco" | "g" => "gcode",
        "sl1" | "sl1s" | "sla" | "pwmx" | "pwms" | "zip" => "sla",
        _ => "other",
    }
    .to_string()
}

/// Pull `model_id` into `dest_root`.
pub fn pull_model<F>(
    client: &PrintablesClient,
    model_id: &str,
    dest_root: &Path,
    opts: &PullOptions,
    mut emit: F,
) -> AppResult<PullOutcome>
where
    F: FnMut(Value),
{
    emit(json!({
        "kind": "phase", "phase": "metadata", "percent": 5,
        "message": "Fetching model metadata…"
    }));

    let remote_meta = client.get_model_metadata(model_id)?;

    let existing_dir = locator::try_find_model_dir(dest_root, model_id);
    let created = existing_dir.is_none();
    let model_dir = match existing_dir {
        Some(dir) => dir,
        None => locator::new_model_dir(dest_root, model_id, &remote_meta.slug),
    };

    let files_dir = model_dir.join("files");
    let images_dir = model_dir.join("images");
    fs::create_dir_all(&files_dir)?;
    fs::create_dir_all(&images_dir)?;

    let previous = locator::read_metadata(&model_dir).ok();
    // A dirty `metadata.json` means the user edited it locally; a pull must not
    // silently overwrite those edits.
    let local_metadata_dirty = previous.is_some()
        && git::is_repo(&model_dir)
        && git::status(&model_dir)
            .map(|s| {
                s.modified
                    .iter()
                    .chain(s.untracked.iter())
                    .any(|p| p == "metadata.json")
            })
            .unwrap_or(false);

    emit(json!({
        "kind": "phase", "phase": "metadata", "percent": 20,
        "message": "Metadata fetched"
    }));

    // ---------------------------------------------------------------- files
    emit(json!({
        "kind": "phase", "phase": "files", "percent": 25,
        "message": "Listing model files…"
    }));

    let remote_files = client.list_model_files(model_id)?;
    let remote_by_name: HashMap<String, &crate::printables::api::RemoteFile> = remote_files
        .iter()
        .map(|f| (safe_filename(&f.name), f))
        .collect();

    let mut warnings: Vec<String> = Vec::new();
    let mut downloaded: Vec<MetadataFile> = Vec::new();
    let mut files_downloaded = 0usize;
    let mut files_skipped = 0usize;

    let total_files = remote_files.len().max(1);

    for (idx, remote_file) in remote_files.iter().enumerate() {
        let fname = safe_filename(&remote_file.name);
        let dest = files_dir.join(&fname);
        let pct = 30 + (((idx + 1) as f64 / total_files as f64) * 40.0) as u32;

        let expected_size = remote_file.size_bytes;
        let already_ok = !opts.force
            && dest.exists()
            && match expected_size {
                Some(size) if size > 0 => dest.metadata().map(|m| m.len() == size).unwrap_or(false),
                _ => dest.metadata().map(|m| m.len() > 0).unwrap_or(false),
            };

        if already_ok {
            files_skipped += 1;
            let size = dest.metadata().map(|m| m.len()).unwrap_or(0);
            downloaded.push(MetadataFile {
                name: fname.clone(),
                kind: remote_file.file_type.clone(),
                size_bytes: size,
                local_path: format!("files/{fname}"),
                remote_id: Some(remote_file.id.clone()),
            });
            emit(json!({
                "kind": "progress", "phase": "files", "percent": pct,
                "message": format!("Skipping {fname} (already downloaded)"),
                "file": fname, "file_index": idx + 1, "file_total": remote_files.len(),
            }));
            continue;
        }

        emit(json!({
            "kind": "progress", "phase": "files", "percent": pct,
            "message": format!("Downloading {fname}…"),
            "file": fname, "file_index": idx + 1, "file_total": remote_files.len(),
        }));

        let link =
            match client.resolve_download_link(&remote_file.id, model_id, &remote_file.file_type) {
                Ok(Some(link)) => link,
                Ok(None) => {
                    warnings.push(format!("No download link for {fname}"));
                    // Keep whatever copy we already have.
                    if dest.exists() {
                        let size = dest.metadata().map(|m| m.len()).unwrap_or(0);
                        downloaded.push(MetadataFile {
                            name: fname.clone(),
                            kind: remote_file.file_type.clone(),
                            size_bytes: size,
                            local_path: format!("files/{fname}"),
                            remote_id: Some(remote_file.id.clone()),
                        });
                    }
                    continue;
                }
                Err(err) => {
                    warnings.push(format!("Could not resolve {fname}: {}", err.message));
                    if dest.exists() {
                        let size = dest.metadata().map(|m| m.len()).unwrap_or(0);
                        downloaded.push(MetadataFile {
                            name: fname.clone(),
                            kind: remote_file.file_type.clone(),
                            size_bytes: size,
                            local_path: format!("files/{fname}"),
                            remote_id: Some(remote_file.id.clone()),
                        });
                    }
                    continue;
                }
            };

        match client.download_file(&link, &dest, |_, _| {}) {
            Ok(size_bytes) => {
                files_downloaded += 1;
                downloaded.push(MetadataFile {
                    name: fname.clone(),
                    kind: remote_file.file_type.clone(),
                    size_bytes,
                    local_path: format!("files/{fname}"),
                    remote_id: Some(remote_file.id.clone()),
                });
            }
            Err(err) => {
                warnings.push(format!("Failed to download {fname}: {}", err.message));
                if dest.exists() {
                    let size = dest.metadata().map(|m| m.len()).unwrap_or(0);
                    downloaded.push(MetadataFile {
                        name: fname.clone(),
                        kind: remote_file.file_type.clone(),
                        size_bytes: size,
                        local_path: format!("files/{fname}"),
                        remote_id: Some(remote_file.id.clone()),
                    });
                }
            }
        }
    }

    // Files that exist locally but not on Printables are local additions.
    let mut files_local_only = 0usize;
    if let Ok(entries) = fs::read_dir(&files_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.ends_with(".part") || remote_by_name.contains_key(&name) {
                continue;
            }
            if downloaded.iter().any(|f| f.name == name) {
                continue;
            }
            files_local_only += 1;
            let size = path.metadata().map(|m| m.len()).unwrap_or(0);
            downloaded.push(MetadataFile {
                name: name.clone(),
                kind: kind_from_extension(&name),
                size_bytes: size,
                local_path: format!("files/{name}"),
                remote_id: None,
            });
        }
    }

    emit(json!({
        "kind": "phase", "phase": "files", "percent": 70,
        "message": format!("{} files downloaded, {} already present", files_downloaded, files_skipped)
    }));

    // --------------------------------------------------------------- images
    emit(json!({
        "kind": "phase", "phase": "images", "percent": 75,
        "message": "Fetching image list…"
    }));

    let gallery = client.get_model_images(model_id).unwrap_or_default();
    let mut all_images: Vec<RemoteImage> = Vec::new();
    if let Some(ref cover_url) = remote_meta.main_image_url {
        all_images.push(RemoteImage {
            id: None,
            url: cover_url.clone(),
            kind: "cover".to_string(),
        });
    }
    for img in gallery {
        if remote_meta.main_image_url.as_deref() != Some(img.url.as_str()) {
            all_images.push(img);
        }
    }

    let total_images = all_images.len().max(1);
    let mut images: Vec<MetadataImage> = Vec::new();
    let mut images_downloaded = 0usize;
    let mut images_skipped = 0usize;

    for (idx, img) in all_images.iter().enumerate() {
        let img_kind = if idx == 0 {
            "cover".to_string()
        } else {
            img.kind.clone()
        };
        let ext = image_extension_from_url(&img.url);
        let img_name = format!("image_{idx:02}_{img_kind}.{ext}");
        let img_dest = images_dir.join(&img_name);
        let pct = 75 + (((idx + 1) as f64 / total_images as f64) * 10.0) as u32;

        if img_dest.exists() && !opts.force {
            images_skipped += 1;
        } else {
            emit(json!({
                "kind": "progress", "phase": "images", "percent": pct,
                "message": format!("Downloading image {} of {}…", idx + 1, all_images.len())
            }));
            match client.download_file(&img.url, &img_dest, |_, _| {}) {
                Ok(_) => images_downloaded += 1,
                Err(err) => {
                    warnings.push(format!(
                        "Failed to download image {img_name}: {}",
                        err.message
                    ));
                    continue;
                }
            }
        }

        images.push(MetadataImage {
            name: img_name.clone(),
            kind: img_kind,
            local_path: format!("images/{img_name}"),
            remote_id: img.id.clone(),
        });
    }

    emit(json!({
        "kind": "phase", "phase": "images", "percent": 85,
        "message": format!("{} images downloaded, {} already present", images_downloaded, images_skipped)
    }));

    // -------------------------------------------------------------- metadata
    emit(json!({
        "kind": "phase", "phase": "finalize", "percent": 90,
        "message": "Writing metadata.json…"
    }));

    let keep_local = local_metadata_dirty && previous.is_some();
    let prev = previous
        .clone()
        .unwrap_or_else(|| locator::empty_metadata(model_id));

    let metadata = MetadataV1 {
        schema_version: 2,
        source: "printables.com".to_string(),
        model_id: model_id.to_string(),
        slug: remote_meta.slug.clone(),
        name: if keep_local {
            prev.name.clone()
        } else {
            remote_meta.name.clone()
        },
        url: format!(
            "https://www.printables.com/model/{model_id}-{}",
            remote_meta.slug
        ),
        author: remote_meta.author.clone(),
        tags: if keep_local {
            prev.tags.clone()
        } else {
            remote_meta.tags.clone()
        },
        stats: ModelStats {
            likes: remote_meta.likes,
            downloads: remote_meta.downloads,
            rating: remote_meta.rating,
            published_date: remote_meta.published_date.clone(),
        },
        description: if keep_local {
            prev.description.clone()
        } else {
            remote_meta.description.clone()
        },
        remote_description_html: remote_meta.raw_description.clone(),
        cloned_at: prev.cloned_at.clone(),
        files: downloaded,
        images,
    };

    locator::write_metadata(&model_dir, &metadata)?;

    let mut committed = None;
    if opts.git {
        let was_repo = git::is_repo(&model_dir);
        git::open(&model_dir, true)?;
        let message = if was_repo {
            "Pulled changes from printables.com"
        } else {
            "Imported from printables.com"
        };
        committed = git::commit_all(&model_dir, message)?;
    }

    let abs_dir = model_dir
        .canonicalize()
        .unwrap_or_else(|_| model_dir.clone());

    emit(json!({
        "kind": "done",
        "model_dir": abs_dir.display().to_string(),
        "percent": 100
    }));

    Ok(PullOutcome {
        model_id: model_id.to_string(),
        model_dir: abs_dir.display().to_string(),
        name: metadata.name,
        created,
        files_downloaded,
        files_skipped,
        files_local_only,
        images_downloaded,
        images_skipped,
        warnings,
        committed,
    })
}

/// Adopt every pre-2.0 model folder into version control.
pub fn adopt_existing_models(library: &Path) -> AppResult<usize> {
    let mut adopted = 0usize;
    for dir in locator::model_dirs(library)? {
        if git::is_repo(&dir) {
            continue;
        }
        if git::ensure_initialised(&dir)?.is_some() {
            adopted += 1;
        }
    }
    Ok(adopted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_parse_model_url_variants() {
        let p1 = parse_model_url(
            "https://www.printables.com/model/1648230-four-color-multicolor-test-benchy-for-cfs-mmu-copr",
        )
        .unwrap();
        assert_eq!(p1.id, "1648230");
        assert_eq!(
            p1.slug,
            "four-color-multicolor-test-benchy-for-cfs-mmu-copr"
        );

        let p2 = parse_model_url("https://printables.com/model/12345").unwrap();
        assert_eq!(p2.id, "12345");
        assert_eq!(p2.slug, "");

        let p3 = parse_model_url("https://www.printables.com/model/999-test-slug/files?foo=bar")
            .unwrap();
        assert_eq!(p3.id, "999");
        assert_eq!(p3.slug, "test-slug");

        assert!(parse_model_url("https://example.com/model/123").is_none());
    }

    #[test]
    fn test_safe_filename_and_image_ext() {
        assert_eq!(
            safe_filename("a<b>c:d\"e/f\\g|h?i*j.3mf"),
            "a_b_c_d_e_f_g_h_i_j.3mf"
        );
        assert_eq!(
            image_extension_from_url(
                "https://media.printables.com/media/prints/123/images/cover.webp?t=123"
            ),
            "webp"
        );
        assert_eq!(
            image_extension_from_url(
                "https://media.printables.com/media/prints/123/images/cover.PNG"
            ),
            "png"
        );
    }

    #[test]
    fn extension_maps_to_printables_kind() {
        assert_eq!(kind_from_extension("a.stl"), "stl");
        assert_eq!(kind_from_extension("b.gcode"), "gcode");
        assert_eq!(kind_from_extension("c.SL1"), "sla");
        assert_eq!(kind_from_extension("readme.txt"), "other");
    }

    #[test]
    fn adoption_puts_existing_folders_under_version_control() {
        let library = std::env::temp_dir().join(format!("po-adopt-{}", uuid::Uuid::new_v4()));
        let model_dir = library.join("777-legacy-model");
        fs::create_dir_all(model_dir.join("files")).unwrap();
        fs::create_dir_all(model_dir.join("images")).unwrap();
        fs::write(model_dir.join("files/part.stl"), b"solid legacy").unwrap();
        fs::write(
            model_dir.join("metadata.json"),
            serde_json::to_string_pretty(&locator::empty_metadata("777")).unwrap(),
        )
        .unwrap();

        let adopted = adopt_existing_models(&library).expect("adoption failed");
        assert_eq!(adopted, 1);
        assert!(
            git::is_repo(&model_dir),
            "folder must become a git repository"
        );

        let commits = git::log(&model_dir, 10).unwrap();
        assert_eq!(commits.len(), 1);
        assert!(commits[0].summary.contains("Initial import"));

        // Running it again is a no-op.
        assert_eq!(adopt_existing_models(&library).unwrap(), 0);

        let _ = fs::remove_dir_all(&library);
    }

    /// Requires network: `PO_LIVE=1 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn test_end_to_end_pull_and_index_live_model() {
        if std::env::var("PO_LIVE").is_err() {
            return;
        }
        let temp_dir = std::env::temp_dir().join(format!(
            "printables_manager_pull_test_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&temp_dir).unwrap();

        let client = PrintablesClient::anonymous().unwrap();
        let mut events = Vec::new();
        let outcome = pull_model(
            &client,
            "1648230",
            &temp_dir,
            &PullOptions::default(),
            |ev| events.push(ev),
        )
        .expect("live pull failed");

        assert!(outcome.created);
        assert!(
            !outcome.files_downloaded + outcome.files_skipped == 0,
            "expected files"
        );
        assert!(outcome.committed.is_some(), "pull must create a commit");

        let model_dir = PathBuf::from(&outcome.model_dir);
        assert!(model_dir.join("metadata.json").exists());
        assert!(git::is_repo(&model_dir));

        // Second pull must be idempotent and create no new commit.
        let again = pull_model(
            &client,
            "1648230",
            &temp_dir,
            &PullOptions::default(),
            |_| {},
        )
        .expect("second pull failed");
        assert!(!again.created);
        assert_eq!(again.files_downloaded, 0);
        assert_eq!(again.committed, None, "no-op pull must not commit");

        let index = crate::library::indexer::index_library(&temp_dir).unwrap();
        assert_eq!(index.models.len(), 1, "no duplicate folders");
        assert!(index.models[0].cover_asset_url.is_some());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
