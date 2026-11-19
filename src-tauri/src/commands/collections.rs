//! Collections: user-defined groupings of models, stored as
//! `<library_folder>/collections.json`, entirely separate from each model's
//! own metadata.json so existing model metadata never has to change shape.

use crate::models::{AppPrefs, Collection, CollectionsFile};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::State;
use uuid::Uuid;

fn collections_path(lib_dir: &Path) -> PathBuf {
    lib_dir.join("collections.json")
}

fn library_dir(prefs: &AppPrefs) -> Result<PathBuf, String> {
    prefs
        .library_folder
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| "Library folder not configured. Set it in Preferences first.".to_string())
}

/// Missing or corrupt collections.json is treated as "no collections yet"
/// rather than an error, so a fresh or older library folder still opens fine.
fn load_collections(lib_dir: &Path) -> CollectionsFile {
    let path = collections_path(lib_dir);
    let Ok(raw) = fs::read_to_string(&path) else {
        return CollectionsFile::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_collections(lib_dir: &Path, data: &CollectionsFile) -> Result<(), String> {
    let path = collections_path(lib_dir);
    let raw = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
    fs::write(&path, raw).map_err(|e| e.to_string())
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Return all collections defined in the current library folder.
#[tauri::command]
pub fn list_collections(prefs: State<'_, Mutex<AppPrefs>>) -> Result<Vec<Collection>, String> {
    let prefs = prefs.lock().map_err(|e| e.to_string())?;
    let lib_dir = library_dir(&prefs)?;
    Ok(load_collections(&lib_dir).collections)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateCollectionArgs {
    pub name: String,
}

#[tauri::command]
pub fn create_collection(
    args: CreateCollectionArgs,
    prefs: State<'_, Mutex<AppPrefs>>,
) -> Result<Collection, String> {
    let prefs = prefs.lock().map_err(|e| e.to_string())?;
    let lib_dir = library_dir(&prefs)?;
    let mut data = load_collections(&lib_dir);

    let name = args.name.trim();
    if name.is_empty() {
        return Err("Collection name cannot be empty.".into());
    }

    let now = now_iso();
    let collection = Collection {
        id: Uuid::new_v4().to_string(),
        name: name.to_string(),
        model_ids: Vec::new(),
        created_at: now.clone(),
        updated_at: now,
        site_id: None,
        site_count: None,
    };
    data.collections.push(collection.clone());
    save_collections(&lib_dir, &data)?;
    Ok(collection)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameCollectionArgs {
    pub collection_id: String,
    pub name: String,
}

#[tauri::command]
pub fn rename_collection(
    args: RenameCollectionArgs,
    prefs: State<'_, Mutex<AppPrefs>>,
) -> Result<Collection, String> {
    let prefs = prefs.lock().map_err(|e| e.to_string())?;
    let lib_dir = library_dir(&prefs)?;
    let mut data = load_collections(&lib_dir);

    let name = args.name.trim();
    if name.is_empty() {
        return Err("Collection name cannot be empty.".into());
    }

    let collection = data
        .collections
        .iter_mut()
        .find(|c| c.id == args.collection_id)
        .ok_or("Collection not found.")?;
    collection.name = name.to_string();
    collection.updated_at = now_iso();
    let result = collection.clone();

    save_collections(&lib_dir, &data)?;
    Ok(result)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteCollectionArgs {
    pub collection_id: String,
}

#[tauri::command]
pub fn delete_collection(
    args: DeleteCollectionArgs,
    prefs: State<'_, Mutex<AppPrefs>>,
) -> Result<(), String> {
    let prefs = prefs.lock().map_err(|e| e.to_string())?;
    let lib_dir = library_dir(&prefs)?;
    let mut data = load_collections(&lib_dir);

    let before = data.collections.len();
    data.collections.retain(|c| c.id != args.collection_id);
    if data.collections.len() == before {
        return Err("Collection not found.".into());
    }

    save_collections(&lib_dir, &data)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionMembershipArgs {
    pub collection_id: String,
    pub model_id: String,
}

/// Add a model to a collection. Idempotent if it's already a member.
#[tauri::command]
pub fn add_model_to_collection(
    args: CollectionMembershipArgs,
    prefs: State<'_, Mutex<AppPrefs>>,
) -> Result<Collection, String> {
    let prefs = prefs.lock().map_err(|e| e.to_string())?;
    let lib_dir = library_dir(&prefs)?;
    let mut data = load_collections(&lib_dir);

    let collection = data
        .collections
        .iter_mut()
        .find(|c| c.id == args.collection_id)
        .ok_or("Collection not found.")?;
    if !collection.model_ids.iter().any(|id| id == &args.model_id) {
        collection.model_ids.push(args.model_id.clone());
        collection.updated_at = now_iso();
    }
    let result = collection.clone();

    save_collections(&lib_dir, &data)?;
    Ok(result)
}

/// Remove a model from a collection without deleting the model itself.
#[tauri::command]
pub fn remove_model_from_collection(
    args: CollectionMembershipArgs,
    prefs: State<'_, Mutex<AppPrefs>>,
) -> Result<Collection, String> {
    let prefs = prefs.lock().map_err(|e| e.to_string())?;
    let lib_dir = library_dir(&prefs)?;
    let mut data = load_collections(&lib_dir);

    let collection = data
        .collections
        .iter_mut()
        .find(|c| c.id == args.collection_id)
        .ok_or("Collection not found.")?;
    collection.model_ids.retain(|id| id != &args.model_id);
    collection.updated_at = now_iso();
    let result = collection.clone();

    save_collections(&lib_dir, &data)?;
    Ok(result)
}

/// Merge the account's printables.com collections into collections.json.
///
/// Ids are namespaced `site-<id>` so a site collection and a local one can
/// never collide, and re-running preserves any local membership and ordering.
pub fn upsert_site_collections(
    data: &mut CollectionsFile,
    site: &[crate::printables::api::SiteCollection],
) -> usize {
    let now = now_iso();
    let mut changed = 0;

    for remote in site {
        let key = format!("site-{}", remote.id);
        match data.collections.iter_mut().find(|c| c.id == key) {
            Some(existing) => {
                let count_changed = existing.site_count != Some(remote.prints_count);
                if existing.name != remote.name || count_changed {
                    existing.name = remote.name.clone();
                    existing.site_count = Some(remote.prints_count);
                    existing.updated_at = now.clone();
                    changed += 1;
                }
            }
            None => {
                data.collections.push(Collection {
                    id: key,
                    name: remote.name.clone(),
                    model_ids: Vec::new(),
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    site_id: Some(remote.id.clone()),
                    site_count: Some(remote.prints_count),
                });
                changed += 1;
            }
        }
    }

    changed
}

/// Replace a site collection's membership with the ids from the API.
pub fn apply_membership(data: &mut CollectionsFile, site_id: &str, ids: Vec<String>) -> bool {
    let key = format!("site-{site_id}");
    let Some(collection) = data.collections.iter_mut().find(|c| c.id == key) else {
        return false;
    };
    collection.model_ids = ids;
    collection.updated_at = now_iso();
    true
}

/// Pull the collections that exist on printables.com into the sidebar.
#[tauri::command]
pub fn import_site_collections(
    prefs: State<'_, Mutex<AppPrefs>>,
    auth: State<'_, crate::auth::AuthState>,
) -> Result<Vec<Collection>, String> {
    let snapshot = prefs.lock().map(|p| p.clone()).map_err(|e| e.to_string())?;
    let lib_dir = library_dir(&snapshot)?;

    let token = auth
        .token()
        .ok_or_else(|| "Sign in to Printables to import your collections.".to_string())?;
    let client =
        crate::printables::api::PrintablesClient::with_token(Some(token)).map_err(|e| e.message)?;
    let account = client
        .viewer()
        .map_err(|e| e.message)?
        .id
        .ok_or_else(|| "Printables did not report an account id.".to_string())?;

    let site = client.user_collections(&account).map_err(|e| e.message)?;

    let mut data = load_collections(&lib_dir);
    upsert_site_collections(&mut data, &site);

    // Membership: without it a collection opens empty, which is not a
    // collection at all. One request per collection.
    for remote in &site {
        if let Ok(ids) = client.collection_model_ids(&remote.id) {
            apply_membership(&mut data, &remote.id, ids);
        }
    }

    save_collections(&lib_dir, &data)?;
    Ok(data.collections)
}

/// Pull every model in a collection that is not local yet.
#[tauri::command]
pub fn sync_collection(
    prefs: State<'_, Mutex<AppPrefs>>,
    auth: State<'_, crate::auth::AuthState>,
    args: SyncCollectionArgs,
) -> Result<serde_json::Value, String> {
    let snapshot = prefs.lock().map(|p| p.clone()).map_err(|e| e.to_string())?;
    let lib_dir = library_dir(&snapshot)?;
    let data = load_collections(&lib_dir);
    let collection = data
        .collections
        .iter()
        .find(|c| c.id == args.collection_id)
        .ok_or_else(|| "Collection not found.".to_string())?;

    if collection.model_ids.is_empty() {
        return Err(
            "This collection has no models yet. Import your collections first.".to_string(),
        );
    }

    let token = auth
        .token()
        .ok_or_else(|| "Sign in to Printables to download a collection.".to_string())?;
    let client =
        crate::printables::api::PrintablesClient::with_token(Some(token)).map_err(|e| e.message)?;

    let mut pulled = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();

    for id in &collection.model_ids {
        if crate::library::locator::try_find_model_dir(&lib_dir, id).is_some() {
            skipped.push(id.clone());
            continue;
        }
        match crate::printables::clone::pull_model(
            &client,
            id,
            &lib_dir,
            &crate::printables::clone::PullOptions {
                force: false,
                git: snapshot.git_enabled,
            },
            |_| {},
        ) {
            Ok(o) => pulled.push(o.model_id),
            Err(e) => failed.push(format!("{id}: {}", e.message)),
        }
    }

    Ok(serde_json::json!({ "pulled": pulled, "already_local": skipped, "failed": failed }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCollectionArgs {
    pub collection_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::printables::api::SiteCollection;

    fn site(id: &str, name: &str, count: u32) -> SiteCollection {
        SiteCollection {
            id: id.to_string(),
            name: name.to_string(),
            description: String::new(),
            prints_count: count,
        }
    }

    #[test]
    fn site_collections_are_imported_once_and_updated_after() {
        let mut data = CollectionsFile::default();
        assert_eq!(
            upsert_site_collections(&mut data, &[site("1", "Glock", 12)]),
            1
        );
        assert_eq!(data.collections.len(), 1);
        assert_eq!(data.collections[0].id, "site-1");
        assert_eq!(data.collections[0].site_count, Some(12));

        // Second run with the same data changes nothing.
        assert_eq!(
            upsert_site_collections(&mut data, &[site("1", "Glock", 12)]),
            0
        );

        // Rename/count change updates in place.
        assert_eq!(
            upsert_site_collections(&mut data, &[site("1", "Glock 17", 13)]),
            1
        );
        assert_eq!(data.collections.len(), 1);
        assert_eq!(data.collections[0].name, "Glock 17");
        assert_eq!(data.collections[0].site_count, Some(13));
    }

    #[test]
    fn membership_is_replaced_not_merged() {
        let mut data = CollectionsFile::default();
        upsert_site_collections(&mut data, &[site("7", "Glock", 2)]);
        assert!(apply_membership(
            &mut data,
            "7",
            vec!["1".into(), "2".into()]
        ));
        assert_eq!(
            data.collections[0].model_ids,
            vec!["1".to_string(), "2".to_string()]
        );

        // A later import that no longer lists model 1 must drop it.
        apply_membership(&mut data, "7", vec!["2".into()]);
        assert_eq!(data.collections[0].model_ids, vec!["2".to_string()]);

        // Unknown collections are ignored rather than invented.
        assert!(!apply_membership(&mut data, "nope", vec!["9".into()]));
    }

    #[test]
    fn importing_never_clobbers_local_collections() {
        let mut data = CollectionsFile {
            schema_version: 1,
            collections: vec![Collection {
                id: "local-uuid".into(),
                name: "Mine".into(),
                model_ids: vec!["42".into()],
                created_at: "x".into(),
                updated_at: "x".into(),
                site_id: None,
                site_count: None,
            }],
        };
        upsert_site_collections(&mut data, &[site("9", "Reloading", 4)]);
        assert_eq!(data.collections.len(), 2);
        assert_eq!(data.collections[0].model_ids, vec!["42".to_string()]);
    }
}
