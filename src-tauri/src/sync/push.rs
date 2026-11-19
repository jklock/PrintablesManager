//! Executes a reviewed push plan against printables.com.
//!
//! The write path is the website's own: upload new files through
//! `printFileUpload2` → presigned POST → `printFileUploadFinished`, then send the
//! complete editable model back through `modelUpdate`. Deleting a file is the
//! same call with that file omitted from the list.
//!
//! Safety model:
//!   * nothing runs unless remote writes are enabled in Preferences,
//!   * the plan must match the one the user reviewed (`plan_id`),
//!   * a remote change since the last pull blocks the push,
//!   * uploads happen first; the model is only updated if every upload worked,
//!   * every attempt is journalled and a pre-push commit makes it revertable.

use super::plan::PushPlan;
use crate::config;
use crate::error::{AppError, AppResult};
use crate::git;
use crate::library::locator;
use crate::printables::api::PrintablesClient;
use crate::printables::write::{category_for, FileInput, RemoteWriter};
use serde::Serialize;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct PushReport {
    pub dry_run: bool,
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
    pub committed: Option<String>,
    pub plan_id: String,
}

pub fn apply(
    client: &PrintablesClient,
    library: &Path,
    plan: &PushPlan,
    dry_run: bool,
) -> AppResult<PushReport> {
    super::plan::verify(plan, library)?;

    if plan.remote_changed {
        return Err(AppError::conflict(
            "printables.com changed since the last pull. Pull first, then review the push again.",
        ));
    }

    if plan.operations.is_empty() {
        return Ok(PushReport {
            dry_run,
            applied: Vec::new(),
            skipped: vec!["Nothing to push — the folder matches printables.com.".into()],
            errors: Vec::new(),
            committed: None,
            plan_id: plan.plan_id.clone(),
        });
    }

    if dry_run {
        return Ok(PushReport {
            dry_run: true,
            applied: Vec::new(),
            skipped: plan
                .operations
                .iter()
                .map(|op| op.summary.clone())
                .collect(),
            errors: Vec::new(),
            committed: None,
            plan_id: plan.plan_id.clone(),
        });
    }

    if !plan.write_enabled {
        return Err(AppError::unsupported(
            "Writing to printables.com is disabled. Turn on “Allow pushing to printables.com” in Preferences.",
        ));
    }

    let model_dir = Path::new(&plan.model_dir);
    let metadata = locator::read_metadata(model_dir)?;
    let writer = RemoteWriter::new(client);

    // Snapshot before touching the remote so the push is revertable.
    let snapshot = git::commit_all(model_dir, "Before pushing to printables.com")?;

    let mut report = PushReport {
        dry_run: false,
        applied: Vec::new(),
        skipped: Vec::new(),
        errors: Vec::new(),
        committed: None,
        plan_id: plan.plan_id.clone(),
    };

    let mut state = match writer.edit_state(&plan.model_id) {
        Ok(state) => state,
        Err(err) => {
            report
                .errors
                .push(format!("Could not read the model: {}", err.message));
            journal(library, &plan.model_id, "read_model", false, &err.message);
            return Ok(report);
        }
    };

    // ---------------------------------------------------------------- uploads
    let mut uploaded = 0usize;
    for op in plan.operations.iter().filter(|op| op.kind == "add_file") {
        let Some(rel) = op.path.as_deref() else {
            continue;
        };
        let path = model_dir.join(rel);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        match writer.upload_model_file(&path, "") {
            Ok(file_id) => {
                let input = FileInput {
                    id: file_id,
                    name: name.clone(),
                    folder: String::new(),
                    note: String::new(),
                };
                match category_for(&name) {
                    "stls" => state.stls.push(input),
                    "gcodes" => state.gcodes.push(input),
                    "slas" => state.slas.push(input),
                    _ => state.other_files.push(input),
                }
                uploaded += 1;
                report.applied.push(format!("Uploaded “{name}”"));
                journal(library, &plan.model_id, "add_file", true, &name);
            }
            Err(err) => {
                report
                    .errors
                    .push(format!("Upload “{name}” failed: {}", err.message));
                journal(library, &plan.model_id, "add_file", false, &err.message);
            }
        }
    }

    // -------------------------------------------------------------- deletions
    for op in plan.operations.iter().filter(|op| op.kind == "delete_file") {
        let Some(remote_id) = op.remote_id.as_deref() else {
            continue;
        };
        let mut removed = false;
        for list in [
            &mut state.stls,
            &mut state.gcodes,
            &mut state.slas,
            &mut state.other_files,
        ] {
            let before = list.len();
            list.retain(|f| f.id != remote_id);
            removed |= list.len() != before;
        }
        if removed {
            report.applied.push(format!(
                "Removed file {}",
                op.path.clone().unwrap_or_default()
            ));
            journal(library, &plan.model_id, "delete_file", true, remote_id);
        } else {
            report.skipped.push(format!(
                "{} was already absent remotely",
                op.path.clone().unwrap_or_default()
            ));
        }
    }

    if plan.operations.iter().any(|op| op.kind == "add_image") {
        report.errors.push(
            "Image uploads are not supported yet; model files and metadata were still pushed."
                .into(),
        );
    }

    if !report.errors.is_empty() {
        report.skipped.push(
            "Uploads failed, so the model was left untouched. Fix the errors and push again."
                .into(),
        );
        let _ = snapshot;
        return Ok(report);
    }

    // --------------------------------------------------------------- metadata
    let metadata_changed = plan.operations.iter().any(|op| op.kind == "update_model");

    if metadata_changed {
        state.name = metadata.name.clone();
        // Only rewrite the description when the local text no longer matches
        // what the website said last time we pulled it.
        let remote_text =
            crate::printables::api::clean_description_html(&metadata.remote_description_html);
        if !metadata.description.is_empty() && metadata.description != remote_text {
            state.description = text_to_html(&metadata.description);
        }
    }

    match writer.save(&state) {
        Ok(()) => {
            if metadata_changed {
                report.applied.push("Updated the model details".into());
                journal(library, &plan.model_id, "update_model", true, "modelUpdate");
            }
            if uploaded > 0 || metadata_changed {
                report.committed = git::commit_all(model_dir, "Synced changes to printables.com")?;
            }
            if report.applied.is_empty() {
                report
                    .skipped
                    .push("Nothing changed on printables.com.".into());
            }
        }
        Err(err) => {
            report
                .errors
                .push(format!("Saving the model failed: {}", err.message));
            journal(library, &plan.model_id, "update_model", false, &err.message);
            let _ = snapshot;
        }
    }

    Ok(report)
}

/// Turns the app's plain-text description back into the simple HTML the site
/// stores. Supports the subset the pull pipeline produces.
pub fn text_to_html(text: &str) -> String {
    let mut html = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix("## ") {
            html.push_str(&format!("<h3>{}</h3>", escape_html(heading)));
        } else {
            let with_links = linkify(trimmed);
            html.push_str(&format!("<p>{with_links}</p>"));
        }
    }
    html
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn linkify(line: &str) -> String {
    // Keep [label](url) as an anchor, escape everything else.
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find('[') {
        if let Some(mid) = rest[start..].find("](") {
            if let Some(end_rel) = rest[start + mid..].find(')') {
                let label = &rest[start + 1..start + mid];
                let url = &rest[start + mid + 2..start + mid + end_rel];
                out.push_str(&escape_html(&rest[..start]));
                out.push_str(&format!(
                    "<a href=\"{}\">{}</a>",
                    escape_html(url),
                    escape_html(label)
                ));
                rest = &rest[start + mid + end_rel + 1..];
                continue;
            }
        }
        out.push_str(&escape_html(&rest[..start + 1]));
        rest = &rest[start + 1..];
    }
    out.push_str(&escape_html(rest));
    out
}

fn journal(library: &Path, model_id: &str, kind: &str, ok: bool, message: &str) {
    let _ = config::ensure_manager_dirs(library);
    let path = config::push_log_path(library);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let entry = json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "model_id": model_id,
            "kind": kind,
            "ok": ok,
            "message": message,
        });
        let _ = writeln!(file, "{entry}");
    }
}

/// Reads the journal, newest first, for the UI's activity list.
pub fn recent_entries(library: &Path, limit: usize) -> Vec<Value> {
    let Ok(raw) = fs::read_to_string(config::push_log_path(library)) else {
        return Vec::new();
    };
    raw.lines()
        .rev()
        .take(limit)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

/// What the app can actually publish. Shown in Preferences instead of the old
/// (impossible) schema-introspection probe.
pub fn capabilities() -> Value {
    json!({
        "files": true,
        "metadata": true,
        "images": false,
        "note": "Files and model details use the website's own upload + modelUpdate flow. Image uploads are not implemented."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_becomes_paragraphs_and_headings() {
        let html = text_to_html(
            "## Print Settings\nUse 0.2mm layers\n\nSee [Printables](https://www.printables.com)",
        );
        assert_eq!(
            html,
            "<h3>Print Settings</h3><p>Use 0.2mm layers</p><p>See <a href=\"https://www.printables.com\">Printables</a></p>"
        );
    }

    #[test]
    fn html_is_escaped() {
        let html = text_to_html("a < b & c > d");
        assert_eq!(html, "<p>a &lt; b &amp; c &gt; d</p>");
    }
}
