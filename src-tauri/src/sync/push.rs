//! Executes a reviewed push plan against printables.com.
//!
//! Safety model:
//!   * nothing runs unless the user enabled remote writes in Settings,
//!   * the plan must match the one the user reviewed (`plan_id`),
//!   * a remote change since the last pull blocks the push,
//!   * every operation is journalled to `<library>/.manager/push-log.jsonl`,
//!   * a pre-push git commit makes the result revertable.
//!
//! Mutations are taken from discovered capabilities rather than hard-coded
//! names. If a role cannot be resolved, the push refuses to run and reports
//! exactly which capability is missing.

use super::capabilities::{self, Capabilities};
use super::plan::{PushOperation, PushPlan};
use crate::config;
use crate::error::{AppError, AppResult};
use crate::git;
use crate::library::locator;
use crate::printables::api::PrintablesClient;
use serde::Serialize;
use serde_json::{json, Map, Value};
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

/// Executes `plan`. With `dry_run` (the default in the UI) nothing is sent.
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
            "Writing to printables.com is disabled. Enable it in Settings once you are ready to publish changes.",
        ));
    }

    let caps = capabilities::load(library).ok_or_else(|| {
        AppError::unsupported(
            "The write API has not been probed yet. Run “Probe write API” in Settings while signed in.",
        )
    })?;

    let missing: Vec<&str> = plan
        .operations
        .iter()
        .map(|op| role_for_kind(&op.kind))
        .filter(|role| caps.role(role).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(AppError::unsupported(format!(
            "Printables does not expose the operations needed for this push ({}). Capability report: {}",
            missing.join(", "),
            caps.summary()
        )));
    }

    // Snapshot before touching the remote so the push is revertable.
    let model_dir = Path::new(&plan.model_dir);
    let snapshot = git::commit_all(model_dir, "Before pushing to printables.com")?;

    let mut report = PushReport {
        dry_run: false,
        applied: Vec::new(),
        skipped: Vec::new(),
        errors: Vec::new(),
        committed: None,
        plan_id: plan.plan_id.clone(),
    };

    for op in &plan.operations {
        match execute_operation(client, &caps, model_dir, &plan.model_id, op) {
            Ok(message) => {
                report.applied.push(message.clone());
                journal(library, &plan.model_id, op, true, &message);
            }
            Err(err) => {
                report
                    .errors
                    .push(format!("{}: {}", op.summary, err.message));
                journal(library, &plan.model_id, op, false, &err.message);
            }
        }
    }

    if report.errors.is_empty() {
        report.committed = git::commit_all(model_dir, "Synced changes to printables.com")?;
    } else {
        report.skipped.push(
            "Some operations failed; the working tree was left uncommitted so you can retry."
                .into(),
        );
        // Keep the pre-push snapshot meaningful even on partial failure.
        let _ = snapshot;
    }

    Ok(report)
}

fn role_for_kind(kind: &str) -> &'static str {
    match kind {
        "add_file" => "add_file",
        "delete_file" => "delete_file",
        "update_model" => "update_model",
        "add_image" => "add_image",
        _ => "update_model",
    }
}

fn execute_operation(
    client: &PrintablesClient,
    caps: &Capabilities,
    model_dir: &Path,
    model_id: &str,
    op: &PushOperation,
) -> AppResult<String> {
    let role = role_for_kind(&op.kind);
    let field_name = caps
        .role(role)
        .ok_or_else(|| AppError::unsupported(format!("No mutation discovered for {role}")))?;
    let field = caps.field(field_name).ok_or_else(|| {
        AppError::unsupported(format!(
            "Mutation {field_name} is not in the capability report"
        ))
    })?;

    let args = build_args(caps, model_dir, model_id, op)?;
    run_mutation(client, field, &args)?;
    Ok(format!("{} ({field_name})", op.summary))
}

/// Maps our operation data onto the discovered argument names.
fn build_args(
    caps: &Capabilities,
    model_dir: &Path,
    model_id: &str,
    op: &PushOperation,
) -> AppResult<Map<String, Value>> {
    let role = role_for_kind(&op.kind);
    let field_name = caps.role(role).unwrap_or_default();
    let field = caps
        .field(field_name)
        .ok_or_else(|| AppError::unsupported("Mutation metadata missing"))?;

    let meta = locator::read_metadata(model_dir)?;
    let mut args = Map::new();

    for arg in &field.args {
        let key = arg.name.to_lowercase();
        let value = if key.contains("printid") || key.contains("modelid") {
            json!(model_id)
        } else if key.contains("id") {
            match op.remote_id.as_ref() {
                Some(id) => json!(id),
                None => json!(model_id),
            }
        } else if key.contains("name") {
            json!(op
                .path
                .as_ref()
                .and_then(|p| Path::new(p).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| meta.name.clone()))
        } else if key.contains("description") || key.contains("summary") {
            json!(meta.description)
        } else if key.contains("tags") {
            json!(meta.tags)
        } else if key.contains("folder") {
            json!("")
        } else {
            return Err(AppError::unsupported(format!(
                "Cannot supply argument “{}: {}” for {field_name} without API documentation.",
                arg.name, arg.type_ref
            )));
        };
        args.insert(arg.name.clone(), value);
    }

    Ok(args)
}

/// Builds and sends a mutation using the introspected argument types.
fn run_mutation(
    client: &PrintablesClient,
    field: &capabilities::MutationField,
    args: &Map<String, Value>,
) -> AppResult<Value> {
    let mut var_decls = Vec::new();
    let mut call_args = Vec::new();
    for arg in &field.args {
        let Some(value) = args.get(&arg.name) else {
            return Err(AppError::unsupported(format!(
                "Missing argument {} for {}",
                arg.name, field.name
            )));
        };
        var_decls.push(format!("${}: {}", arg.name, arg.type_ref));
        call_args.push(format!("{}: ${}", arg.name, arg.name));
        let _ = value;
    }

    let selection = if field.returns_object {
        " { __typename }"
    } else {
        ""
    };
    let document = format!(
        "mutation POMutation({}) {{ {}({}){} }}",
        var_decls.join(", "),
        field.name,
        call_args.join(", "),
        selection
    );

    let mut variables = Map::new();
    for arg in &field.args {
        if let Some(value) = args.get(&arg.name) {
            variables.insert(arg.name.clone(), value.clone());
        }
    }

    client.execute_graphql(&json!({
        "operationName": "POMutation",
        "query": document,
        "variables": Value::Object(variables),
    }))
}

fn journal(library: &Path, model_id: &str, op: &PushOperation, ok: bool, message: &str) {
    let _ = config::ensure_manager_dirs(library);
    let path = config::push_log_path(library);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let entry = json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "model_id": model_id,
            "kind": op.kind,
            "path": op.path,
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
