mod support;
use serde_json::{json, Value};
use std::fs;
use support::*;

fn payload(root: &std::path::Path, format: &str) -> Value {
    match format {
        "cursor" => json!({"conversation_id":"test","workspace_roots":[root],"prompt":"schema"}),
        "copilot" => json!({"sessionId":"test","cwd":root,"prompt":"schema"}),
        _ => json!({"session_id":"test","cwd":root,"prompt":"schema"}),
    }
}
#[test]
fn prompt_protocols_and_deferred_context() {
    for (format, event) in [
        ("claude", "UserPromptSubmit"),
        ("codex", "UserPromptSubmit"),
        ("gemini", "BeforeAgent"),
        ("cursor", "beforeSubmitPrompt"),
        ("copilot", "userPromptSubmitted"),
    ] {
        let dir = workspace();
        let input = payload(dir.path(), format);
        let out = hook(dir.path(), format, event, input.clone(), true);
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        let text = String::from_utf8(out.stdout).unwrap();
        match format {
            "claude" => assert!(text.contains("Preserve user data.")),
            "codex" | "gemini" => {
                let v: Value = serde_json::from_str(&text).unwrap();
                if format == "codex" {
                    assert_eq!(v["hookSpecificOutput"]["hookEventName"], event);
                } else {
                    assert_eq!(v.as_object().unwrap().len(), 1);
                    assert_eq!(
                        v["hookSpecificOutput"]
                            .as_object()
                            .unwrap()
                            .keys()
                            .collect::<Vec<_>>(),
                        vec!["additionalContext"]
                    );
                }
                assert!(v["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .unwrap()
                    .contains("Preserve user data."));
            }
            _ => {
                if format == "cursor" {
                    assert_eq!(
                        serde_json::from_str::<Value>(&text).unwrap(),
                        json!({"continue":true})
                    );
                } else {
                    assert!(text.is_empty());
                }
                let post = hook(dir.path(), format, "postToolUse", input.clone(), true);
                let v: Value = serde_json::from_slice(&post.stdout).unwrap();
                let key = if format == "cursor" {
                    "additional_context"
                } else {
                    "additionalContext"
                };
                assert!(v[key].as_str().unwrap().contains("Preserve user data."));
                assert!(hook(dir.path(), format, "postToolUse", input, true)
                    .stdout
                    .is_empty());
            }
        }
        assert!(dir.path().join(".elo/shadow/log.jsonl").exists());
    }
}
#[test]
fn native_and_kebab_names_match_and_unknown_events_do_nothing() {
    for (format, native) in [
        ("codex", "UserPromptSubmit"),
        ("gemini", "BeforeAgent"),
        ("cursor", "beforeSubmitPrompt"),
        ("copilot", "userPromptSubmitted"),
    ] {
        let a = workspace();
        let b = workspace();
        let first = hook(a.path(), format, native, payload(a.path(), format), true);
        let second = hook(
            b.path(),
            format,
            "user-prompt-submit",
            payload(b.path(), format),
            true,
        );
        assert_eq!(first.stdout, second.stdout);
        assert!(hook(a.path(), format, "unknown", json!({}), true)
            .stdout
            .is_empty());
        assert!(
            hook(a.path(), format, native, payload(a.path(), format), false)
                .stdout
                .is_empty()
        );
    }
}
#[test]
fn denials_use_each_protocol_and_shadow_is_silent() {
    for format in ["claude", "codex", "gemini", "cursor", "copilot"] {
        let dir = workspace();
        let config = dir.path().join("elo.toml");
        let mut text = fs::read_to_string(&config).unwrap();
        text.push_str("\n[[guard]]\nid = \"block\"\ntool = \"*\"\nfield = \"command\"\nall = [\"danger\"]\naction = \"deny\"\nmessage = \"Blocked operation\"\nenforce = true\n");
        fs::write(config, text).unwrap();
        let mut input = payload(dir.path(), format);
        if format == "copilot" {
            input["toolName"] = json!("shell");
            input["toolArgs"] = json!("{\"command\":\"danger\"}");
        } else {
            input["tool_name"] = json!("shell");
            input["tool_input"] = json!({"command":"danger"});
        }
        let out = hook(dir.path(), format, "pre-tool-use", input.clone(), true);
        if matches!(format, "claude" | "codex") {
            assert!(out.status.success());
            assert!(out.stderr.is_empty());
            let v: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
            assert!(v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("Blocked operation"));
        } else {
            assert!(out.status.success());
            assert!(out.stderr.is_empty());
            let v: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(
                v[match format {
                    "gemini" => "decision",
                    "cursor" => "permission",
                    _ => "permissionDecision",
                }],
                "deny"
            );
        }
        assert!(hook(dir.path(), format, "pre-tool-use", input, false)
            .stdout
            .is_empty());
        assert_eq!(
            fs::read_to_string(dir.path().join(".elo/shadow/guards.jsonl"))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }
}
#[test]
fn editing_tools_refresh_the_existing_map() {
    for (format, event, tool, field) in [
        ("codex", "PostToolUse", "apply_patch", "patch"),
        ("codex", "PostToolUse", "apply_patch", "command"),
        ("codex", "PostToolUse", "apply_patch", "input"),
        ("gemini", "AfterTool", "replace", "absolute_path"),
        ("gemini", "AfterTool", "write_file", "file_path"),
        ("cursor", "afterFileEdit", "", "file_path"),
        ("claude", "PostToolUse", "Edit", "file_path"),
        ("claude", "FileChanged", "", "file_path"),
    ] {
        let dir = workspace();
        fs::write(dir.path().join("a.ts"), "export function oldName() {}\n").unwrap();
        ok(dir.path(), &["map", "sync"]);
        fs::write(
            dir.path().join("a.ts"),
            "export function revisedName() {}\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.ts"), "export function addedName() {}\n").unwrap();
        let mut input = payload(dir.path(), format);
        input["tool_name"] = json!(tool);
        input["tool_input"] = json!({field:if tool=="apply_patch" {"*** Update File: a.ts\n*** Add File: b.ts"} else {"a.ts"}});
        input["file_path"] = json!("a.ts");
        let result = hook(dir.path(), format, event, input, true);
        assert!(result.status.success());
        assert!(ok(dir.path(), &["map", "query", "--contains", "revisedName"]).contains("a.ts"));
        if tool == "apply_patch" {
            assert!(ok(dir.path(), &["map", "query", "--contains", "addedName"]).contains("b.ts"));
        }
    }
}
#[test]
fn gemini_before_tool_never_injects_text() {
    let dir = workspace();
    let mut input = payload(dir.path(), "gemini");
    input["tool_name"] = json!("replace");
    input["tool_input"] = json!({"file_path":"db/migrations/a.ts"});
    let out = hook(dir.path(), "gemini", "BeforeTool", input, true);
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
}

#[test]
fn work_block_is_delivered_at_session_start_or_once_at_post_tool() {
    for format in ["codex", "gemini", "cursor", "copilot"] {
        for start in [false, true] {
            let dir = workspace();
            ok(dir.path(), &["project", "add", "demo", "--path", "."]);
            ok(
                dir.path(),
                &[
                    "handoff",
                    "new",
                    "--project",
                    "demo",
                    "--title",
                    "Pending work",
                    "--body",
                    "Continue the review.",
                ],
            );
            let input = payload(dir.path(), format);
            if start {
                let out = hook(dir.path(), format, "session-start", input.clone(), true);
                let v: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert!(v.to_string().contains("Pending work"));
            }
            if matches!(format, "cursor" | "copilot") {
                hook(
                    dir.path(),
                    format,
                    "user-prompt-submit",
                    input.clone(),
                    true,
                );
                let out = hook(dir.path(), format, "post-tool-use", input.clone(), true);
                let v: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(v.to_string().contains("Pending work"), !start);
                assert!(v.to_string().contains("Check the schema."));
                assert!(hook(dir.path(), format, "post-tool-use", input, true)
                    .stdout
                    .is_empty());
            } else {
                let out = hook(dir.path(), format, "user-prompt-submit", input, true);
                assert!(out.status.success());
                let v: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(v.to_string().contains("Pending work"), !start);
                assert!(v.to_string().contains("Check the schema."));
            }
        }
    }
}

#[test]
fn copilot_tool_args_object_and_string_select_identical_guards() {
    for args in [
        json!({"file_path":"src/demo.ts"}),
        json!("{\"file_path\":\"src/demo.ts\"}"),
    ] {
        let normalized = elo_ctx::adapt::input(
            elo_ctx::adapt::Format::Copilot,
            json!({"sessionId":"s","cwd":"/tmp","toolName":"edit","toolArgs":args}),
        )
        .unwrap();
        assert_eq!(normalized["tool_input"]["file_path"], "src/demo.ts");
    }
}

#[test]
fn allowed_and_ask_operations_are_never_translated_to_denials() {
    for format in ["claude", "codex", "gemini", "cursor", "copilot"] {
        for action in ["ask", "deny"] {
            let dir = workspace();
            let config = dir.path().join("elo.toml");
            let mut text = fs::read_to_string(&config).unwrap();
            text.push_str(&format!("\n[[guard]]\nid = \"review\"\ntool = \"*\"\nfield = \"command\"\nall = [\"review\"]\naction = \"{action}\"\nmessage = \"Review operation\"\nenforce = true\n"));
            fs::write(config, text).unwrap();
            let mut input = payload(dir.path(), format);
            let command = if action == "ask" { "review" } else { "safe" };
            if format == "copilot" {
                input["toolName"] = json!("shell");
                input["toolArgs"] = json!({"command":command});
            } else {
                input["tool_name"] = json!("shell");
                input["tool_input"] = json!({"command":command});
            }
            let out = hook(dir.path(), format, "pre-tool-use", input, true);
            assert!(out.status.success(), "{format}/{action}");
            assert!(out.stderr.is_empty());
            if action == "ask" && matches!(format, "claude" | "codex") {
                let v: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "ask");
            } else {
                assert!(out.stdout.is_empty(), "{format}/{action}");
            }
        }
    }
}

#[test]
fn edited_paths_reject_traversal_and_escape_before_sync() {
    use elo_ctx::events::contained_path;
    let dir = workspace();
    let outside = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.ts"), "export function before() {}\n").unwrap();
    fs::write(
        outside.path().join("outside.ts"),
        "export function escaped() {}\n",
    )
    .unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    ok(dir.path(), &["map", "sync"]);
    let map_before = fs::read(dir.path().join(".elo/map.db")).unwrap();
    fs::write(
        dir.path().join("a.ts"),
        "export function afterTraversal() {}\n",
    )
    .unwrap();
    let absolute = dir.path().join("a.ts");
    assert_eq!(
        contained_path(dir.path(), dir.path(), &absolute),
        Some(absolute.canonicalize().unwrap())
    );
    assert_eq!(
        contained_path(dir.path(), dir.path(), std::path::Path::new("./a.ts")),
        Some(absolute.canonicalize().unwrap())
    );
    for path in [
        "sub/../a.ts".to_owned(),
        outside
            .path()
            .join("outside.ts")
            .to_string_lossy()
            .into_owned(),
    ] {
        assert!(contained_path(dir.path(), dir.path(), std::path::Path::new(&path)).is_none());
        for (format, tool, field) in [
            ("codex", "apply_patch", "patch"),
            ("gemini", "replace", "absolute_path"),
            ("gemini", "write_file", "file_path"),
            ("claude", "Edit", "file_path"),
        ] {
            let mut input = payload(dir.path(), format);
            input["tool_name"] = json!(tool);
            input["tool_input"] = json!({field: if tool == "apply_patch" { format!("*** Update File: {path}") } else { path.clone() }});
            assert!(hook(dir.path(), format, "post-tool-use", input, true)
                .status
                .success());
            assert_eq!(
                fs::read(dir.path().join(".elo/map.db")).unwrap(),
                map_before
            );
        }
    }
    #[cfg(unix)]
    {
        let link = dir.path().join("escape.ts");
        std::os::unix::fs::symlink(outside.path().join("outside.ts"), &link).unwrap();
        assert!(contained_path(dir.path(), dir.path(), &link).is_none());
    }
}
