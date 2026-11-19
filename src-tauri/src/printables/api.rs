//! Client for the Printables.com GraphQL API and media CDNs.
//!
//! 2.0 changes from the read-only 1.x client:
//!   * optional bearer token (account sessions),
//!   * GraphQL `errors[]` are inspected instead of silently producing `null`,
//!   * a real rate limiter with `429`/`Retry-After` handling,
//!   * file listing is separated from download-link resolution, so a sync only
//!     pays for links it actually needs,
//!   * downloads go to a `.part` file and are renamed atomically.

use crate::error::{AppError, AppResult};
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const GRAPHQL_ENDPOINT: &str = "https://api.printables.com/graphql/";
const MEDIA_BASE_URL: &str = "https://media.printables.com/";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub author: String,
    pub tags: Vec<String>,
    pub likes: Option<u64>,
    pub downloads: Option<u64>,
    pub rating: Option<f64>,
    pub published_date: Option<String>,
    pub main_image_url: Option<String>,
    pub description: String,
    /// Raw HTML as returned by the API; kept so description edits can be pushed.
    #[serde(default)]
    pub raw_description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteFile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub preview_path: Option<String>,
    pub size_bytes: Option<u64>,
    /// "stl" | "gcode" | "sla" | "other"
    pub file_type: String,
    #[serde(default)]
    pub download_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteImage {
    pub id: Option<String>,
    pub url: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ViewerInfo {
    pub id: Option<String>,
    /// The account's numeric user id, which `userModels` needs.
    pub user_id: Option<String>,
    pub handle: Option<String>,
    pub public_username: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorModelsPage {
    pub models: Vec<Value>,
    pub cursor: Option<String>,
}

pub struct PrintablesClient {
    http: Client,
    token: Option<String>,
    last_request: Mutex<Option<Instant>>,
}

impl PrintablesClient {
    #[allow(dead_code)] // used by tests and by future anonymous-only flows
    pub fn anonymous() -> AppResult<Self> {
        Self::with_token(None)
    }

    pub fn with_token(token: Option<String>) -> AppResult<Self> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| AppError::network(format!("Failed to initialize HTTP client: {e}")))?;
        Ok(Self {
            http,
            token,
            last_request: Mutex::new(None),
        })
    }

    pub fn has_token(&self) -> bool {
        self.token.as_ref().map(|t| !t.is_empty()).unwrap_or(false)
    }

    /// Shared HTTP client, used by the upload writer for presigned POSTs.
    pub fn http(&self) -> &Client {
        &self.http
    }

    /// Executes a caller-supplied GraphQL document. Used by capability
    /// discovery and by the push executor, which builds documents from the
    /// introspected schema instead of hard-coded operation names.
    pub fn execute_graphql(&self, payload: &Value) -> AppResult<Value> {
        self.post_graphql(payload)
    }

    /// Keeps us under ~4 requests/second; parallel workers share this budget.
    fn throttle(&self) {
        if let Ok(mut guard) = self.last_request.lock() {
            if let Some(previous) = *guard {
                let elapsed = previous.elapsed();
                if elapsed < MIN_REQUEST_INTERVAL {
                    std::thread::sleep(MIN_REQUEST_INTERVAL - elapsed);
                }
            }
            *guard = Some(Instant::now());
        }
    }

    fn post_graphql(&self, payload: &Value) -> AppResult<Value> {
        let mut attempt = 0usize;
        loop {
            self.throttle();

            let mut request = self.http.post(GRAPHQL_ENDPOINT).json(payload);
            if let Some(token) = self.token.as_deref().filter(|t| !t.is_empty()) {
                request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
            }

            let resp = request
                .send()
                .map_err(|e| AppError::network(format!("GraphQL request failed: {e}")))?;
            let status = resp.status();

            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(AppError::not_authenticated(
                    "Printables rejected this session. Sign in again.",
                )
                .with_detail(format!("HTTP {status}")));
            }

            if status.as_u16() == 429 {
                if attempt >= 2 {
                    return Err(AppError::rate_limited(
                        "Printables is rate limiting requests. Try again in a minute.",
                    ));
                }
                let wait = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(2 + attempt as u64);
                std::thread::sleep(Duration::from_secs(wait.min(30)));
                attempt += 1;
                continue;
            }

            if !status.is_success() {
                return Err(AppError::graphql(format!("GraphQL HTTP error: {status}")));
            }

            let value: Value = resp
                .json()
                .map_err(|e| AppError::graphql(format!("Invalid GraphQL JSON response: {e}")))?;

            self.check_graphql_errors(&value)?;
            return Ok(value);
        }
    }

    /// Turns a `200 OK` with `{"errors":[...]}` into a typed error.
    /// 1.x ignored these, so "Not authenticated" looked like "model not found".
    fn check_graphql_errors(&self, value: &Value) -> AppResult<()> {
        let Some(errors) = value.get("errors").and_then(|e| e.as_array()) else {
            return Ok(());
        };
        if errors.is_empty() {
            return Ok(());
        }

        let messages: Vec<String> = errors
            .iter()
            .map(|e| {
                e.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown GraphQL error")
                    .to_string()
            })
            .collect();
        let joined = messages.join("; ");
        let lower = joined.to_lowercase();

        let auth_markers = [
            "not authenticated",
            "unauthenticated",
            "unauthorized",
            "not authorized",
            "permission",
            "jwt",
            "login required",
        ];
        if auth_markers.iter().any(|m| lower.contains(m)) {
            return Err(AppError::not_authenticated(
                "This action needs a Printables account. Sign in first.",
            )
            .with_detail(joined));
        }

        Err(AppError::graphql(format!("Printables API error: {joined}")))
    }

    // ---------------------------------------------------------------- account

    /// Returns the signed-in account. Requires a token.
    pub fn viewer(&self) -> AppResult<ViewerInfo> {
        if !self.has_token() {
            return Err(AppError::not_authenticated("Not signed in to Printables."));
        }
        // Verified against the live API: the account root is `me`, and the
        // public identity hangs off `me.user`.
        let query = r#"
        query Me {
          me {
            id
            email
            user {
              id
              handle
              publicUsername
            }
          }
        }
        "#;
        let payload = json!({ "operationName": "Me", "query": query, "variables": {} });
        let data = self.post_graphql(&payload)?;

        let me = data
            .pointer("/data/me")
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                AppError::not_authenticated(
                    "Printables did not return an account for this session.",
                )
            })?;

        Ok(ViewerInfo {
            id: me.get("id").and_then(|v| v.as_str()).map(String::from),
            user_id: me
                .pointer("/user/id")
                .and_then(|v| v.as_str())
                .map(String::from),
            handle: me
                .pointer("/user/handle")
                .and_then(|v| v.as_str())
                .map(String::from),
            public_username: me
                .pointer("/user/publicUsername")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }

    /// Lists models authored by a user.
    ///
    /// `userModels` is the website's own listing operation. It is public, takes
    /// a real cursor, and needs the numeric user id (see [`viewer`]):
    ///
    /// ```graphql
    /// query UserModels($id: ID!, $limit: Int, $cursor: String, $search: String) {
    ///   userModels(userId: $id, limit: $limit, cursor: $cursor, query: $search) {
    ///     cursor
    ///     items { ... }
    ///   }
    /// }
    /// ```
    pub fn user_models(
        &self,
        user_id: &str,
        limit: usize,
        cursor: Option<&str>,
        search: Option<&str>,
    ) -> AppResult<AuthorModelsPage> {
        let query = r#"
        query UserModels($id: ID!, $limit: Int!, $cursor: String, $search: String) {
          userModels(userId: $id, limit: $limit, cursor: $cursor, query: $search) {
            cursor
            items {
              id
              name
              slug
              datePublished
              downloadCount
              likesCount
              image { id filePath }
              user { id handle publicUsername }
            }
          }
        }
        "#;

        let payload = json!({
            "operationName": "UserModels",
            "query": query,
            "variables": {
                "id": user_id,
                "limit": limit,
                "cursor": cursor,
                "search": search,
            }
        });

        let data = self.post_graphql(&payload)?;
        let models = data
            .pointer("/data/userModels/items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(AuthorModelsPage {
            models,
            cursor: data
                .pointer("/data/userModels/cursor")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }

    /// Asks the API which mutations exist, so write capabilities can be
    /// discovered rather than guessed. Introspection may be disabled.
    #[allow(dead_code)]
    pub fn probe_mutation_fields(&self) -> AppResult<Vec<String>> {
        let query = r#"
        query MutationFields {
          __type(name: "Mutation") {
            fields { name }
          }
        }
        "#;
        let payload = json!({ "operationName": "MutationFields", "query": query, "variables": {} });
        let data = self.post_graphql(&payload)?;
        let fields = data
            .pointer("/data/__type/fields")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                AppError::unsupported(
                    "Printables has schema introspection disabled, so write operations cannot be discovered automatically.",
                )
            })?;
        Ok(fields
            .iter()
            .filter_map(|f| f.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect())
    }

    // ----------------------------------------------------------------- models

    /// Fetches model metadata, tags, stats, cover image, and description.
    pub fn get_model_metadata(&self, model_id: &str) -> AppResult<ModelMetadata> {
        let query = r#"
        query ModelTags($id: ID!) {
          model: print(id: $id) {
            id name slug summary description ratingAvg likesCount downloadCount datePublished
            user { publicUsername __typename }
            image { filePath }
            tags { name __typename }
            __typename
          }
        }
        "#;

        let payload = json!({
            "operationName": "ModelTags",
            "query": query,
            "variables": { "id": model_id }
        });

        let data = self.post_graphql(&payload)?;
        let model = data
            .pointer("/data/model")
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                AppError::not_found(format!("Model {model_id} was not found on Printables.com"))
            })?;

        let name = model
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(model_id)
            .to_string();
        let slug = model
            .get("slug")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let author = model
            .pointer("/user/publicUsername")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let tags = model
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let likes = model.get("likesCount").and_then(|v| v.as_u64());
        let downloads = model.get("downloadCount").and_then(|v| v.as_u64());
        let rating = match model.get("ratingAvg") {
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(s)) => s.parse::<f64>().ok(),
            _ => None,
        };
        let published_date = model
            .get("datePublished")
            .and_then(|v| v.as_str())
            .map(String::from);

        let main_image_url = model
            .pointer("/image/filePath")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|fp| format!("{MEDIA_BASE_URL}{}", fp.trim_start_matches('/')));

        let raw_desc = model
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let summary = model.get("summary").and_then(|v| v.as_str()).unwrap_or("");

        let cleaned = clean_description_html(raw_desc);
        let description = if !cleaned.is_empty() {
            cleaned
        } else if !summary.trim().is_empty() {
            summary.trim().to_string()
        } else {
            String::new()
        };

        Ok(ModelMetadata {
            id: model_id.to_string(),
            name,
            slug,
            author,
            tags,
            likes,
            downloads,
            rating,
            published_date,
            main_image_url,
            description,
            raw_description: raw_desc.to_string(),
        })
    }

    /// Lists the model's files without resolving download links.
    pub fn list_model_files(&self, model_id: &str) -> AppResult<Vec<RemoteFile>> {
        let query = r#"
        query ModelFiles($id: ID!) {
          model: print(id: $id) {
            id
            filesType
            gcodes { ...GcodeDetail __typename }
            stls { ...StlDetail __typename }
            slas { ...SlaDetail __typename }
            otherFiles { ...OtherFileDetail __typename }
            __typename
          }
        }
        fragment GcodeDetail on GCodeType { id created name folder note printer { id name __typename } excludeFromTotalSum printDuration layerHeight nozzleDiameter material { id name __typename } weight fileSize filePreviewPath rawDataPrinter order __typename }
        fragment OtherFileDetail on OtherFileType { id created name folder note fileSize filePreviewPath order __typename }
        fragment SlaDetail on SLAType { id created name folder note expTime firstExpTime printer { id name __typename } printDuration layerHeight usedMaterial fileSize filePreviewPath order __typename }
        fragment StlDetail on STLType { id created name folder note fileSize filePreviewPath order __typename }
        "#;

        let payload = json!({
            "operationName": "ModelFiles",
            "query": query,
            "variables": { "id": model_id }
        });

        let data = self.post_graphql(&payload)?;
        let Some(model_files) = data.pointer("/data/model").filter(|v| !v.is_null()) else {
            return Ok(Vec::new());
        };

        let supported = [
            ("stls", "stl"),
            ("gcodes", "gcode"),
            ("slas", "sla"),
            ("otherFiles", "other"),
        ];
        let mut results = Vec::new();

        for (list_name, api_type) in supported {
            if let Some(items) = model_files.get(list_name).and_then(|v| v.as_array()) {
                for item in items {
                    let Some(file_id) = item.get("id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let Some(file_name) = item.get("name").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    results.push(RemoteFile {
                        id: file_id.to_string(),
                        name: file_name.to_string(),
                        folder: item
                            .get("folder")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        note: item.get("note").and_then(|v| v.as_str()).map(String::from),
                        preview_path: item
                            .get("filePreviewPath")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        size_bytes: item.get("fileSize").and_then(|v| v.as_u64()),
                        file_type: api_type.to_string(),
                        download_url: None,
                    });
                }
            }
        }

        Ok(results)
    }

    /// Resolves a temporary direct download URL for one file.
    pub fn resolve_download_link(
        &self,
        file_id: &str,
        model_id: &str,
        file_type: &str,
    ) -> AppResult<Option<String>> {
        let query = r#"
        mutation GetDownloadLink($id: ID!, $modelId: ID!, $fileType: DownloadFileTypeEnum!, $source: DownloadSourceEnum!) {
          getDownloadLink(id: $id, printId: $modelId, fileType: $fileType, source: $source) {
            ok
            errors {
              ...Error
              __typename
            }
            output {
              link
              count
              ttl
              __typename
            }
            __typename
          }
        }
        fragment Error on ErrorType {
          field
          messages
          __typename
        }
        "#;

        let payload = json!({
            "operationName": "GetDownloadLink",
            "query": query,
            "variables": {
                "id": file_id,
                "modelId": model_id,
                "fileType": file_type,
                "source": "model_detail"
            }
        });

        let data = self.post_graphql(&payload)?;
        let Some(dl) = data.pointer("/data/getDownloadLink") else {
            return Ok(None);
        };
        if !dl.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(None);
        }
        Ok(dl
            .pointer("/output/link")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from))
    }

    /// Fetches all gallery images for a model.
    pub fn get_model_images(&self, model_id: &str) -> AppResult<Vec<RemoteImage>> {
        let query = r#"
        query ModelImages($id: ID!) {
          model: print(id: $id) {
            id
            images {
              id
              filePath
              __typename
            }
            __typename
          }
        }
        "#;

        let payload = json!({
            "operationName": "ModelImages",
            "query": query,
            "variables": { "id": model_id }
        });

        let data = self.post_graphql(&payload)?;
        let Some(images_arr) = data
            .pointer("/data/model/images")
            .and_then(|v| v.as_array())
        else {
            return Ok(Vec::new());
        };

        let mut images = Vec::new();
        for img in images_arr {
            if let Some(fp) = img
                .get("filePath")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                images.push(RemoteImage {
                    id: img.get("id").and_then(|v| v.as_str()).map(String::from),
                    url: format!("{MEDIA_BASE_URL}{}", fp.trim_start_matches('/')),
                    kind: "photo".to_string(),
                });
            }
        }

        Ok(images)
    }

    /// Searches printables.com for models (kept for a future browse view).
    #[allow(dead_code)]
    pub fn search_models(&self, term: &str, limit: usize, ordering: &str) -> AppResult<Vec<Value>> {
        const VALID_ORDERINGS: &[&str] =
            &["best_match", "popular", "latest", "rating", "makes_count"];
        if !VALID_ORDERINGS.contains(&ordering) {
            return Err(AppError::invalid(format!(
                "Invalid ordering '{ordering}'. Must be one of: {}",
                VALID_ORDERINGS.join(", ")
            )));
        }

        let query = r#"
        query SearchModels($query: String!, $limit: Int, $ordering: SearchChoicesEnum) {
          result: searchPrints2(query: $query, printType: print, limit: $limit, ordering: $ordering) {
            items { ...Model __typename }
          }
        }
        fragment Model on PrintType {
            id name slug ratingAvg likesCount downloadCount datePublished
            user { id handle publicUsername __typename }
            image { filePath }
            __typename
        }
        "#;

        let payload = json!({
            "operationName": "SearchModels",
            "query": query,
            "variables": { "query": term, "limit": limit, "ordering": ordering }
        });

        let data = self.post_graphql(&payload)?;
        Ok(data
            .pointer("/data/result/items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    // --------------------------------------------------------------- transfer

    /// Downloads `url` into `dest` atomically (`.part` then rename).
    pub fn download_file<F>(&self, url: &str, dest: &Path, mut on_progress: F) -> AppResult<u64>
    where
        F: FnMut(u64, u64),
    {
        let mut resp =
            self.http.get(url).send().map_err(|e| {
                AppError::network(format!("Download request failed for {url}: {e}"))
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(AppError::network(format!(
                "Download HTTP {status} for {url}"
            )));
        }

        let total = resp.content_length().unwrap_or(0);
        let part_path = dest.with_file_name(format!(
            "{}.part",
            dest.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "download".to_string())
        ));

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = File::create(&part_path)
            .map_err(|e| AppError::io(format!("Failed to create {}: {e}", part_path.display())))?;

        let mut buf = [0u8; 64 * 1024];
        let mut done: u64 = 0;
        loop {
            let n = resp
                .read(&mut buf)
                .map_err(|e| AppError::network(format!("Error reading stream from {url}: {e}")))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .map_err(|e| AppError::io(format!("Error writing {}: {e}", dest.display())))?;
            done += n as u64;
            on_progress(done, total);
        }
        file.flush()?;
        drop(file);

        std::fs::rename(&part_path, dest).map_err(|e| {
            AppError::io(format!(
                "Cannot move {} into place: {e}",
                part_path.display()
            ))
        })?;

        Ok(done)
    }
}

/// Converts Printables HTML description into the clean markdown-like text the UI renders.
pub fn clean_description_html(html: &str) -> String {
    if html.trim().is_empty() {
        return String::new();
    }

    let mut text = html.replace("\r\n", "\n");

    if let Ok(re_h) = Regex::new(r"(?is)<h[1-4][^>]*>(.*?)</h[1-4]>") {
        text = re_h
            .replace_all(&text, |caps: &regex::Captures| {
                let inner = strip_tags(&caps[1]);
                let decoded = decode_html_entities(inner.trim());
                if decoded.is_empty() {
                    String::new()
                } else {
                    format!("\n## {}\n", decoded)
                }
            })
            .into_owned();
    }

    if let Ok(re_a) = Regex::new(r#"(?is)<a\b[^>]*?\bhref\s*=\s*["']([^"']*)["'][^>]*>(.*?)</a>"#) {
        text = re_a
            .replace_all(&text, |caps: &regex::Captures| {
                let href = caps[1].trim();
                let label = decode_html_entities(strip_tags(&caps[2]).trim());
                if label.is_empty() {
                    String::new()
                } else if href.is_empty() {
                    label
                } else {
                    format!("[{label}]({href})")
                }
            })
            .into_owned();
    }

    if let Ok(re_br) = Regex::new(r"(?i)<br\s*/?>") {
        text = re_br.replace_all(&text, "\n").into_owned();
    }
    if let Ok(re_li_open) = Regex::new(r"(?i)<li[^>]*>") {
        text = re_li_open.replace_all(&text, "\n• ").into_owned();
    }
    if let Ok(re_block_close) = Regex::new(r"(?i)</(p|div|li|ul|ol|blockquote|tr)>") {
        text = re_block_close.replace_all(&text, "\n").into_owned();
    }

    let stripped = strip_tags(&text);
    let decoded = decode_html_entities(&stripped);

    let mut lines: Vec<String> = Vec::new();
    for raw_line in decoded.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines.push(trimmed.to_string());
    }

    lines.join("\n")
}

fn strip_tags(input: &str) -> String {
    if let Ok(re_tags) = Regex::new(r"(?s)<[^>]+>") {
        re_tags.replace_all(input, "").into_owned()
    } else {
        input.to_string()
    }
}

fn decode_html_entities(input: &str) -> String {
    let mut s = input
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&ndash;", "–")
        .replace("&mdash;", "—");

    if let Ok(re_num) = Regex::new(r"&#(\d+);") {
        s = re_num
            .replace_all(&s, |caps: &regex::Captures| {
                caps[1]
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            })
            .into_owned();
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_description_html_headings_links_and_entities() {
        let raw = r#"
            <div class="user-inserted">
                <h3>Print Settings</h3>
                <p>Use 0.2mm layer height&nbsp;and <a href="https://www.printables.com">Printables</a> profile.</p>
                <p>Supports: No<br/>Infill: 15 &amp; 20%</p>
            </div>
        "#;
        let cleaned = clean_description_html(raw);
        assert_eq!(
            cleaned,
            "## Print Settings\nUse 0.2mm layer height and [Printables](https://www.printables.com) profile.\nSupports: No\nInfill: 15 & 20%"
        );
    }

    #[test]
    fn test_search_models_rejects_invalid_ordering() {
        let client = PrintablesClient::anonymous().unwrap();
        let err = client
            .search_models("benchy", 5, "invalid_order")
            .unwrap_err();
        assert_eq!(err.code, "INVALID_INPUT");
    }

    #[test]
    fn graphql_errors_are_classified_not_swallowed() {
        let client = PrintablesClient::anonymous().unwrap();

        let auth_error = json!({ "errors": [{ "message": "Not authenticated" }] });
        let err = client.check_graphql_errors(&auth_error).unwrap_err();
        assert_eq!(err.code, "NOT_AUTHENTICATED");

        let other = json!({ "errors": [{ "message": "Field 'x' does not exist" }] });
        let err = client.check_graphql_errors(&other).unwrap_err();
        assert_eq!(err.code, "GRAPHQL");

        assert!(client.check_graphql_errors(&json!({ "data": {} })).is_ok());
    }

    #[test]
    fn anonymous_client_reports_missing_session() {
        let client = PrintablesClient::anonymous().unwrap();
        let err = client.viewer().unwrap_err();
        assert_eq!(err.code, "NOT_AUTHENTICATED");
    }

    #[test]
    fn part_path_sits_next_to_destination() {
        let dest = Path::new("/tmp/model/files/a.stl");
        let part = dest.with_file_name(format!(
            "{}.part",
            dest.file_name().unwrap().to_string_lossy()
        ));
        assert_eq!(part, Path::new("/tmp/model/files/a.stl.part"));
    }

    /// Requires network. Run with `PO_LIVE=1 cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn test_live_graphql_metadata_files_and_images() {
        if std::env::var("PO_LIVE").is_err() {
            return;
        }
        let client = PrintablesClient::anonymous().unwrap();
        let meta = client
            .get_model_metadata("1648230")
            .expect("metadata fetch failed");
        assert_eq!(meta.id, "1648230");
        assert!(meta.name.to_lowercase().contains("benchy"));

        let files = client
            .list_model_files("1648230")
            .expect("files fetch failed");
        assert!(!files.is_empty());
        let link = client
            .resolve_download_link(&files[0].id, "1648230", &files[0].file_type)
            .expect("download link failed");
        assert!(link.is_some());

        let images = client
            .get_model_images("1648230")
            .expect("images fetch failed");
        assert!(!images.is_empty());
    }
}
