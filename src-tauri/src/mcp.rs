//! Model Context Protocol server: the whole app, minus native dialogs.
//!
//! JSON-RPC 2.0 over stdio (one object per line). Every tool below maps onto the
//! same code paths the UI uses, so an agent can do anything a person can do
//! except click a native file picker — those tools take explicit paths instead.
//!
//! Set `PRINTABLES_MANAGER_LIBRARY` to point the server at another library
//! (used by the tests so they never touch the real one).

use crate::error::{AppError, AppResult};
use crate::library::locator;
use crate::models::{AppPrefs, Collection, CollectionsFile};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Every Tauri command and how MCP covers it. `-ui-only` means a native dialog
/// or window action that has no headless equivalent; everything else must exist
/// as a tool. This table is the contract enforced by `coverage_is_complete`.
pub const COMMAND_COVERAGE: &[(&str, &str)] = &[
    // library
    ("list_models", "list_models"),
    ("library_stats", "library_stats"),
    ("rescan_library", "rescan_library"),
    ("get_model_edit", "get_model"),
    // models
    ("create_local_model", "create_model"),
    ("update_model_details", "update_model_details"),
    ("delete_model", "delete_model"),
    ("import_model_files", "add_files"),
    ("delete_model_file", "delete_file"),
    ("set_model_cover", "set_cover"),
    ("export_files", "export_model"),
    // collections
    ("list_collections", "list_collections"),
    ("create_collection", "create_collection"),
    ("rename_collection", "rename_collection"),
    ("delete_collection", "delete_collection"),
    ("add_model_to_collection", "add_to_collection"),
    ("remove_model_from_collection", "remove_from_collection"),
    ("import_site_collections", "import_site_collections"),
    // git
    ("git_status", "git_status"),
    ("git_log", "git_log"),
    ("git_show", "git_show"),
    ("git_commit", "git_commit"),
    ("git_restore", "git_restore"),
    ("git_compact", "git_compact"),
    ("git_init", "git_init"),
    ("git_adopt_library", "git_adopt_library"),
    // prefs + system
    ("get_prefs", "get_prefs"),
    ("set_prefs", "set_prefs"),
    ("backup_to_icloud", "backup_to_icloud"),
    ("slicer_info", "slicer_info"),
    ("push_log", "push_log"),
    // account + sync
    ("auth_status", "auth_status"),
    ("auth_logout", "auth_logout"),
    ("account_models", "account_models"),
    ("sync_all", "sync_all"),
    ("sync_model", "sync_model"),
    ("clone_model", "clone_model"),
    ("push_preview", "push_preview"),
    ("push_apply", "push_apply"),
    ("publish_model", "publish_model"),
    ("delete_remote_model", "delete_remote_model"),
    // native UI only
    ("dialog_open", "-ui-only"),
    ("ui_ready", "-ui-only"),
    ("open_config_folder", "-ui-only"),
    ("open_folder", "-ui-only"),
    ("open_external", "-ui-only"),
    ("open_in_slicer", "-ui-only"),
    ("slice_file", "-ui-only"),
    ("slicer_test", "-ui-only"),
    ("rebuild_thumbs", "-ui-only"),
    ("auth_login", "-ui-only"),
    ("auth_verify", "-ui-only"),
    ("auth_refresh", "-ui-only"),
    ("auth_set_token", "-ui-only"),
    ("auth_capabilities", "-ui-only"),
    ("auth_keychain_test", "-ui-only"),
    ("auth_storage_retry", "-ui-only"),
    ("sync_cancel", "-ui-only"),
    ("jobs_list", "-ui-only"),
    ("jobs_clear", "-ui-only"),
];

fn tool_spec(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false
        }
    })
}

fn tools() -> Value {
    let id = json!({ "type": "string" });
    let s = json!({ "type": "string" });
    let tags = json!({ "type": "array", "items": { "type": "string" } });
    json!([
        tool_spec(
            "list_models",
            "List every model with size, file count and git state.",
            json!({}),
            &[]
        ),
        tool_spec(
            "library_stats",
            "Aggregate totals for the library.",
            json!({}),
            &[]
        ),
        tool_spec(
            "rescan_library",
            "Re-index the library folder from disk.",
            json!({}),
            &[]
        ),
        tool_spec(
            "get_model",
            "Read one model's full metadata.",
            json!({ "model_id": id }),
            &["model_id"]
        ),
        tool_spec(
            "create_model",
            "Create a new local model folder (git initialised) and return it.",
            json!({ "name": s, "description": s, "tags": tags }),
            &["name"]
        ),
        tool_spec(
            "update_model_details",
            "Rename / re-describe / re-tag a model. Commits.",
            json!({ "model_id": id, "name": s, "description": s, "tags": tags }),
            &["model_id"]
        ),
        tool_spec(
            "delete_model",
            "Delete a model folder and drop it from collections.",
            json!({ "model_id": id }),
            &["model_id"]
        ),
        tool_spec(
            "add_files",
            "Copy explicit file paths into a model and register them. Commits.",
            json!({ "model_id": id, "paths": json!({ "type": "array", "items": { "type": "string" } }) }),
            &["model_id", "paths"]
        ),
        tool_spec(
            "delete_file",
            "Remove a file from a model and from its manifest. Commits.",
            json!({ "model_id": id, "name": s }),
            &["model_id", "name"]
        ),
        tool_spec(
            "set_cover",
            "Choose which local image is the cover. Commits.",
            json!({ "model_id": id, "name": s }),
            &["model_id", "name"]
        ),
        tool_spec(
            "export_model",
            "Copy a model's files to a folder you name.",
            json!({ "model_id": id, "destination": s }),
            &["model_id", "destination"]
        ),
        tool_spec(
            "list_collections",
            "List collections and their model ids.",
            json!({}),
            &[]
        ),
        tool_spec(
            "create_collection",
            "Create a local collection.",
            json!({ "name": s }),
            &["name"]
        ),
        tool_spec(
            "rename_collection",
            "Rename a collection.",
            json!({ "collection_id": id, "name": s }),
            &["collection_id", "name"]
        ),
        tool_spec(
            "delete_collection",
            "Delete a collection (models stay).",
            json!({ "collection_id": id }),
            &["collection_id"]
        ),
        tool_spec(
            "add_to_collection",
            "Add a model to a collection.",
            json!({ "collection_id": id, "model_id": id }),
            &["collection_id", "model_id"]
        ),
        tool_spec(
            "remove_from_collection",
            "Remove a model from a collection.",
            json!({ "collection_id": id, "model_id": id }),
            &["collection_id", "model_id"]
        ),
        tool_spec(
            "import_site_collections",
            "Import collections (and their models) from printables.com. Needs a session.",
            json!({}),
            &[]
        ),
        tool_spec(
            "git_status",
            "Working-tree status for a model.",
            json!({ "model_id": id }),
            &["model_id"]
        ),
        tool_spec(
            "git_log",
            "Commit history for a model.",
            json!({ "model_id": id, "limit": json!({ "type": "integer" }) }),
            &["model_id"]
        ),
        tool_spec(
            "git_show",
            "One commit with per-file line counts.",
            json!({ "model_id": id, "commit_id": s }),
            &["model_id", "commit_id"]
        ),
        tool_spec(
            "git_commit",
            "Commit the working tree now.",
            json!({ "model_id": id, "message": s }),
            &["model_id"]
        ),
        tool_spec(
            "git_restore",
            "Restore a revision and record it as a new commit.",
            json!({ "model_id": id, "commit_id": s }),
            &["model_id", "commit_id"]
        ),
        tool_spec(
            "git_compact",
            "Collapse history to one commit (tagged first).",
            json!({ "model_id": id }),
            &["model_id"]
        ),
        tool_spec(
            "git_init",
            "Start tracking a model that has no repository.",
            json!({ "model_id": id }),
            &["model_id"]
        ),
        tool_spec(
            "git_adopt_library",
            "Put every untracked model folder under version control.",
            json!({}),
            &[]
        ),
        tool_spec("get_prefs", "Read preferences.", json!({}), &[]),
        tool_spec(
            "set_prefs",
            "Update preferences (theme, slicer, git, backups, ...).",
            json!({ "theme": s, "slicer_key": s, "slicer_executable": s, "library_folder": s, "git_enabled": json!({"type":"boolean"}), "auto_commit_local_edits": json!({"type":"boolean"}), "sync_concurrency": json!({"type":"integer"}), "remote_write_enabled": json!({"type":"boolean"}), "last_view": s }),
            &[]
        ),
        tool_spec(
            "backup_to_icloud",
            "Mirror every model's git history into iCloud Drive.",
            json!({}),
            &[]
        ),
        tool_spec(
            "slicer_info",
            "Which slicer is configured and whether it exists.",
            json!({}),
            &[]
        ),
        tool_spec("push_log", "Recent push attempts.", json!({}), &[]),
        tool_spec(
            "auth_status",
            "Whether a Printables session is available.",
            json!({}),
            &[]
        ),
        tool_spec("auth_logout", "Forget the stored session.", json!({}), &[]),
        tool_spec(
            "account_models",
            "List the models authored by the signed-in account.",
            json!({}),
            &[]
        ),
        tool_spec(
            "sync_all",
            "Pull every authored model into the library.",
            json!({}),
            &[]
        ),
        tool_spec(
            "sync_model",
            "Pull one model by id.",
            json!({ "model_id": id, "force": json!({"type":"boolean"}) }),
            &["model_id"]
        ),
        tool_spec(
            "clone_model",
            "Clone a model from a printables.com URL.",
            json!({ "url": s }),
            &["url"]
        ),
        tool_spec(
            "push_preview",
            "Compare a model folder with printables.com (read-only).",
            json!({ "model_id": id }),
            &["model_id"]
        ),
        tool_spec(
            "push_apply",
            "Apply a reviewed push; dry_run defaults to true.",
            json!({ "model_id": id, "dry_run": json!({"type":"boolean"}) }),
            &["model_id"]
        ),
        tool_spec(
            "publish_model",
            "Upload images and files, then create or update the model on printables.com (draft by default). Requires writes enabled.",
            json!({ "model_id": id, "draft": json!({"type":"boolean"}) }),
            &["model_id"]
        ),
        tool_spec(
            "delete_remote_model",
            "Delete the model on printables.com. Irreversible. Requires writes enabled.",
            json!({ "model_id": id }),
            &["model_id"]
        ),
    ])
}

// ---------------------------------------------------------------- plumbing

pub fn library() -> AppResult<PathBuf> {
    if let Some(dir) = std::env::var_os("PRINTABLES_MANAGER_LIBRARY") {
        return Ok(PathBuf::from(dir));
    }
    let prefs = crate::config::load_prefs()?;
    crate::config::library_dir(&prefs)
}

fn prefs() -> AppResult<AppPrefs> {
    crate::config::load_prefs()
}

fn model_dir(model_id: &str) -> AppResult<PathBuf> {
    locator::find_model_dir(&library()?, model_id)
}

fn collections_path(lib: &Path) -> PathBuf {
    lib.join("collections.json")
}

fn load_collections(lib: &Path) -> CollectionsFile {
    std::fs::read_to_string(collections_path(lib))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_collections(lib: &Path, data: &CollectionsFile) -> AppResult<()> {
    std::fs::write(collections_path(lib), serde_json::to_string_pretty(data)?)?;
    Ok(())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn client() -> AppResult<crate::printables::api::PrintablesClient> {
    let session = crate::auth::load_session()?.ok_or_else(|| {
        AppError::not_authenticated("No Printables session. Sign in from the app first.")
    })?;
    crate::printables::api::PrintablesClient::with_token(Some(session.access_token))
}

// ------------------------------------------------------------------ dispatch

fn call(name: &str, args: &Value) -> Result<Value, String> {
    let s = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let sopt = |k: &str| args.get(k).and_then(|v| v.as_str()).map(String::from);
    let list = |k: &str| -> Vec<String> {
        args.get(k)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let err = |e: AppError| e.message;

    let out: AppResult<Value> = (|| match name {
        "list_models" => {
            let lib = library()?;
            let index = crate::library::indexer::index_library(&lib)?;
            Ok(json!({
                "library": lib.display().to_string(),
                "totals": index.totals,
                "models": index.models.iter().map(|m| json!({
                    "id": m.id, "name": m.name, "author": m.creator, "tags": m.tags,
                    "files": m.files.len(), "size_mb": m.size_mb,
                    "sync_state": m.sync_state, "dir": m.dir,
                    "has_git": m.has_git, "dirty": m.dirty,
                })).collect::<Vec<_>>()
            }))
        }
        "library_stats" => {
            let index = crate::library::indexer::index_library(&library()?)?;
            Ok(serde_json::to_value(index.totals)?)
        }
        "rescan_library" => {
            let index = crate::library::indexer::index_library(&library()?)?;
            Ok(json!({ "models": index.models.len(), "files": index.totals.files }))
        }
        "get_model" => {
            let dir = model_dir(&s("model_id"))?;
            Ok(serde_json::to_value(locator::read_metadata(&dir)?)?)
        }
        "create_model" => {
            let created = crate::commands::edit::create_model_in(
                &library()?,
                &s("name"),
                &s("description"),
                list("tags"),
                prefs()?.git_enabled,
            )?;
            Ok(json!({ "model_id": created.model_id, "model": created }))
        }
        "update_model_details" => {
            let dir = model_dir(&s("model_id"))?;
            let current = locator::read_metadata(&dir)?;
            let name = sopt("name").unwrap_or(current.name);
            let description = args
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or(current.description);
            let tags = if args.get("tags").is_some() {
                list("tags")
            } else {
                current.tags
            };
            Ok(serde_json::to_value(crate::commands::edit::apply_details(
                &dir,
                &name,
                &description,
                tags,
            )?)?)
        }
        "delete_model" => {
            let id = s("model_id");
            let dir = model_dir(&id)?;
            std::fs::remove_dir_all(&dir)?;
            let lib = library()?;
            let mut data = load_collections(&lib);
            let mut changed = false;
            for c in data.collections.iter_mut() {
                let before = c.model_ids.len();
                c.model_ids.retain(|m| m != &id);
                changed |= c.model_ids.len() != before;
            }
            if changed {
                save_collections(&lib, &data)?;
            }
            Ok(json!({ "deleted": id, "dir": dir.display().to_string() }))
        }
        "add_files" => {
            let dir = model_dir(&s("model_id"))?;
            let paths: Vec<PathBuf> = list("paths").into_iter().map(PathBuf::from).collect();
            let (imported, skipped, metadata) = crate::commands::edit::import_paths(&dir, &paths)?;
            Ok(json!({ "imported": imported, "skipped": skipped, "files": metadata.files.len() }))
        }
        "delete_file" => {
            let dir = model_dir(&s("model_id"))?;
            let meta = crate::commands::edit::apply_delete_file(&dir, &s("name"))?;
            Ok(json!({ "files": meta.files.len() }))
        }
        "set_cover" => {
            let dir = model_dir(&s("model_id"))?;
            Ok(serde_json::to_value(crate::commands::edit::apply_cover(
                &dir,
                &s("name"),
            )?)?)
        }
        "export_model" => {
            let dir = model_dir(&s("model_id"))?;
            let dest = PathBuf::from(s("destination"));
            std::fs::create_dir_all(&dest)?;
            let mut copied = 0;
            for sub in ["files", "images"] {
                let from = dir.join(sub);
                if !from.is_dir() {
                    continue;
                }
                std::fs::create_dir_all(dest.join(sub))?;
                for entry in std::fs::read_dir(&from)?.flatten() {
                    if entry.path().is_file() {
                        std::fs::copy(entry.path(), dest.join(sub).join(entry.file_name()))?;
                        copied += 1;
                    }
                }
            }
            std::fs::copy(dir.join("metadata.json"), dest.join("metadata.json"))?;
            Ok(json!({ "destination": dest.display().to_string(), "copied": copied }))
        }
        "list_collections" => {
            let lib = library()?;
            Ok(serde_json::to_value(load_collections(&lib))?)
        }
        "create_collection" => {
            let lib = library()?;
            let mut data = load_collections(&lib);
            let name = s("name");
            if name.trim().is_empty() {
                return Err(AppError::invalid("Collection name cannot be empty."));
            }
            let now = now();
            let c = Collection {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.trim().to_string(),
                model_ids: Vec::new(),
                created_at: now.clone(),
                updated_at: now,
                site_id: None,
                site_count: None,
            };
            data.collections.push(c.clone());
            save_collections(&lib, &data)?;
            Ok(serde_json::to_value(c)?)
        }
        "rename_collection" => {
            let lib = library()?;
            let mut data = load_collections(&lib);
            let c = data
                .collections
                .iter_mut()
                .find(|c| c.id == s("collection_id"))
                .ok_or_else(|| AppError::not_found("Collection not found."))?;
            c.name = s("name").trim().to_string();
            c.updated_at = now();
            let out = c.clone();
            save_collections(&lib, &data)?;
            Ok(serde_json::to_value(out)?)
        }
        "delete_collection" => {
            let lib = library()?;
            let mut data = load_collections(&lib);
            let before = data.collections.len();
            data.collections.retain(|c| c.id != s("collection_id"));
            if data.collections.len() == before {
                return Err(AppError::not_found("Collection not found."));
            }
            save_collections(&lib, &data)?;
            Ok(json!({ "deleted": true }))
        }
        "add_to_collection" | "remove_from_collection" => {
            let lib = library()?;
            let mut data = load_collections(&lib);
            let c = data
                .collections
                .iter_mut()
                .find(|c| c.id == s("collection_id"))
                .ok_or_else(|| AppError::not_found("Collection not found."))?;
            if name == "add_to_collection" {
                if !c.model_ids.iter().any(|m| m == &s("model_id")) {
                    c.model_ids.push(s("model_id"));
                }
            } else {
                c.model_ids.retain(|m| m != &s("model_id"));
            }
            c.updated_at = now();
            let out = c.clone();
            save_collections(&lib, &data)?;
            Ok(serde_json::to_value(out)?)
        }
        "import_site_collections" => {
            let lib = library()?;
            let api = client()?;
            let account = api.viewer()?.id.ok_or_else(|| {
                AppError::not_authenticated("Printables did not report an account id.")
            })?;
            let site = api.user_collections(&account)?;
            let mut data = load_collections(&lib);
            crate::commands::collections::upsert_site_collections(&mut data, &site);
            for remote in &site {
                if let Ok(ids) = api.collection_model_ids(&remote.id) {
                    crate::commands::collections::apply_membership(&mut data, &remote.id, ids);
                }
            }
            save_collections(&lib, &data)?;
            Ok(serde_json::to_value(data.collections)?)
        }
        "git_status" => {
            let dir = model_dir(&s("model_id"))?;
            Ok(serde_json::to_value(crate::git::status(&dir)?)?)
        }
        "git_log" => {
            let dir = model_dir(&s("model_id"))?;
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            Ok(serde_json::to_value(crate::git::log(&dir, limit)?)?)
        }
        "git_show" => {
            let dir = model_dir(&s("model_id"))?;
            Ok(serde_json::to_value(crate::git::show(
                &dir,
                &s("commit_id"),
            )?)?)
        }
        "git_commit" => {
            let dir = model_dir(&s("model_id"))?;
            let message = sopt("message").unwrap_or_else(|| "Manual commit".into());
            Ok(json!({ "commit": crate::git::commit_all(&dir, &message)? }))
        }
        "git_restore" => {
            let dir = model_dir(&s("model_id"))?;
            Ok(json!({ "commit": crate::git::restore(&dir, &s("commit_id"))? }))
        }
        "git_compact" => {
            let dir = model_dir(&s("model_id"))?;
            Ok(json!({ "removed": crate::git::compact(&dir)? }))
        }
        "git_init" => {
            let dir = model_dir(&s("model_id"))?;
            Ok(json!({ "commit": crate::git::ensure_initialised(&dir)? }))
        }
        "git_adopt_library" => {
            Ok(json!({ "adopted": crate::printables::clone::adopt_existing_models(&library()?)? }))
        }
        "get_prefs" => Ok(serde_json::to_value(prefs()?)?),
        "set_prefs" => {
            let mut p = prefs()?;
            if let Some(v) = sopt("theme") {
                p.theme = v;
            }
            if let Some(v) = sopt("slicer_key") {
                p.slicer_key = v;
            }
            if let Some(v) = sopt("slicer_executable") {
                p.slicer_executable = Some(v);
            }
            if let Some(v) = sopt("library_folder") {
                p.library_folder = Some(v);
            }
            if let Some(v) = sopt("last_view") {
                p.last_view = Some(v);
            }
            if let Some(v) = args.get("git_enabled").and_then(|v| v.as_bool()) {
                p.git_enabled = v;
            }
            if let Some(v) = args
                .get("auto_commit_local_edits")
                .and_then(|v| v.as_bool())
            {
                p.auto_commit_local_edits = v;
            }
            if let Some(v) = args.get("remote_write_enabled").and_then(|v| v.as_bool()) {
                p.remote_write_enabled = v;
            }
            if let Some(v) = args.get("sync_concurrency").and_then(|v| v.as_u64()) {
                p.sync_concurrency = (v as usize).clamp(1, 8);
            }
            crate::config::save_prefs(&p)?;
            Ok(serde_json::to_value(p)?)
        }
        "backup_to_icloud" => {
            let lib = library()?;
            Ok(serde_json::to_value(crate::backup::backup_to(
                &lib,
                &crate::backup::icloud_root()?,
            )?)?)
        }
        "slicer_info" => Ok(serde_json::to_value(crate::commands::slicer::inspect(
            &prefs()?,
        ))?),
        "push_log" => {
            let lib = library()?;
            let raw =
                std::fs::read_to_string(crate::config::push_log_path(&lib)).unwrap_or_default();
            let lines: Vec<Value> = raw
                .lines()
                .rev()
                .take(50)
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .collect();
            Ok(json!(lines))
        }
        "auth_status" => {
            let session = crate::auth::load_session()?;
            Ok(json!({
                "signed_in": session.as_ref().map(|s| !s.is_expired()).unwrap_or(false),
                "username": session.as_ref().and_then(|s| s.username.clone()),
                "expired": session.as_ref().map(|s| s.is_expired()).unwrap_or(false),
                "can_refresh": session.as_ref().map(|s| s.refresh_token.is_some()).unwrap_or(false),
                "storage": session.as_ref().map(|s| s.backend.clone()),
            }))
        }
        "auth_logout" => {
            crate::auth::delete_stored_session()?;
            Ok(json!({ "signed_in": false }))
        }
        "account_models" => {
            let api = client()?;
            let me = api.viewer()?;
            let id = me
                .id
                .clone()
                .ok_or_else(|| AppError::not_authenticated("No account id on this session."))?;
            let page = api.user_models(&id, 100, None, None)?;
            Ok(json!({
                "username": me.public_username,
                "count": page.models.len(),
                "truncated": page.cursor.is_some(),
                "models": page.models,
            }))
        }
        "sync_all" => {
            let api = client()?;
            let me = api.viewer()?;
            let id = me
                .id
                .ok_or_else(|| AppError::not_authenticated("No account id on this session."))?;
            let page = api.user_models(&id, 100, None, None)?;
            let lib = library()?;
            let git = prefs()?.git_enabled;
            let mut pulled = Vec::new();
            let mut failed = Vec::new();
            for item in &page.models {
                let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                match crate::printables::clone::pull_model(
                    &api,
                    id,
                    &lib,
                    &crate::printables::clone::PullOptions { force: false, git },
                    |_| {},
                ) {
                    Ok(o) => pulled.push(o.model_id),
                    Err(e) => failed.push(format!("{id}: {}", e.message)),
                }
            }
            Ok(json!({ "pulled": pulled, "failed": failed }))
        }
        "sync_model" => {
            let api = client()?;
            let lib = library()?;
            let outcome = crate::printables::clone::pull_model(
                &api,
                &s("model_id"),
                &lib,
                &crate::printables::clone::PullOptions {
                    force: args.get("force").and_then(|v| v.as_bool()).unwrap_or(false),
                    git: prefs()?.git_enabled,
                },
                |_| {},
            )?;
            Ok(serde_json::to_value(outcome)?)
        }
        "clone_model" => {
            let parsed = crate::printables::clone::parse_model_url(&s("url"))
                .ok_or_else(|| AppError::invalid("Not a printables.com model URL."))?;
            let api = crate::printables::api::PrintablesClient::with_token(
                crate::auth::load_session()?.map(|s| s.access_token),
            )?;
            let outcome = crate::printables::clone::pull_model(
                &api,
                &parsed.id,
                &library()?,
                &crate::printables::clone::PullOptions {
                    force: false,
                    git: prefs()?.git_enabled,
                },
                |_| {},
            )?;
            Ok(serde_json::to_value(outcome)?)
        }
        "push_preview" => {
            let api = client()?;
            let plan = crate::sync::plan::build(
                &api,
                &library()?,
                &s("model_id"),
                prefs()?.remote_write_enabled,
            )?;
            Ok(serde_json::to_value(plan)?)
        }
        "push_apply" => {
            let api = client()?;
            let lib = library()?;
            let plan = crate::sync::plan::build(
                &api,
                &lib,
                &s("model_id"),
                prefs()?.remote_write_enabled,
            )?;
            let report = crate::sync::push::apply(
                &api,
                &lib,
                &plan,
                args.get("dry_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
            )?;
            Ok(serde_json::to_value(report)?)
        }
        "publish_model" => {
            let prefs = prefs()?;
            if !prefs.remote_write_enabled {
                return Err(AppError::unsupported(
                    "Publishing is disabled. Enable remote writes in preferences first.",
                ));
            }
            let api = client()?;
            let dir = model_dir(&s("model_id"))?;
            let report = crate::commands::publish::publish(
                &api,
                &dir,
                args.get("draft").and_then(|v| v.as_bool()).unwrap_or(true),
            )?;
            Ok(serde_json::to_value(report)?)
        }
        "delete_remote_model" => {
            let prefs = prefs()?;
            if !prefs.remote_write_enabled {
                return Err(AppError::unsupported(
                    "Remote writes are disabled. Enable them in preferences first.",
                ));
            }
            let api = client()?;
            crate::commands::publish::delete_remote(&api, &s("model_id"))?;
            Ok(json!({ "deleted": s("model_id") }))
        }
        other => Err(AppError::invalid(format!("Unknown tool: {other}"))),
    })();

    out.map_err(err)
}

fn handle(request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });

    match method {
        "initialize" => Some(reply(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "printables-manager", "version": env!("CARGO_PKG_VERSION") }
        }))),
        "notifications/initialized" => None,
        "ping" => Some(reply(json!({}))),
        "tools/list" => Some(reply(json!({ "tools": tools() }))),
        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or(json!({}));
            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Some(match call(name, &args) {
                Ok(value) => reply(json!({
                    "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
                    "isError": false
                })),
                Err(message) => reply(json!({
                    "content": [{ "type": "text", "text": message }],
                    "isError": true
                })),
            })
        }
        other => match &id {
            None | Some(Value::Null) => None,
            _ => Some(
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Method not found: {other}") } }),
            ),
        },
    }
}

pub fn serve() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(response) = handle(&request) {
            let _ = serde_json::to_writer(&mut stdout, &response);
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_names() -> Vec<String> {
        tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn coverage_is_complete() {
        let names = tool_names();
        let mut missing = Vec::new();
        for (command, tool) in COMMAND_COVERAGE {
            if *tool == "-ui-only" {
                continue;
            }
            if !names.iter().any(|n| n == tool) {
                missing.push(format!("{command} -> {tool}"));
            }
        }
        assert!(missing.is_empty(), "commands with no MCP tool: {missing:?}");
    }

    #[test]
    fn every_declared_tool_is_dispatchable() {
        // A listed tool that is not handled would advertise a lie. Point the
        // server at an empty temp library and skip anything that talks to the
        // network or iCloud, so this stays hermetic and fast.
        let scratch = std::env::temp_dir().join(format!("po-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&scratch).unwrap();
        std::env::set_var("PRINTABLES_MANAGER_LIBRARY", &scratch);

        let needs_network_or_cloud = [
            "backup_to_icloud",
            "account_models",
            "sync_all",
            "sync_model",
            "clone_model",
            "import_site_collections",
            "push_preview",
            "push_apply",
            "auth_status",
            "auth_logout",
        ];

        let mut unknown = Vec::new();
        for name in tool_names() {
            if needs_network_or_cloud.contains(&name.as_str()) {
                continue;
            }
            if let Err(message) = call(&name, &json!({})) {
                if message.starts_with("Unknown tool") {
                    unknown.push(name);
                }
            }
        }
        std::env::remove_var("PRINTABLES_MANAGER_LIBRARY");
        let _ = std::fs::remove_dir_all(&scratch);
        assert!(unknown.is_empty(), "listed but unhandled: {unknown:?}");
    }

    #[test]
    fn initialize_and_notifications_behave() {
        let ok = handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap();
        assert_eq!(ok["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
        let err = handle(&json!({"jsonrpc":"2.0","id":2,"method":"nope"})).unwrap();
        assert_eq!(err["error"]["code"], -32601);
    }

    #[test]
    fn coverage_table_covers_every_known_command() {
        // Guards against new commands being added without an MCP decision.
        let commands = [
            "clone_model",
            "delete_model",
            "list_models",
            "library_stats",
            "rescan_library",
            "list_collections",
            "create_collection",
            "rename_collection",
            "delete_collection",
            "add_model_to_collection",
            "remove_model_from_collection",
            "import_site_collections",
            "get_prefs",
            "set_prefs",
            "open_config_folder",
            "ui_ready",
            "backup_to_icloud",
            "dialog_open",
            "open_in_slicer",
            "slice_file",
            "slicer_info",
            "slicer_test",
            "rebuild_thumbs",
            "export_files",
            "open_folder",
            "open_external",
            "get_model_edit",
            "update_model_details",
            "import_model_files",
            "delete_model_file",
            "set_model_cover",
            "create_local_model",
            "auth_status",
            "auth_login",
            "auth_logout",
            "auth_verify",
            "auth_refresh",
            "auth_set_token",
            "auth_capabilities",
            "auth_keychain_test",
            "auth_storage_retry",
            "account_models",
            "sync_all",
            "sync_model",
            "sync_cancel",
            "jobs_list",
            "jobs_clear",
            "git_status",
            "git_log",
            "git_show",
            "git_commit",
            "git_restore",
            "git_compact",
            "git_init",
            "git_adopt_library",
            "push_preview",
            "push_apply",
            "push_log",
        ];
        let mapped: Vec<&str> = COMMAND_COVERAGE.iter().map(|(c, _)| *c).collect();
        let unmapped: Vec<&str> = commands
            .iter()
            .copied()
            .filter(|c| !mapped.contains(c))
            .collect();
        assert!(
            unmapped.is_empty(),
            "no coverage decision for: {unmapped:?}"
        );
    }
}
