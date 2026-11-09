//! Pure translations between agent hook protocols and ELO's internal events.
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Claude,
    Codex,
    Gemini,
    Cursor,
    Copilot,
}

pub fn event(format: Format, name: &str) -> Option<&'static str> {
    match name {
        "user-prompt-submit" | "session-start" | "pre-tool-use" | "post-tool-use" => match name {
            "user-prompt-submit" => Some("user-prompt-submit"),
            "session-start" => Some("session-start"),
            "pre-tool-use" => Some("pre-tool-use"),
            _ => Some("post-tool-use"),
        },
        "UserPromptSubmit" if matches!(format, Format::Claude | Format::Codex) => {
            Some("user-prompt-submit")
        }
        "SessionStart" if matches!(format, Format::Claude | Format::Codex | Format::Gemini) => {
            Some("session-start")
        }
        "PreToolUse" if matches!(format, Format::Claude | Format::Codex) => Some("pre-tool-use"),
        "PostToolUse" if matches!(format, Format::Claude | Format::Codex) => Some("post-tool-use"),
        "BeforeAgent" if format == Format::Gemini => Some("user-prompt-submit"),
        "BeforeTool" if format == Format::Gemini => Some("pre-tool-use"),
        "AfterTool" if format == Format::Gemini => Some("post-tool-use"),
        "sessionStart" if matches!(format, Format::Cursor | Format::Copilot) => {
            Some("session-start")
        }
        "beforeSubmitPrompt" if format == Format::Cursor => Some("user-prompt-submit"),
        "userPromptSubmitted" if format == Format::Copilot => Some("user-prompt-submit"),
        "preToolUse" if matches!(format, Format::Cursor | Format::Copilot) => Some("pre-tool-use"),
        "postToolUse" if matches!(format, Format::Cursor | Format::Copilot) => {
            Some("post-tool-use")
        }
        "afterFileEdit" if format == Format::Cursor => Some("file-changed"),
        "file-changed" | "FileChanged" if matches!(format, Format::Claude | Format::Cursor) => {
            Some("file-changed")
        }
        "cwd-changed" | "CwdChanged" if format == Format::Claude => Some("cwd-changed"),
        _ => None,
    }
}

pub fn external_event(format: Format, event: &str) -> &'static str {
    match (format, event) {
        (Format::Gemini, "user-prompt-submit") => "BeforeAgent",
        (Format::Gemini, "pre-tool-use") => "BeforeTool",
        (Format::Gemini, "post-tool-use") => "AfterTool",
        (_, "user-prompt-submit") => "UserPromptSubmit",
        (_, "session-start") => "SessionStart",
        (_, "pre-tool-use") => "PreToolUse",
        _ => "PostToolUse",
    }
}

pub fn input(format: Format, mut v: Value) -> Option<Value> {
    v.as_object()?;
    if format == Format::Cursor {
        v["session_id"] = v["conversation_id"].clone();
        if !v["cwd"].is_string() {
            v["cwd"] = v["workspace_roots"][0].clone();
        }
    }
    if format == Format::Copilot {
        v["session_id"] = v["sessionId"].clone();
        v["tool_name"] = v["toolName"].clone();
        v["tool_input"] = match v["toolArgs"].as_str() {
            Some(s) => serde_json::from_str(s).ok()?,
            None => v["toolArgs"].clone(),
        };
    }
    if v["tool_name"].is_null() {
        v["tool_name"] = json!("");
    }
    if format == Format::Gemini
        && v["tool_input"].is_object()
        && !v["tool_input"]["file_path"].is_string()
    {
        v["tool_input"]["file_path"] = v["tool_input"]["absolute_path"].clone();
    }
    v["session_id"].as_str()?;
    v["cwd"].as_str()?;
    Some(v)
}

pub fn changed_paths(v: &Value) -> Vec<String> {
    let tool = v["tool_name"].as_str().unwrap_or_default();
    let input = &v["tool_input"];
    if tool == "apply_patch" {
        let patch = input
            .as_str()
            .or_else(|| {
                ["command", "input", "patch"]
                    .iter()
                    .find_map(|k| input[k].as_str())
            })
            .unwrap_or_default();
        return patch
            .lines()
            .filter_map(|l| {
                l.strip_prefix("*** Update File: ")
                    .or_else(|| l.strip_prefix("*** Add File: "))
            })
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect();
    }
    if matches!(tool, "write_file" | "replace") {
        return input["file_path"]
            .as_str()
            .or_else(|| input["absolute_path"].as_str())
            .map(|s| vec![s.to_owned()])
            .unwrap_or_default();
    }
    Vec::new()
}

pub fn context(format: Format, event: &str, text: Option<&str>) -> Option<String> {
    if format == Format::Cursor && event == "user-prompt-submit" {
        return Some(json!({"continue":true}).to_string());
    }
    let text = text.filter(|s| !s.is_empty())?;
    let value = match format {
        Format::Claude => return Some(text.to_owned()),
        Format::Codex => {
            json!({"hookSpecificOutput":{"hookEventName":external_event(format,event),"additionalContext":text}})
        }
        Format::Gemini if event != "pre-tool-use" => {
            json!({"hookSpecificOutput":{"additionalContext":text}})
        }
        Format::Cursor if matches!(event, "session-start" | "post-tool-use") => {
            json!({"additional_context":text})
        }
        Format::Copilot if matches!(event, "session-start" | "post-tool-use") => {
            json!({"additionalContext":text})
        }
        _ => return None,
    };
    Some(value.to_string())
}

pub fn deny(format: Format, reason: &str) -> Option<Value> {
    match format {
        Format::Claude | Format::Codex => Some(
            json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":reason}}),
        ),
        Format::Gemini => Some(json!({"decision":"deny","reason":reason})),
        Format::Cursor => {
            Some(json!({"permission":"deny","user_message":reason,"agent_message":reason}))
        }
        Format::Copilot => {
            Some(json!({"permissionDecision":"deny","permissionDecisionReason":reason}))
        }
    }
}
