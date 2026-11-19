//! Publishing local models to printables.com, and deleting them there.
//!
//! The documents below were recovered from the site's own serialized schema,
//! which the SPA ships in its bundle:
//!
//! ```text
//! modelUpdate(aiGenerated authorship category description draft
//!             excludeCommercialUsage gcodes[] id images[] license mainImage
//!             name nsfw otherFiles[] politicalContent premium printer
//!             published ... slas[] stls[] summary tags[] variationOf ...)
//!   -> ModelCreateUpdateMutation { ok errors { field messages } }
//! fileUploadCreate(filename! imageHash imageHeight imageWidth kind!)
//!   -> { fileUpload { id } uploadData { url fields } }
//! fileUploadFinished(uuid!)
//!   -> { output { id } }
//! printDelete(id: Any!)
//! ```
//!
//! The API requires a model to have at least one image and one file, which is
//! exactly why this uploads media *first* and only then creates the model.
//! Everything here is gated behind `remote_write_enabled`.

use crate::config;
use crate::error::{AppError, AppResult};
use crate::library::locator;
use crate::models::AppPrefs;
use crate::printables::api::PrintablesClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Mutex;
use tauri::State;

pub const FILE_UPLOAD_CREATE: &str = r#"
mutation FileUploadCreate($filename: Any!, $kind: Any!) {
  fileUploadCreate(filename: $filename, kind: $kind) {
    fileUpload { id __typename }
    uploadData { url fields __typename }
    errors { field messages __typename }
    __typename
  }
}
"#;

pub const FILE_UPLOAD_FINISHED: &str = r#"
mutation FileUploadFinished($uuid: Any!) {
  fileUploadFinished(uuid: $uuid) {
    ok
    errors { field messages __typename }
    output { id __typename }
    __typename
  }
}
"#;

pub const MODEL_UPDATE: &str = r#"
mutation ModelUpdate(
  $id: Any, $name: Any, $description: Any, $summary: Any, $tags: Any,
  $category: Any, $license: Any, $printer: Any, $draft: Any, $published: Any,
  $images: Any, $mainImage: Any, $stls: Any, $gcodes: Any, $slas: Any,
  $otherFiles: Any
) {
  modelUpdate(
    id: $id, name: $name, description: $description, summary: $summary,
    tags: $tags, category: $category, license: $license, printer: $printer,
    draft: $draft, published: $published, images: $images, mainImage: $mainImage,
    stls: $stls, gcodes: $gcodes, slas: $slas, otherFiles: $otherFiles
  ) {
    ok
    errors { field messages __typename }
    __typename
  }
}
"#;

pub const PRINT_DELETE: &str = r#"
mutation PrintDelete($id: Any!) {
  printDelete(id: $id) {
    ok
    errors { field messages __typename }
    __typename
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishArgs {
    pub model_id: String,
    /// Publish as an unpublished draft (the safe default).
    #[serde(default)]
    pub draft: Option<bool>,
}

fn upload_kind(file_type: &str) -> &'static str {
    match file_type {
        "stl" => "stl",
        "gcode" => "gcode",
        "sla" => "sla",
        _ => "other",
    }
}

/// Ask for an upload slot, POST the bytes, then finalise and return the id.
fn upload_one(client: &PrintablesClient, path: &Path, kind: &str) -> AppResult<String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());

    let data = client.execute_graphql(&json!({
        "operationName": "FileUploadCreate",
        "query": FILE_UPLOAD_CREATE,
        "variables": { "filename": name, "kind": kind }
    }))?;

    let node = data
        .pointer("/data/fileUploadCreate")
        .filter(|v| !v.is_null())
        .ok_or_else(|| AppError::graphql("Upload slot request returned nothing"))?;

    let uuid = node
        .pointer("/fileUpload/id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::graphql("Upload slot had no id"))?
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
        "operationName": "FileUploadFinished",
        "query": FILE_UPLOAD_FINISHED,
        "variables": { "uuid": uuid }
    }))?;
    let finished = done
        .pointer("/data/fileUploadFinished")
        .filter(|v| !v.is_null())
        .ok_or_else(|| AppError::graphql("Upload could not be finalised"))?;
    if !finished
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(AppError::graphql(format!(
            "Upload finalise failed: {}",
            finished.get("errors").cloned().unwrap_or(Value::Null)
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

    let mut stls = Vec::new();
    let mut gcodes = Vec::new();
    let mut slas = Vec::new();
    let mut other = Vec::new();
    for file in &meta.files {
        let path = model_dir.join(&file.local_path);
        if !path.exists() {
            continue;
        }
        let id = upload_one(client, &path, upload_kind(&file.kind))?;
        match upload_kind(&file.kind) {
            "stl" => stls.push(id),
            "gcode" => gcodes.push(id),
            "sla" => slas.push(id),
            _ => other.push(id),
        }
    }

    if image_ids.is_empty()
        || (stls.is_empty() && gcodes.is_empty() && slas.is_empty() && other.is_empty())
    {
        return Err(AppError::invalid(
            "printables.com requires at least one image and one model file before a model can be created.",
        ));
    }

    // A model that already has a printables id is an update, not a create.
    let remote_id = if meta.model_id.chars().all(|c| c.is_ascii_digit()) {
        Some(meta.model_id.clone())
    } else {
        None
    };

    let description_html = crate::sync::push::text_to_html(&meta.description);
    let variables = json!({
        "id": remote_id,
        "name": meta.name,
        "description": description_html,
        "tags": meta.tags,
        "draft": draft,
        "published": !draft,
        "images": image_ids,
        "mainImage": image_ids.first().cloned(),
        "stls": stls,
        "gcodes": gcodes,
        "slas": slas,
        "otherFiles": other,
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
    let errors: Vec<String> = node
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|e| {
                    let field = e.get("field").and_then(|v| v.as_str()).unwrap_or("?");
                    let msgs = e
                        .get("messages")
                        .and_then(|v| v.as_array())
                        .map(|m| {
                            m.iter()
                                .filter_map(|x| x.as_str())
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .unwrap_or_default();
                    format!("{field}: {msgs}")
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(PublishReport {
        model_id: meta.model_id,
        images_uploaded: image_ids.len(),
        files_uploaded: stls.len() + gcodes.len() + slas.len() + other.len(),
        ok,
        errors,
        remote_id,
        draft,
    })
}

/// Delete a model on printables.com. Irreversible.
pub fn delete_remote(client: &PrintablesClient, remote_id: &str) -> AppResult<()> {
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
            node.get("errors").cloned().unwrap_or(Value::Null)
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

    #[test]
    fn model_update_document_carries_every_required_argument() {
        for arg in [
            "id",
            "name",
            "description",
            "tags",
            "draft",
            "published",
            "images",
            "mainImage",
            "stls",
            "gcodes",
            "slas",
            "otherFiles",
        ] {
            assert!(
                MODEL_UPDATE.contains(&format!("${arg}: Any")),
                "MODEL_UPDATE is missing {arg}"
            );
        }
    }

    #[test]
    fn upload_documents_match_the_schema() {
        assert!(FILE_UPLOAD_CREATE.contains("fileUploadCreate(filename: $filename, kind: $kind)"));
        assert!(FILE_UPLOAD_CREATE.contains("uploadData { url fields"));
        assert!(FILE_UPLOAD_FINISHED.contains("fileUploadFinished(uuid: $uuid)"));
        assert!(PRINT_DELETE.contains("printDelete(id: $id)"));
    }

    #[test]
    fn upload_kinds_follow_the_file_type() {
        assert_eq!(upload_kind("stl"), "stl");
        assert_eq!(upload_kind("gcode"), "gcode");
        assert_eq!(upload_kind("sla"), "sla");
        assert_eq!(upload_kind("other"), "other");
    }
}
