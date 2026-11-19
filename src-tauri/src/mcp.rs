//! A Model Context Protocol server for the local library.
//!
//! Speaks JSON-RPC 2.0 over stdio (one JSON object per line), so an agent can
//! inspect and edit the library without the GUI:
//!
//!     {"command":"/Applications/Printables Manager.app/Contents/MacOS/printables-manager-mcp"}
//!
//! Everything here is local and offline: it reads and writes the same folders
//! the app does, and every write is committed to that model's git repository.

use crate::error::AppResult;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL_VERSION: &str = "2024-11-05";

fn tools() -> Value {
    json!([
        {
            "name": "list_models",
            "description": "List every model in the local library, with size, file count and git state.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "get_model",
            "description": "Read one model's metadata: name, description (Markdown), tags, files and image paths.",
            "inputSchema": {
                "type": "object",
                "properties": { "model_id": { "type": "string" } },
                "required": ["model_id"]
            }
        },
        {
            "name": "update_model_details",
            "description": "Rename a model, replace its Markdown description and tags. Writes metadata.json and commits.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "model_id": { "type": "string" },
                    "name": { "type": "string" },
                    "description": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["model_id", "name"]
            }
        },
        {
            "name": "list_collections",
            "description": "List collections and the local model ids they contain.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "backup_to_icloud",
            "description": "Mirror every model's git history into iCloud Drive and report the result.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }
    ])
}

fn library() -> AppResult<std::path::PathBuf> {
    let prefs = crate::config::load_prefs()?;
    crate::config::library_dir(&prefs)
}

fn call(name: &str, args: &Value) -> Result<Value, String> {
    let s = |key: &str| -> String {
        args.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };

    match name {
        "list_models" => {
            let lib = library().map_err(|e| e.message)?;
            let index = crate::library::indexer::index_library(&lib)?;
            Ok(json!({
                "library": lib.display().to_string(),
                "totals": index.totals,
                "models": index.models.iter().map(|m| json!({
                    "id": m.id,
                    "name": m.name,
                    "author": m.creator,
                    "tags": m.tags,
                    "files": m.files.len(),
                    "size_mb": m.size_mb,
                    "sync_state": m.sync_state,
                    "dir": m.dir,
                })).collect::<Vec<_>>()
            }))
        }
        "get_model" => {
            let lib = library().map_err(|e| e.message)?;
            let dir = crate::library::locator::find_model_dir(&lib, &s("model_id"))
                .map_err(|e| e.message)?;
            let meta = crate::library::locator::read_metadata(&dir).map_err(|e| e.message)?;
            Ok(serde_json::to_value(meta).map_err(|e| e.to_string())?)
        }
        "update_model_details" => {
            let lib = library().map_err(|e| e.message)?;
            let dir = crate::library::locator::find_model_dir(&lib, &s("model_id"))
                .map_err(|e| e.message)?;
            let tags = args
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let updated =
                crate::commands::edit::apply_details(&dir, &s("name"), &s("description"), tags)
                    .map_err(|e| e.message)?;
            Ok(serde_json::to_value(updated).map_err(|e| e.to_string())?)
        }
        "list_collections" => {
            let lib = library().map_err(|e| e.message)?;
            let raw = std::fs::read_to_string(lib.join("collections.json")).unwrap_or_default();
            let parsed: Value = serde_json::from_str(&raw).unwrap_or(json!({"collections": []}));
            Ok(parsed)
        }
        "backup_to_icloud" => {
            let lib = library().map_err(|e| e.message)?;
            let root = crate::backup::icloud_root().map_err(|e| e.message)?;
            let summary = crate::backup::backup_to(&lib, &root).map_err(|e| e.message)?;
            Ok(serde_json::to_value(summary).map_err(|e| e.to_string())?)
        }
        other => Err(format!("Unknown tool: {other}")),
    }
}

fn handle(request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default();

    // Notifications carry no id and must not be answered.
    let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let error = |code: i64, message: String| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });

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
        other => {
            msg_id_is_missing(&id).then(|| error(-32601, format!("Method not found: {other}")))
        }
    }
}

fn msg_id_is_missing(id: &Option<Value>) -> bool {
    !matches!(id, None | Some(Value::Null))
}

/// Entry point used by the `printables-manager-mcp` binary.
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

    #[test]
    fn initialize_reports_tools_capability() {
        let response = handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap();
        assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(response["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_exposes_the_documented_tools() {
        let response = handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
        let names: Vec<&str> = response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in [
            "list_models",
            "get_model",
            "update_model_details",
            "list_collections",
            "backup_to_icloud",
        ] {
            assert!(names.contains(&expected), "missing {expected} in {names:?}");
        }
    }

    #[test]
    fn notifications_get_no_reply() {
        assert!(handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
    }

    #[test]
    fn unknown_methods_are_an_error_and_unknown_tools_report_is_error() {
        let err = handle(&json!({"jsonrpc":"2.0","id":3,"method":"nope"})).unwrap();
        assert_eq!(err["error"]["code"], -32601);

        let tool = handle(&json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"does_not_exist","arguments":{}}
        }))
        .unwrap();
        assert_eq!(tool["result"]["isError"], true);
    }
}
