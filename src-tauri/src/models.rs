use serde::{Deserialize, Deserializer, Serialize};

/// Shape-compatible with the prototype's mock MODELS array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSummary {
    pub id: String,
    pub name: String,
    pub creator: String,
    pub tags: Vec<String>,
    pub added: String,       // ISO date string
    pub source: String,      // original printables.com URL
    pub description: String, // model description (may contain error text if fetch failed)
    pub files: Vec<FileEntry>,
    pub kinds: Vec<String>, // deduplicated file extensions
    pub size_mb: f64,
    pub cover_asset_url: Option<String>, // asset://localhost/… path or None
    pub images: Vec<ImageAsset>,         // all downloaded images with asset URLs
    /// Absolute path of this model's directory. The UI must never re-derive
    /// this from `source` — the on-disk slug can differ from the URL slug.
    #[serde(default)]
    pub dir: String,
    /// True when this folder is a git repository.
    #[serde(default)]
    pub has_git: bool,
    /// True when the working tree differs from HEAD (uncommitted local edits).
    #[serde(default)]
    pub dirty: bool,
    /// "clean" | "dirty" | "no-git" — convenience for the UI badge.
    #[serde(default)]
    pub sync_state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAsset {
    pub url: String,  // asset://localhost/… path
    pub kind: String, // "cover" | "photo" | "render" | etc.
    pub name: String, // filename
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub n: String, // filename (matches prototype convention)
    pub mb: f64,
    pub modified: String,
}

/// Full metadata.json schema v1 — parsed from disk during indexing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataV1 {
    pub schema_version: u32,
    pub source: String,
    pub model_id: String,
    pub slug: String,
    pub name: String,
    pub url: String,
    pub author: String,
    pub tags: Vec<String>,
    pub stats: ModelStats,
    pub description: String,
    /// Raw HTML description exactly as printables.com served it. Used to tell a
    /// real local edit (which should be pushed) from an untouched pull.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub remote_description_html: String,
    pub cloned_at: String,
    pub files: Vec<MetadataFile>,
    pub images: Vec<MetadataImage>,
}

/// Custom deserializer for rating that accepts both string and number formats.
fn deserialize_rating<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    let opt: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_f64()
            .map(Some)
            .ok_or_else(|| Error::custom("expected number")),
        Some(serde_json::Value::String(s)) => s
            .parse::<f64>()
            .map(Some)
            .map_err(|e| Error::custom(e.to_string())),
        _ => Err(Error::custom("expected string or number")),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStats {
    pub likes: Option<u64>,
    pub downloads: Option<u64>,
    #[serde(deserialize_with = "deserialize_rating")]
    pub rating: Option<f64>,
    pub published_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataFile {
    pub name: String,
    pub kind: String,
    pub size_bytes: u64,
    pub local_path: String, // relative to model dir: "files/x.3mf"
    /// Remote file id on printables.com. `None` means "local-only, upload it".
    /// Deleting this value is the supported way to re-upload a file as new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataImage {
    pub name: String,
    pub kind: String,       // "cover" | "photo"
    pub local_path: String, // relative to model dir: "images/cover.jpg"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
}

/// Aggregated library index returned by `list_models`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryIndex {
    pub models: Vec<ModelSummary>,
    pub totals: LibraryTotals,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryTotals {
    pub models: usize,
    pub files: usize,
    pub bytes_used: u64,
    pub bytes_capacity: u64, // 4 GB default; configurable via prefs
}

/// One user-defined grouping of models. Stored independently of metadata.json
/// so existing model metadata never needs to change shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub model_ids: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    /// printables.com collection id, when this mirrors a site collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_id: Option<String>,
    /// How many models the site says the collection holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_count: Option<u32>,
}

/// Root shape of `<library_folder>/collections.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionsFile {
    #[serde(default = "default_collections_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub collections: Vec<Collection>,
}

fn default_collections_schema_version() -> u32 {
    1
}

impl Default for CollectionsFile {
    fn default() -> Self {
        Self {
            schema_version: 1,
            collections: Vec::new(),
        }
    }
}

/// Preferences, stored as JSON at
/// `~/Library/Application Support/dev.printablesmanager/prefs.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppPrefs {
    pub theme: String,      // "light" | "dark" | "system"
    pub slicer_key: String, // "prusa" | "orca" | "bambu" | "cura"
    pub slicer_executable: Option<String>,
    pub library_folder: Option<String>,

    // ---- 2.0 fields (all defaulted so 1.x prefs keep loading) ----
    /// Create and maintain a git repository inside every model folder.
    #[serde(default = "default_true")]
    pub git_enabled: bool,
    /// Commit local edits automatically when the folder changes.
    #[serde(default = "default_true")]
    pub auto_commit_local_edits: bool,
    /// Number of models downloaded in parallel by "Sync all".
    #[serde(default = "default_sync_concurrency")]
    pub sync_concurrency: usize,
    /// Pull the account library automatically on launch (only when signed in).
    #[serde(default)]
    pub auto_sync_on_launch: bool,
    /// Keep the session in the login Keychain. Off by default: a Keychain item
    /// is ACL'd to the build that created it, so every ad-hoc rebuild looks like
    /// a new app and macOS asks for permission again. A 0600 file never asks.
    #[serde(default)]
    pub store_in_keychain: bool,
    /// View the app was last showing, so it reopens where you left off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_view: Option<String>,
    /// Master switch for writing to printables.com. Off by default: uploads
    /// mutate the user's real account and the write API is not documented.
    #[serde(default)]
    pub remote_write_enabled: bool,
    /// Last successful full sync (ISO-8601), for the UI.
    #[serde(default)]
    pub last_sync_at: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_sync_concurrency() -> usize {
    3
}

impl Default for AppPrefs {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            slicer_key: "prusa".into(),
            slicer_executable: None,
            library_folder: None,
            git_enabled: true,
            auto_commit_local_edits: true,
            sync_concurrency: 3,
            auto_sync_on_launch: false,
            store_in_keychain: false,
            last_view: None,
            remote_write_enabled: false,
            last_sync_at: None,
        }
    }
}
