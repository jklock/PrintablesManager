//! Discovered write capabilities of the Printables GraphQL API.
//!
//! Printables publishes no write API documentation. Rather than guessing
//! mutation names (and silently failing), the app introspects the schema and
//! records which mutations exist and what arguments they take. Pushes are only
//! attempted for operations that were actually discovered.

use crate::config;
use crate::error::{AppError, AppResult};
use crate::printables::api::PrintablesClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationArg {
    pub name: String,
    /// Rendered GraphQL type, e.g. `ID!` or `[Upload!]!`.
    pub type_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationField {
    pub name: String,
    pub args: Vec<MutationArg>,
    /// True when the field returns an object, so a selection set is required.
    pub returns_object: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub discovered_at: String,
    pub fields: Vec<MutationField>,
    /// role -> discovered mutation name (None when nothing matched).
    pub roles: HashMap<String, Option<String>>,
}

impl Capabilities {
    pub fn role(&self, role: &str) -> Option<&str> {
        self.roles.get(role).and_then(|v| v.as_deref())
    }

    pub fn field(&self, name: &str) -> Option<&MutationField> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Human-readable summary used in the UI and in error details.
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        for role in ["add_file", "delete_file", "update_model", "add_image"] {
            match self.role(role) {
                Some(name) => lines.push(format!("{role}: {name}")),
                None => lines.push(format!("{role}: not available")),
            }
        }
        lines.join("; ")
    }
}

fn render_type(t: &Value) -> String {
    match t.get("kind").and_then(|k| k.as_str()) {
        Some("NON_NULL") => format!("{}!", render_type(&t["ofType"])),
        Some("LIST") => format!("[{}]", render_type(&t["ofType"])),
        _ => t
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("String")
            .to_string(),
    }
}

fn returns_object(t: &Value) -> bool {
    let kind = t.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    if kind == "NON_NULL" {
        return returns_object(&t["ofType"]);
    }
    matches!(kind, "OBJECT" | "INTERFACE" | "UNION")
}

/// Introspect the schema and persist the result next to the library.
pub fn probe(client: &PrintablesClient, library: &Path) -> AppResult<Capabilities> {
    if !client.has_token() {
        return Err(AppError::not_authenticated(
            "Sign in before probing the write API.",
        ));
    }

    let query = r#"
    query MutationFields {
      __type(name: "Mutation") {
        fields {
          name
          args {
            name
            type { kind name ofType { kind name ofType { kind name } } }
          }
          type { kind name ofType { kind name } }
        }
      }
    }
    "#;
    let payload = json!({ "operationName": "MutationFields", "query": query, "variables": {} });
    let data = client.execute_graphql(&payload)?;

    let fields_json = data
        .pointer("/data/__type/fields")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            AppError::unsupported(
                "Printables has schema introspection disabled, so write capabilities cannot be discovered.",
            )
        })?;

    let mut fields = Vec::new();
    for field in fields_json {
        let Some(name) = field.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let args = field
            .get("args")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|arg| {
                        Some(MutationArg {
                            name: arg.get("name")?.as_str()?.to_string(),
                            type_ref: render_type(arg.get("type")?),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        fields.push(MutationField {
            name: name.to_string(),
            args,
            returns_object: field.get("type").map(returns_object).unwrap_or(false),
        });
    }

    let caps = Capabilities {
        discovered_at: chrono::Utc::now().to_rfc3339(),
        roles: resolve_roles(&fields),
        fields,
    };

    config::ensure_manager_dirs(library)?;
    let path = config::capabilities_path(library);
    fs::write(&path, serde_json::to_string_pretty(&caps)?)
        .map_err(|e| AppError::io(format!("Cannot write {}: {e}", path.display())))?;

    Ok(caps)
}

pub fn load(library: &Path) -> Option<Capabilities> {
    let raw = fs::read_to_string(config::capabilities_path(library)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Heuristic mapping from our operations to discovered mutation names.
fn resolve_roles(fields: &[MutationField]) -> HashMap<String, Option<String>> {
    let mut roles = HashMap::new();
    roles.insert(
        "add_file".to_string(),
        pick(fields, &["file"], &["add", "create", "upload", "attach"]),
    );
    roles.insert(
        "delete_file".to_string(),
        pick(fields, &["file"], &["delete", "remove"]),
    );
    roles.insert(
        "update_model".to_string(),
        pick(fields, &["print"], &["update", "edit", "patch"]),
    );
    roles.insert(
        "add_image".to_string(),
        pick(fields, &["image"], &["add", "create", "upload"]),
    );
    roles
}

fn pick(fields: &[MutationField], required: &[&str], verbs: &[&str]) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for field in fields {
        let name = field.name.to_lowercase();
        if !required.iter().any(|k| name.contains(k)) {
            continue;
        }
        if !verbs.iter().any(|v| name.contains(v)) {
            continue;
        }
        // Prefer names that also mention the model/print context.
        let score = if name.contains("print") || name.contains("model") {
            2
        } else {
            1
        };
        if best.map(|(s, _)| score > s).unwrap_or(true) {
            best = Some((score, field.name.as_str()));
        }
    }
    best.map(|(_, name)| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str) -> MutationField {
        MutationField {
            name: name.to_string(),
            args: Vec::new(),
            returns_object: true,
        }
    }

    #[test]
    fn roles_are_resolved_from_discovered_names() {
        let fields = vec![
            field("addPrintFile"),
            field("deletePrintFile"),
            field("updatePrint"),
            field("uploadPrintImage"),
            field("likePrint"),
        ];
        let roles = resolve_roles(&fields);
        assert_eq!(roles["add_file"].as_deref(), Some("addPrintFile"));
        assert_eq!(roles["delete_file"].as_deref(), Some("deletePrintFile"));
        assert_eq!(roles["update_model"].as_deref(), Some("updatePrint"));
        assert_eq!(roles["add_image"].as_deref(), Some("uploadPrintImage"));
    }

    #[test]
    fn missing_roles_stay_none() {
        let roles = resolve_roles(&[field("likePrint")]);
        assert!(roles["add_file"].is_none());
        assert!(roles["update_model"].is_none());
    }

    #[test]
    fn type_rendering_unwraps_nested_types() {
        let t = json!({
            "kind": "NON_NULL",
            "name": null,
            "ofType": { "kind": "LIST", "name": null, "ofType": { "kind": "SCALAR", "name": "ID" } }
        });
        assert_eq!(render_type(&t), "[ID]!");
        assert!(!returns_object(&t));
    }
}
