//! Synchronous MCP server: one JSON-RPC message per line, no logs on stdout.
use std::io::{self, BufRead, Write};
use std::path::{Component, Path};

use serde_json::{json, Value};

use crate::{config::Config, context, explain, live, map, recall, store::Store, write};

const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const VERSIONS: &[&str] = &[
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];
const INSTRUCTIONS: &str = "Use elo_recall to search memories before working; elo_context to read a scope or topic; elo_add to record notes and decisions as an agent; elo_explain to understand an item; elo_map to query the previously indexed code map.";

/// Processes until EOF; empty lines are ignored and each response is flushed.
pub fn serve(root: &Path, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(message) => dispatch(root, message),
            Err(_) => Some(error(Value::Null, -32700, "Invalid JSON")),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message}})
}

fn dispatch(root: &Path, message: Value) -> Option<Value> {
    let Some(object) = message.as_object() else {
        return Some(error(Value::Null, -32600, "Invalid request"));
    };
    // Notifications never run tools nor produce responses.
    if !object.contains_key("id") {
        return None;
    }
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    if !(id.is_null() || id.is_string() || id.is_number()) {
        return Some(error(Value::Null, -32600, "Invalid id"));
    }
    if message["jsonrpc"] != "2.0" || !message["method"].is_string() {
        return Some(error(id, -32600, "Invalid request"));
    }
    let empty = json!({});
    let params = object.get("params").unwrap_or(&empty);
    if !params.is_object() {
        return Some(error(id, -32602, "params must be an object"));
    }
    let method = message["method"].as_str().unwrap();
    // initialize keeps the legacy negotiation; modern metadata is valid per request.
    let mut modern = method == "server/discover";
    if method != "initialize" {
        if let Some(meta) = params.get("_meta") {
            if !meta.is_object() {
                return Some(error(id, -32602, "_meta must be an object"));
            }
            if let Some(version) = meta.get("io.modelcontextprotocol/protocolVersion") {
                let Some(version) = version.as_str() else {
                    return Some(error(id, -32602, "protocolVersion must be a string"));
                };
                if !VERSIONS.contains(&version) {
                    let mut response = error(id, -32022, "Unsupported protocol version");
                    response["error"]["data"] = json!({"supported":VERSIONS});
                    return Some(response);
                }
                modern |= version == VERSIONS[0];
            }
        }
    }
    let mut result = match method {
        "server/discover" => json!({
            "supportedVersions": VERSIONS,
            "capabilities": {"tools":{}},
            "instructions": INSTRUCTIONS
        }),
        "initialize" => {
            let Some(version) = params["protocolVersion"].as_str() else {
                return Some(error(id, -32602, "protocolVersion must be a string"));
            };
            json!({
                "protocolVersion": if LEGACY_VERSIONS.contains(&version) { version } else { LEGACY_VERSIONS[0] },
                "capabilities": {"tools":{}},
                "serverInfo": {"name":"elo", "version":env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tool_definitions()}),
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return Some(error(id, -32602, "name must be a string"));
            };
            let args = params.get("arguments").unwrap_or(&empty);
            if !args.is_object() {
                return Some(error(id, -32602, "arguments must be an object"));
            }
            let definition = tool_definitions()
                .into_iter()
                .find(|tool| tool["name"] == name);
            let Some(definition) = definition else {
                return Some(error(id, -32602, "Unknown tool"));
            };
            if let Err(message) = validate_arguments(args, &definition["inputSchema"]) {
                return Some(error(id, -32602, &message));
            }
            match call_tool(root, name, args) {
                Ok(text) => json!({"content":[{"type":"text","text":text}],"isError":false}),
                Err(err) => {
                    json!({"content":[{"type":"text","text":err.to_string()}],"isError":true})
                }
            }
        }
        _ => return Some(error(id, -32601, "Unknown method")),
    };
    if modern {
        result["resultType"] = json!("complete");
        result["_meta"] = json!({
            "io.modelcontextprotocol/serverInfo": {
                "name":"elo", "version":env!("CARGO_PKG_VERSION")
            }
        });
    }
    Some(json!({"jsonrpc":"2.0", "id":id, "result":result}))
}

fn tool_definitions() -> Vec<Value> {
    let string = json!({"type":"string"});
    let strings = json!({"type":"array", "items":{"type":"string"}});
    vec![
        tool("elo_recall", "Search memories by text.", json!({"query":string,"limit":{"type":"integer","minimum":0,"default":10}}), &["query"]),
        tool("elo_context", "Read the context of a scope or topic.", json!({"target":string}), &["target"]),
        tool("elo_add", "Record an agent memory; laws require user authorship and are rejected. The text becomes both title and body.", json!({
            "text":string,
            "kind":{"type":"string","enum":crate::import::KINDS.iter().filter(|kind| **kind != "law").collect::<Vec<_>>(),"default":"note"},
            "scope":{"type":"string","default":"global"},
            "keywords":strings,"paths":strings
        }), &["text"]),
        tool("elo_explain", "Explain an item by id.", json!({"id":string}), &["id"]),
        tool("elo_map", "Query the existing map; run elo map sync to index. Impact uses depth 3. For file and imports, value must be a path relative to the workspace root, without parent components or symlinks escaping the root.", json!({
            "mode":{"type":"string","enum":["file","contains","calls","imports","impact"]},"value":string
        }), &["mode","value"]),
        tool("elo_feedback", "Report which injected items were useful or ignored. Only ids actually delivered in this session are accepted.", json!({
            "session_id":{"type":"string"},
            "used":{"type":"array","items":{"type":"string"}},
            "ignored":{"type":"array","items":{"type":"string"}}
        }), &["session_id"]),
        tool("elo_maintain_catalog", "List up to 30 active memory items for maintenance, with cursor pagination.", json!({
            "cursor":{"type":"integer","default":0}
        }), &[]),
        tool("elo_maintain_propose", "Submit maintenance operations to be reviewed before applying. Operations JSON array: {op: 'set_paths'|'drop_always'|'set_scope'|'merge', ...}.", json!({
            "operations":{"type":"string"}
        }), &["operations"]),
    ]
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"name":name,"description":description,"inputSchema":{
        "type":"object","properties":properties,"required":required,"additionalProperties":false
    }})
}

// Validates the announced types before any storage access.
fn validate_arguments(args: &Value, schema: &Value) -> Result<(), String> {
    for required in schema["required"].as_array().unwrap() {
        let key = required.as_str().unwrap();
        if args.get(key).is_none() {
            return Err(format!("Required argument: {key}"));
        }
    }
    for (key, value) in args.as_object().unwrap() {
        let property = &schema["properties"][key];
        let valid = match property["type"].as_str() {
            Some("string") => value.is_string(),
            Some("integer") => value.as_u64().is_some_and(|n| usize::try_from(n).is_ok()),
            Some("array") => value
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string)),
            _ => false,
        };
        if !valid
            || property["enum"]
                .as_array()
                .is_some_and(|options| !options.contains(value))
        {
            return Err(format!("Invalid argument: {key}"));
        }
    }
    Ok(())
}

fn config(root: &Path) -> Result<Config, crate::config::ConfigError> {
    if root.join("elo.toml").is_file() {
        Config::load(root)
    } else {
        Ok(Config::default())
    }
}

fn call_tool(root: &Path, name: &str, args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let text = |key: &str| args[key].as_str().unwrap_or_default();
    match name {
        "elo_recall" => {
            let store = Store::open_read_only(root)?;
            let hits = recall::recall_now(
                &store,
                &recall::Query {
                    text: text("query").into(),
                    history: false,
                    scope: None,
                    kind: None,
                    limit: args["limit"].as_u64().unwrap_or(10) as usize,
                },
            )?;
            Ok(recall::render_text(&hits, false))
        }
        "elo_context" => {
            let config = config(root)?;
            let store = context::open(root)?;
            let target = text("target");
            let outcome = context::collect(
                &store,
                &config,
                &context::Request {
                    target,
                    words: &[],
                    ids: None,
                    budget: config.workspace.budget_bytes,
                },
            )?;
            Ok(context::render_text(&outcome, target))
        }
        "elo_add" => {
            let strings = |key: &str| -> Vec<String> {
                args[key]
                    .as_array()
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let entry = write::NewEntry {
                kind: args["kind"].as_str().unwrap_or("note").into(),
                scope: args["scope"].as_str().unwrap_or("global").into(),
                title: text("text").into(),
                body: text("text").into(),
                triggers: strings("keywords"),
                importance: 0.5,
                supersedes: None,
                expires_at: None,
                id: None,
                as_user: false,
            };
            let mut store = Store::open(root)?;
            let paths = strings("paths");
            Ok(if paths.is_empty() {
                write::add(&mut store, &entry)?
            } else {
                write::add_with_paths(&mut store, &entry, &paths)?
            })
        }
        "elo_explain" => {
            let store = context::open(root)?;
            let item = live::load_item(&store, text("id"))?;
            Ok(explain::item_report(root, &config(root)?, &item))
        }
        "elo_map" => {
            let value = text("value");
            if matches!(text("mode"), "file" | "imports") {
                validate_map_path(root, value)?;
            }
            let mut out = Vec::new();
            match text("mode") {
                "file" => map::query::print(&mut out, &map::query_file(root, value)?, false)?,
                "contains" => {
                    map::query::print(&mut out, &map::query_contains(root, value)?, false)?
                }
                "calls" => map::refs::print(&mut out, &map::query_calls(root, value)?, false)?,
                "imports" => map::refs::print(&mut out, &map::query_imports(root, value)?, false)?,
                "impact" => map::refs::print(&mut out, &map::query_impact(root, value, 3)?, false)?,
                _ => unreachable!("mode validated by schema"),
            }
            Ok(String::from_utf8(out)?)
        }
        "elo_feedback" => {
            // MCP does not authenticate the session_id; the worst case of a forged
            // session_id is recording shadow-mode feedback for another session's
            // delivered items, which never influences item selection.
            let session_id = text("session_id");
            let extract_strings = |key: &str| -> Vec<String> {
                args[key]
                    .as_array()
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let used = extract_strings("used");
            let ignored = extract_strings("ignored");
            let store = Store::open(root)?;
            let (accepted, rejected) = if !session_id.is_empty() {
                store.record_agent_feedback(session_id, &used, &ignored, 40)?
            } else {
                let mut rej = used;
                rej.extend(ignored);
                (Vec::new(), rej)
            };
            Ok(format!(
                "accepted: {}; rejected: {}",
                accepted.join(", "),
                rejected.join(", ")
            ))
        }
        "elo_maintain_catalog" => {
            let store = Store::open_read_only(root)?;
            let cursor = args["cursor"].as_i64().unwrap_or(0);
            Ok(crate::maintain::catalog(&store, cursor)?)
        }
        "elo_maintain_propose" => {
            let operations = text("operations");
            let parsed: Vec<crate::maintain::ProposalOp> =
                serde_json::from_str(operations).map_err(|e| e.to_string())?;
            // Proposals are stored even before they are applied: this needs a write.
            let store = Store::open(root)?;
            let (accepted, rejected) = crate::maintain::propose(root, &store, parsed)?;
            let mut lines = vec![format!("accepted: {}", accepted.join(", "))];
            if !rejected.is_empty() {
                lines.push("rejected:".to_string());
                for (id, reason) in rejected {
                    lines.push(format!("  {id}: {reason}"));
                }
            }
            Ok(lines.join("\n"))
        }
        _ => Err(format!("Unknown tool: {name}").into()),
    }
}

// Check every existing prefix, including symlinks, while allowing deleted indexed files.
fn validate_map_path(root: &Path, value: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(
            "Invalid map path: use a path relative to the workspace root without '..' components"
                .into(),
        );
    }
    let root = root.canonicalize()?;
    let mut candidate = root.clone();
    for component in path.components() {
        candidate.push(component);
        match candidate.canonicalize() {
            Ok(resolved) if !resolved.starts_with(&root) => {
                return Err("Invalid map path: resolved path escapes the workspace root".into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Fail closed for dangling symlinks; a missing ordinary file is valid.
                if candidate
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_symlink())
                {
                    return Err("Invalid map path: cannot resolve symbolic link".into());
                }
            }
            Err(error) => return Err(format!("Invalid map path: {error}").into()),
        }
    }
    Ok(())
}
