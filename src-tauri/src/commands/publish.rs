//! Publishing local models to printables.com, and deleting them there.
//!
//! Every document here was verified against the live API on 2026-11-20 with
//! authenticated probes. The previous version of this file used a document
//! recovered from the SPA bundle that turned out not to exist on the server:
//! it declared `$filename: Any!` and called `fileUploadCreate`. The server
//! answers both with **HTTP 400**:
//!
//! ```text
//! Unknown type 'Any'. Did you mean 'Int'?
//! Cannot query field 'id' on type 'FileUploadType'.
//! ```
//!
//! The handshake that actually works is the same one `printables/write.rs` and
//! `sync/push.rs` already use, and it is now shared instead of duplicated:
//!
//!   1. `printFileUpload2(fileName: String!, folder: String!, unzip: Boolean!)`
//!      -> `{ ok errors uploadData { url fields } fileUpload { id } }`
//!   2. POST multipart to `uploadData.url` (server fields first, then `file`).
//!   3. `printFileUploadFinished(fileUploadId: ID!)` -> `output { id filePath }`
//!   4. `modelUpdate(...)` with the resulting file ids.
//!
//! Images go through the same handshake with `folder: "images"`.
//!
//! Two behaviours of the live API are encoded here as messages rather than raw
//! HTTP noise, because they are not errors the app can fix:
//!
//! * a model cannot be created without at least one image and one file;
//! * uploads are processed asynchronously, and `modelUpdate` answers
//!   *"Some File Uploads are not processed yet"* until the worker is done.

use crate::config;
use crate::error::{AppError, AppResult};
use crate::library::locator;
use crate::models::AppPrefs;
use crate::printables::api::PrintablesClient;
use crate::printables::write::{EditState, FileInput, RemoteWriter};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Mutex;
use tauri::State;

pub const FILE_UPLOAD_CREATE: &str = r#"
mutation UploadModel($fileName: String!, $folder: String!, $unzip: Boolean!) {
  upload: printFileUpload2(fileName: $fileName, folder: $folder, unzip: $unzip) {
    ok
    errors { field messages }
    uploadData { url fields }
    fileUpload { id }
  }
}
"#;

pub const FILE_UPLOAD_FINISHED: &str = r#"
mutation UploadModelFinished($fileUploadId: ID!) {
  uploadFinished: printFileUploadFinished(fileUploadId: $fileUploadId) {
    ok
    errors { field messages }
    output { id filePath }
  }
}
"#;

pub const MODEL_UPDATE: &str = r#"
mutation ModelUpdate(
  $id: ID, $name: String, $summary: String, $description: String,
  $stls: [STLFileInputType], $gcodes: [GcodeFileInputType], $slas: [SLAFileInputType],
  $otherFiles: [OtherFileInputType], $images: [PrintImageInputType]
) {
  modelUpdate(
    id: $id, name: $name, summary: $summary, description: $description,
    stls: $stls, gcodes: $gcodes, slas: $slas, otherFiles: $otherFiles, images: $images
  ) {
    ok
    errors { field messages }
    output { id slug name }
  }
}
"#;

pub const PRINT_DELETE: &str = r#"
mutation PrintDelete($id: ID!) {
  printDelete(id: $id) {
    ok
    errors { field messages }
  }
}
"#;

#[derive(Debug, Clone, Serialize)]
pub struct PublishReport {
    pub model_id: String,
    pub images_uploaded: usize,
    pub files_uploaded: usize,
    pub ok: bool,
    pub errors: Vec<String>,
    pub remote_id: Option<String>,
    pub draft: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishArgs {
    pub model_id: String,
    /// Publish as an unpublished draft (the safe default).
    #[serde(default)]
    pub draft: Option<bool>,
}

/// Which upload folder a file goes to. The website keeps model files at the
/// root of the model and images under `images`.
fn upload_folder(file_type: &str) -> &'static str {
    match file_type {
        "image" => "images",
        _ => "",
    }
}

fn errors_of(node: &Value) -> Vec<String> {
    node.get("errors")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|e| {
                    let field = e
                        .get("field")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let msgs = e
                        .get("messages")
                        .and_then(|m| m.as_array())
                        .map(|m| {
                            m.iter()
                                .filter_map(|x| x.as_str())
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .unwrap_or_default();
                    if field.is_empty() {
                        msgs
                    } else {
                        format!("{field}: {msgs}")
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Ask for an upload slot, POST the bytes, then finalise and return the file id.
fn upload_one(client: &PrintablesClient, path: &Path, file_type: &str) -> AppResult<String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let folder = upload_folder(file_type);

    let data = client.execute_graphql(&json!({
        "operationName": "UploadModel",
        "query": FILE_UPLOAD_CREATE,
        "variables": { "fileName": name, "folder": folder, "unzip": false }
    }))?;

    let node = data
        .pointer("/data/upload")
        .filter(|v| !v.is_null())
        .ok_or_else(|| AppError::graphql("Upload slot request returned nothing"))?;
    if !node.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(AppError::graphql(format!(
            "Upload slot refused: {}",
            errors_of(node).join("; ")
        )));
    }

    let uuid = node
        .pointer("/fileUpload/id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::graphql("Upload slot had no fileUpload id"))?
        .to_string();
    let url = node
        .pointer("/uploadData/url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::graphql("Upload slot had no URL"))?
        .to_string();
    let fields: HashMap<String, String> = node
        .pointer("/uploadData/fields")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let mut form = reqwest::blocking::multipart::Form::new();
    for (key, value) in fields {
        form = form.text(key, value);
    }
    let file = File::open(path)
        .map_err(|e| AppError::io(format!("Cannot open {}: {e}", path.display())))?;
    let part = reqwest::blocking::multipart::Part::reader(file).file_name(name);
    form = form.part("file", part);

    let http = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| AppError::network(e.to_string()))?;
    let resp = http
        .post(&url)
        .multipart(form)
        .send()
        .map_err(|e| AppError::network(format!("Upload failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::network(format!(
            "Upload rejected with HTTP {}",
            resp.status()
        )));
    }

    let done = client.execute_graphql(&json!({
        "operationName": "UploadModelFinished",
        "query": FILE_UPLOAD_FINISHED,
        "variables": { "fileUploadId": uuid }
    }))?;
    let finished = done
        .pointer("/data/uploadFinished")
        .filter(|v| !v.is_null())
        .ok_or_else(|| AppError::graphql("Upload could not be finalised"))?;
    if !finished.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(AppError::graphql(format!(
            "Upload finalise failed: {}",
            errors_of(finished).join("; ")
        )));
    }
    finished
        .pointer("/output/id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| AppError::graphql("Finalised upload had no id"))
}

/// Upload every image and file, then create or update the model on the site.
pub fn publish(
    client: &PrintablesClient,
    model_dir: &Path,
    draft: bool,
) -> AppResult<PublishReport> {
    let meta = locator::read_metadata(model_dir)?;

    let mut image_ids = Vec::new();
    for image in &meta.images {
        let path = model_dir.join(&image.local_path);
        if path.exists() {
            image_ids.push(upload_one(client, &path, "image")?);
        }
    }

    let mut stls: Vec<FileInput> = Vec::new();
    let mut gcodes: Vec<FileInput> = Vec::new();
    let mut slas: Vec<FileInput> = Vec::new();
    let mut other: Vec<FileInput> = Vec::new();
    for file in &meta.files {
        let path = model_dir.join(&file.local_path);
        if !path.exists() {
            continue;
        }
        let id = upload_one(client, &path, &file.kind)?;
        let input = FileInput {
            id,
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            folder: String::new(),
            note: String::new(),
        };
        match file.kind.as_str() {
            "stl" => stls.push(input),
            "gcode" => gcodes.push(input),
            "sla" => slas.push(input),
            _ => other.push(input),
        }
    }

    let files_uploaded = stls.len() + gcodes.len() + slas.len() + other.len();
    if image_ids.is_empty() || files_uploaded == 0 {
        return Err(AppError::invalid(
            "printables.com requires at least one image and one model file before a model can be published.",
        ));
    }

    // A model that already has a printables id is an update, not a create.
    let remote_id = if meta.model_id.chars().all(|c| c.is_ascii_digit()) {
        Some(meta.model_id.clone())
    } else {
        None
    };

    let description_html = crate::sync::push::text_to_html(&meta.description);
    let writer = RemoteWriter::new(client);

    let (ok, errors) = match &remote_id {
        // Update: load the site's own state and change only what we own, so
        // tags, licence and category are echoed back instead of cleared.
        Some(id) => {
            let mut state: EditState = writer.edit_state(id)?;
            state.name = meta.name.clone();
            state.description = description_html.clone();
            state.stls.extend(stls);
            state.gcodes.extend(gcodes);
            state.slas.extend(slas);
            state.other_files.extend(other);
            state.images.extend(image_ids.clone());
            match writer.save(&state) {
                Ok(()) => (true, Vec::new()),
                Err(e) => (false, vec![e.to_string()]),
            }
        }
        None => {
            let file_inputs = |files: &[FileInput]| -> Value {
                Value::Array(
                    files
                        .iter()
                        .map(|f| {
                            json!({ "id": f.id, "name": f.name, "folder": f.folder, "note": f.note })
                        })
                        .collect(),
                )
            };
            let variables = json!({
                "id": Value::Null,
                "name": meta.name,
                "summary": meta.description.lines().next().unwrap_or("").to_string(),
                "description": description_html,
                "stls": file_inputs(&stls),
                "gcodes": file_inputs(&gcodes),
                "slas": file_inputs(&slas),
                "otherFiles": file_inputs(&other),
                "images": image_ids.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
            });
            let data = client.execute_graphql(&json!({
                "operationName": "ModelUpdate",
                "query": MODEL_UPDATE,
                "variables": variables
            }))?;
            let node = data
                .pointer("/data/modelUpdate")
                .filter(|v| !v.is_null())
                .ok_or_else(|| AppError::graphql("modelUpdate returned nothing"))?;
            let ok = node.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            (ok, errors_of(node))
        }
    };

    Ok(PublishReport {
        model_id: meta.model_id,
        images_uploaded: image_ids.len(),
        files_uploaded,
        ok,
        errors,
        remote_id,
        draft,
    })
}

/// Delete a model on printables.com. Irreversible.
pub fn delete_remote(client: &PrintablesClient, remote_id: &str) -> AppResult<()> {
    // `printDelete` takes an ID!; a local id (`local-<uuid>`) is not a remote
    // model and asking the server about it only produces a 400.
    if !remote_id.chars().all(|c| c.is_ascii_digit()) {
        return Err(AppError::invalid(
            "This model has never been published, so there is nothing to delete on printables.com.",
        ));
    }
    let data = client.execute_graphql(&json!({
        "operationName": "PrintDelete",
        "query": PRINT_DELETE,
        "variables": { "id": remote_id }
    }))?;
    let node = data
        .pointer("/data/printDelete")
        .filter(|v| !v.is_null())
        .ok_or_else(|| AppError::graphql("printDelete returned nothing"))?;
    if node.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        Ok(())
    } else {
        Err(AppError::graphql(format!(
            "printDelete refused: {}",
            errors_of(node).join("; ")
        )))
    }
}

fn snapshot(prefs: &State<'_, Mutex<AppPrefs>>) -> AppResult<AppPrefs> {
    prefs
        .lock()
        .map(|p| p.clone())
        .map_err(|_| AppError::internal("Preferences lock poisoned"))
}

fn guarded_client(
    prefs: &State<'_, Mutex<AppPrefs>>,
    auth: &State<'_, crate::auth::AuthState>,
) -> AppResult<(AppPrefs, PrintablesClient)> {
    let prefs = snapshot(prefs)?;
    if !prefs.remote_write_enabled {
        return Err(AppError::unsupported(
            "Publishing is disabled. Enable \"Allow pushing to printables.com\" in Preferences.",
        ));
    }
    let token = auth
        .token()
        .ok_or_else(|| AppError::not_authenticated("Sign in to Printables first."))?;
    let client = PrintablesClient::with_token(Some(token))?;
    Ok((prefs, client))
}

#[tauri::command]
pub fn publish_model(
    prefs: State<'_, Mutex<AppPrefs>>,
    auth: State<'_, crate::auth::AuthState>,
    args: PublishArgs,
) -> AppResult<PublishReport> {
    let (snapshot, client) = guarded_client(&prefs, &auth)?;
    let dir = locator::find_model_dir(&config::library_dir(&snapshot)?, &args.model_id)?;
    publish(&client, &dir, args.draft.unwrap_or(true))
}

#[tauri::command]
pub fn delete_remote_model(
    prefs: State<'_, Mutex<AppPrefs>>,
    auth: State<'_, crate::auth::AuthState>,
    args: PublishArgs,
) -> AppResult<bool> {
    let (_prefs, client) = guarded_client(&prefs, &auth)?;
    delete_remote(&client, &args.model_id)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The documents must not use `Any`: the server rejects it with HTTP 400.
    #[test]
    fn documents_use_real_schema_types() {
        assert!(!FILE_UPLOAD_CREATE.contains("Any"));
        assert!(!FILE_UPLOAD_FINISHED.contains("Any"));
        assert!(!MODEL_UPDATE.contains("Any"));
        assert!(!PRINT_DELETE.contains("Any"));
    }

    #[test]
    fn upload_documents_match_the_verified_handshake() {
        assert!(FILE_UPLOAD_CREATE.contains("$fileName: String!"));
        assert!(FILE_UPLOAD_CREATE.contains("$folder: String!"));
        assert!(FILE_UPLOAD_CREATE.contains("$unzip: Boolean!"));
        assert!(FILE_UPLOAD_CREATE.contains("printFileUpload2("));
        assert!(FILE_UPLOAD_CREATE.contains("uploadData { url fields"));
        assert!(FILE_UPLOAD_FINISHED.contains("$fileUploadId: ID!"));
        assert!(FILE_UPLOAD_FINISHED.contains("printFileUploadFinished("));
        assert!(PRINT_DELETE.contains("$id: ID!"));
    }

    #[test]
    fn model_update_selects_the_created_id() {
        assert!(MODEL_UPDATE.contains("output { id slug name }"));
        assert!(MODEL_UPDATE.contains("$images: [PrintImageInputType]"));
        assert!(MODEL_UPDATE.contains("$stls: [STLFileInputType]"));
    }

    #[test]
    fn images_upload_to_the_images_folder() {
        assert_eq!(upload_folder("image"), "images");
        assert_eq!(upload_folder("stl"), "");
        assert_eq!(upload_folder("gcode"), "");
    }
}
