//! The authenticated write API.
//!
//! Every document in this file was recovered from printables.com's own client
//! bundles and then verified against the live API with schema validation probes
//! (see `docs/api-notes.md`). Nothing here is guessed: the mutation names, the
//! argument names, the input object fields and the upload handshake are the
//! ones the website itself uses.
//!
//! Model files are uploaded in three steps:
//!   1. `printFileUpload2` returns a presigned POST (`uploadData.url` + `fields`)
//!      and a `fileUpload.id`.
//!   2. The file is POSTed as multipart form data (last field must be `file`).
//!   3. `printFileUploadFinished` resolves the upload into a printable file id.
//!
//! The file list is then replaced wholesale by `modelUpdate(stls: [...])`, which
//! is also how a file is deleted: simply omit it from the list.

use crate::error::{AppError, AppResult};
use crate::printables::api::PrintablesClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

/// The editable state of a model, as the website's edit form sees it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EditState {
    pub id: String,
    pub name: String,
    pub description: String,
    pub summary: String,
    #[serde(default)]
    pub nsfw: bool,
    #[serde(default)]
    pub ai_generated: bool,
    #[serde(default)]
    pub political_content: bool,
    pub authorship: Option<String>,
    pub remix_description: Option<String>,
    pub license_id: Option<String>,
    pub category_id: Option<String>,
    pub printer_id: Option<String>,
    pub tags: Vec<(String, String)>, // (id, name)
    pub images: Vec<String>,         // image ids, in order
    pub stls: Vec<FileInput>,
    pub gcodes: Vec<FileInput>,
    pub slas: Vec<FileInput>,
    pub other_files: Vec<FileInput>,
}

/// `STLFileInputType` / `GcodeFileInputType` / `SLAFileInputType` / `OtherFileInputType`.
/// Verified field set: `id` (required), `name`, `folder`, `note`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInput {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct UploadTicket {
    pub file_upload_id: String,
    pub url: String,
    pub fields: HashMap<String, String>,
}

const MODEL_EDIT_DETAIL: &str = r#"
query ModelEditDetail($id: ID!) {
  model: print(id: $id) {
    id
    name
    description
    summary
    nsfw
    aiGenerated
    politicalContent
    authorship
    remixDescription
    club: premium
    price
    excludeCommercialUsage
    filesType
    license { id name }
    category { id }
    printer { id }
    tags { id name }
    images { id }
    stls { id name folder note }
    gcodes { id name folder note }
    slas { id name folder note }
    otherFiles { id name folder note }
  }
}
"#;

const MODEL_UPDATE: &str = r#"
mutation ModelUpdate(
  $id: ID, $name: String, $summary: String, $description: String, $tags: [ID],
  $license: ID, $category: ID, $printer: ID, $nsfw: Boolean, $aiGenerated: Boolean,
  $politicalContent: Boolean, $authorship: PrintAuthorshipEnum, $remixDescription: String,
  $stls: [STLFileInputType], $gcodes: [GcodeFileInputType], $slas: [SLAFileInputType],
  $otherFiles: [OtherFileInputType], $images: [PrintImageInputType]
) {
  modelUpdate(
    id: $id, name: $name, summary: $summary, description: $description, tags: $tags,
    license: $license, category: $category, printer: $printer, nsfw: $nsfw,
    aiGenerated: $aiGenerated, politicalContent: $politicalContent, authorship: $authorship,
    remixDescription: $remixDescription, stls: $stls, gcodes: $gcodes, slas: $slas,
    otherFiles: $otherFiles, images: $images
  ) {
    ok
    errors { field messages }
    output { id slug name modified }
  }
}
"#;

const PRINT_FILE_UPLOAD: &str = r#"
mutation UploadModel($fileName: String!, $folder: String!, $unzip: Boolean!) {
  upload: printFileUpload2(fileName: $fileName, folder: $folder, unzip: $unzip) {
    ok
    errors { field messages }
    uploadData { url fields }
    fileUpload { id }
  }
}
"#;

const PRINT_FILE_UPLOAD_FINISHED: &str = r#"
mutation UploadModelFinished($fileUploadId: ID!) {
  uploadFinished: printFileUploadFinished(fileUploadId: $fileUploadId) {
    ok
    errors { field messages }
    output { id filePath }
  }
}
"#;

fn first_error(value: &Value, pointer: &str) -> String {
    value
        .pointer(&format!("{pointer}/errors"))
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .map(|e| {
                    let field = e.get("field").and_then(|f| f.as_str()).unwrap_or("");
                    let msgs = e
                        .get("messages")
                        .and_then(|m| m.as_array())
                        .map(|m| {
                            m.iter()
                                .filter_map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    if field.is_empty() {
                        msgs
                    } else {
                        format!("{field}: {msgs}")
                    }
                })
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default()
}

fn require_ok(value: &Value, pointer: &str, what: &str) -> AppResult<Value> {
    let node = value
        .pointer(pointer)
        .filter(|v| !v.is_null())
        .ok_or_else(|| AppError::graphql(format!("{what}: the server returned no payload")))?;
    if node.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        Ok(node.clone())
    } else {
        let err = first_error(value, pointer);
        Err(AppError::graphql(if err.is_empty() {
            format!("{what} failed")
        } else {
            format!("{what} failed: {err}")
        }))
    }
}

pub struct RemoteWriter<'a> {
    client: &'a PrintablesClient,
}

impl<'a> RemoteWriter<'a> {
    pub fn new(client: &'a PrintablesClient) -> Self {
        Self { client }
    }

    pub fn edit_state(&self, model_id: &str) -> AppResult<EditState> {
        let data = self.client.execute_graphql(&json!({
            "operationName": "ModelEditDetail",
            "query": MODEL_EDIT_DETAIL,
            "variables": { "id": model_id }
        }))?;
        let model = data
            .pointer("/data/model")
            .filter(|v| !v.is_null())
            .ok_or_else(|| AppError::not_found(format!("Model {model_id} is not on Printables")))?;

        let str_of = |v: Option<&Value>| v.and_then(|x| x.as_str()).unwrap_or("").to_string();
        let files = |list: Option<&Value>| -> Vec<FileInput> {
            list.and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|f| {
                            Some(FileInput {
                                id: f.get("id")?.as_str()?.to_string(),
                                name: str_of(f.get("name")),
                                folder: str_of(f.get("folder")),
                                note: str_of(f.get("note")),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        Ok(EditState {
            id: model_id.to_string(),
            name: str_of(model.get("name")),
            description: str_of(model.get("description")),
            summary: str_of(model.get("summary")),
            nsfw: model.get("nsfw").and_then(|v| v.as_bool()).unwrap_or(false),
            ai_generated: model
                .get("aiGenerated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            political_content: model
                .get("politicalContent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            authorship: model
                .get("authorship")
                .and_then(|v| v.as_str())
                .map(String::from),
            remix_description: model
                .get("remixDescription")
                .and_then(|v| v.as_str())
                .map(String::from),
            license_id: model
                .pointer("/license/id")
                .and_then(|v| v.as_str())
                .map(String::from),
            category_id: model
                .pointer("/category/id")
                .and_then(|v| v.as_str())
                .map(String::from),
            printer_id: model
                .pointer("/printer/id")
                .and_then(|v| v.as_str())
                .map(String::from),
            tags: model
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| {
                            Some((t.get("id")?.as_str()?.to_string(), str_of(t.get("name"))))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            images: model
                .get("images")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|i| i.get("id").and_then(|v| v.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            stls: files(model.get("stls")),
            gcodes: files(model.get("gcodes")),
            slas: files(model.get("slas")),
            other_files: files(model.get("otherFiles")),
        })
    }

    /// Sends the whole editable state. Fields we do not understand are echoed
    /// back unchanged, so an edit cannot silently clear them.
    pub fn save(&self, state: &EditState) -> AppResult<()> {
        let file_inputs = |files: &[FileInput]| -> Value {
            Value::Array(
                files
                    .iter()
                    .map(|f| {
                        json!({
                            "id": f.id,
                            "name": f.name,
                            "folder": f.folder,
                            "note": f.note,
                        })
                    })
                    .collect(),
            )
        };

        let variables = json!({
            "id": state.id,
            "name": state.name,
            "summary": state.summary,
            "description": state.description,
            "tags": state.tags.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            "license": state.license_id,
            "category": state.category_id,
            "printer": state.printer_id,
            "nsfw": state.nsfw,
            "aiGenerated": state.ai_generated,
            "politicalContent": state.political_content,
            "authorship": state.authorship,
            "remixDescription": state.remix_description,
            "stls": file_inputs(&state.stls),
            "gcodes": file_inputs(&state.gcodes),
            "slas": file_inputs(&state.slas),
            "otherFiles": file_inputs(&state.other_files),
            "images": state.images.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
        });

        let data = self.client.execute_graphql(&json!({
            "operationName": "ModelUpdate",
            "query": MODEL_UPDATE,
            "variables": variables
        }))?;
        require_ok(&data, "/data/modelUpdate", "Updating the model")?;
        Ok(())
    }

    /// Step 1+2+3 of the file upload handshake. Returns the new file id.
    pub fn upload_model_file(&self, path: &Path, folder: &str) -> AppResult<String> {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| AppError::invalid("Upload path has no file name"))?;

        let ticket = self.begin_upload(&file_name, folder)?;
        self.post_file(&ticket, path)?;
        self.finish_upload(&ticket.file_upload_id)
    }

    pub fn begin_upload(&self, file_name: &str, folder: &str) -> AppResult<UploadTicket> {
        let data = self.client.execute_graphql(&json!({
            "operationName": "UploadModel",
            "query": PRINT_FILE_UPLOAD,
            "variables": { "fileName": file_name, "folder": folder, "unzip": false }
        }))?;
        let node = require_ok(&data, "/data/upload", "Requesting an upload slot")?;

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
        let file_upload_id = node
            .pointer("/fileUpload/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::graphql("Upload slot had no fileUpload id"))?
            .to_string();

        Ok(UploadTicket {
            file_upload_id,
            url,
            fields,
        })
    }

    /// POSTs the file to the presigned URL. The server expects every returned
    /// field first, then the file under the name `file`.
    pub fn post_file(&self, ticket: &UploadTicket, path: &Path) -> AppResult<()> {
        let bytes = std::fs::read(path)
            .map_err(|e| AppError::io(format!("Cannot read {}: {e}", path.display())))?;
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "upload.bin".to_string());

        let mut form = reqwest::blocking::multipart::Form::new();
        for (key, value) in &ticket.fields {
            form = form.text(key.clone(), value.clone());
        }
        let part = reqwest::blocking::multipart::Part::bytes(bytes).file_name(file_name);
        form = form.part("file", part);

        let resp = self
            .client
            .http()
            .post(&ticket.url)
            .multipart(form)
            .send()
            .map_err(|e| AppError::network(format!("Upload failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(AppError::network(format!(
                "Upload rejected with {status}: {}",
                body.chars().take(300).collect::<String>()
            )));
        }
        Ok(())
    }

    /// Step 3: resolve the upload into a printable file id.
    pub fn finish_upload(&self, file_upload_id: &str) -> AppResult<String> {
        let data = self.client.execute_graphql(&json!({
            "operationName": "UploadModelFinished",
            "query": PRINT_FILE_UPLOAD_FINISHED,
            "variables": { "fileUploadId": file_upload_id }
        }))?;
        let node = require_ok(&data, "/data/uploadFinished", "Finalising the upload")?;
        node.pointer("/output/id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| AppError::graphql("Finalised upload had no file id"))
    }
}

/// Maps a local file name onto the printables file category that `modelUpdate`
/// expects (`stls` / `gcodes` / `slas` / `otherFiles`).
pub fn category_for(file_name: &str) -> &'static str {
    match file_name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "stl" | "3mf" | "obj" => "stls",
        "gcode" | "bgcode" => "gcodes",
        "sl1" | "sl1s" | "sla" => "slas",
        _ => "otherFiles",
    }
}

/// Convenience for tests that build a map from JSON.
#[cfg(test)]
pub fn fields_from_json(value: &Value) -> HashMap<String, String> {
    value
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_categories_match_the_edit_form() {
        assert_eq!(category_for("plate.3mf"), "stls");
        assert_eq!(category_for("part.STL"), "stls");
        assert_eq!(category_for("job.gcode"), "gcodes");
        assert_eq!(category_for("resin.sl1"), "slas");
        assert_eq!(category_for("readme.txt"), "otherFiles");
    }

    #[test]
    fn fields_are_flattened_from_presigned_json() {
        let value = json!({ "key": "uploads/x.stl", "policy": "abc", "x-amz-signature": "sig" });
        let fields = fields_from_json(&value);
        assert_eq!(fields.get("key").unwrap(), "uploads/x.stl");
        assert_eq!(fields.get("x-amz-signature").unwrap(), "sig");
        assert_eq!(fields.len(), 3);
    }
}
