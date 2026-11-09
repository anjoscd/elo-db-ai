//! Printable configuration snippets. No filesystem writes.
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Tool {
    Claude,
    Codex,
    Gemini,
    Cursor,
    Copilot,
    Vscode,
    Windsurf,
    Zed,
    Jetbrains,
}

pub fn render(tool: Tool, project: bool) -> String {
    let prefix = if project { "." } else { "~" };
    let mcp = json!({"elo":{"command":"elo","args":["mcp"]}});
    let mut out = String::new();
    let mut emit = |path: String, value: Value| {
        out.push_str(&format!(
            "# {path}\n{}\n\n",
            serde_json::to_string_pretty(&value).unwrap()
        ));
    };
    let (name, events): (&str, &[&str]) = match tool {
        Tool::Claude => (
            "claude",
            &[
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
            ],
        ),
        Tool::Codex => (
            "codex",
            &[
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
            ],
        ),
        Tool::Gemini => (
            "gemini",
            &["SessionStart", "BeforeAgent", "BeforeTool", "AfterTool"],
        ),
        Tool::Cursor => (
            "cursor",
            &[
                "sessionStart",
                "beforeSubmitPrompt",
                "preToolUse",
                "postToolUse",
                "afterFileEdit",
            ],
        ),
        Tool::Copilot => (
            "copilot",
            &[
                "sessionStart",
                "userPromptSubmitted",
                "preToolUse",
                "postToolUse",
            ],
        ),
        _ => ("", &[]),
    };
    let mut hooks = Map::new();
    for event in events {
        let command = format!("elo hook {event} --live --format {name}");
        let hook = match tool {
            Tool::Cursor => json!([{ "command": command }]),
            Tool::Copilot => {
                json!([{ "type":"command", "bash":command, "powershell":command, "timeoutSec":10 }])
            }
            Tool::Gemini => {
                json!([{ "matcher":"*", "hooks":[{"type":"command","command":command,"timeout":10000}] }])
            }
            _ => {
                json!([{ "matcher":"*", "hooks":[{"type":"command","command":command,"timeout":10}] }])
            }
        };
        hooks.insert((*event).into(), hook);
    }
    match tool {
        Tool::Claude => {
            emit(
                format!("{prefix}/.claude/settings.json"),
                json!({"hooks":hooks}),
            );
            emit(
                if project {
                    "./.mcp.json".into()
                } else {
                    "~/.claude.json".into()
                },
                json!({"mcpServers":mcp}),
            );
        }
        Tool::Codex => {
            emit(
                format!("{prefix}/.codex/hooks.json"),
                json!({"hooks":hooks}),
            );
            out.push_str(&format!("# {prefix}/.codex/config.toml\n[mcp_servers.elo]\ncommand = \"elo\"\nargs = [\"mcp\"]\n"));
        }
        Tool::Gemini => emit(
            format!("{prefix}/.gemini/settings.json"),
            json!({"hooks":hooks,"mcpServers":mcp}),
        ),
        Tool::Cursor => {
            emit(
                format!("{prefix}/.cursor/hooks.json"),
                json!({"version":1,"hooks":hooks}),
            );
            emit(
                format!("{prefix}/.cursor/mcp.json"),
                json!({"mcpServers":mcp}),
            );
        }
        Tool::Copilot | Tool::Vscode => {
            if matches!(tool, Tool::Copilot) {
                emit(
                    if project {
                        ".github/hooks/elo.json"
                    } else {
                        "~/.copilot/hooks/elo.json"
                    }
                    .into(),
                    json!({"version":1,"hooks":hooks}),
                );
            }
            emit(
                if project || matches!(tool, Tool::Copilot) {
                    ".vscode/mcp.json".into()
                } else {
                    match std::env::consts::OS {
                        "macos" => "~/Library/Application Support/Code/User/mcp.json",
                        "windows" => "%APPDATA%/Code/User/mcp.json",
                        _ => "~/.config/Code/User/mcp.json",
                    }
                    .into()
                },
                json!({"servers":{"elo":{"type":"stdio","command":"elo","args":["mcp"]}}}),
            );
        }
        Tool::Windsurf => {
            emit(
                "~/.config/devin/mcp_config.json (current; rules: .devin/rules)".into(),
                json!({"mcpServers":mcp}),
            );
            emit(
                "~/.codeium/windsurf/mcp_config.json (legacy; rules: .windsurf/rules)".into(),
                json!({"mcpServers":mcp}),
            );
        }
        Tool::Zed => emit(
            if project {
                ".zed/settings.json".into()
            } else {
                "~/.config/zed/settings.json".into()
            },
            json!({"context_servers":mcp}),
        ),
        Tool::Jetbrains => emit(
            "Settings > Tools > AI Assistant > MCP (JSON)".into(),
            json!({"mcpServers":mcp}),
        ),
    }
    out
}
