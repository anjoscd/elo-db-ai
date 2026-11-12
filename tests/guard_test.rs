use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use elo_ctx::config::Config;
use elo_ctx::guard::{evaluate, validate, Guard};
use serde_json::{json, Value};
use tempfile::TempDir;

fn guard(id: &str, tool: &str, field: &str, all: &[&str], action: &str, enforce: bool) -> Guard {
    Guard {
        id: id.into(),
        law: format!("lei-{id}"),
        tool: tool.into(),
        field: field.into(),
        all: all.iter().map(|s| s.to_string()).collect(),
        action: action.into(),
        message: format!("mensagem {id}"),
        enforce,
        ..Guard::default()
    }
}

fn call(tool: &str, input: Value, cwd: &Path) -> String {
    json!({"session_id": "s1", "cwd": cwd, "tool_name": tool, "tool_input": input}).to_string()
}

fn workspace(toml: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    elo_ctx::store::Store::create(dir.path()).unwrap();
    fs::write(dir.path().join("elo.toml"), toml).unwrap();
    dir
}

fn log_lines(root: &Path) -> Vec<Value> {
    fs::read_to_string(root.join(".elo/shadow/guards.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn matching_uses_tool_field_all_any_and_none() {
    let mut g = guard("g", "Bash", "command", &["copilot"], "deny", true);
    g.any = vec!["--allow-all-tools".into(), "--yolo".into()];
    g.none = vec!["--help".into()];
    let yes = json!({"command": "copilot -p x --allow-all-tools"});
    assert!(g.matches("Bash", &yes));
    assert!(!g.matches("Edit", &yes), "other tool");
    assert!(
        !g.matches("Bash", &json!({"command": "copilot -p x"})),
        "no `any`"
    );
    assert!(
        !g.matches(
            "Bash",
            &json!({"command": "copilot --allow-all-tools --help"})
        ),
        "`none` present"
    );
    assert!(
        !g.matches("Bash", &json!({"file_path": "x"})),
        "field missing"
    );
    let mut star = guard("s", "*", "*", &["\"run_in_background\":true"], "deny", true);
    star.any.clear();
    assert!(star.matches("Bash", &json!({"command": "ls", "run_in_background": true})));
    assert!(!star.matches(
        "Bash",
        &json!({"command": "ls", "run_in_background": false})
    ));
}

#[test]
fn boolean_fields_compare_as_json() {
    let g = guard("bg", "Bash", "run_in_background", &["true"], "deny", true);
    assert!(g.matches(
        "Bash",
        &json!({"command": "sleep 1", "run_in_background": true})
    ));
    assert!(!g.matches(
        "Bash",
        &json!({"command": "sleep 1", "run_in_background": false})
    ));
    assert!(!g.matches("Bash", &json!({"command": "sleep 1"})));
}

#[test]
fn shadow_guard_records_without_answering() {
    let dir = TempDir::new().unwrap();
    let config = Config {
        guards: vec![guard(
            "sombra",
            "Bash",
            "command",
            &["git push"],
            "deny",
            false,
        )],
        ..Config::default()
    };
    let input = call(
        "Bash",
        json!({"command": "git push origin main --token=SEGREDO123"}),
        dir.path(),
    );
    assert_eq!(evaluate(dir.path(), &config, &input), None);
    let lines = log_lines(dir.path());
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["guard"], "sombra");
    assert_eq!(lines[0]["enforced"], false);
    let raw = fs::read_to_string(dir.path().join(".elo/shadow/guards.jsonl")).unwrap();
    assert!(
        !raw.contains("SEGREDO123") && !raw.contains("git push"),
        "no tool input in log: {raw}"
    );
}

#[test]
fn enforced_deny_and_ask_answer_in_the_claude_code_format() {
    let dir = TempDir::new().unwrap();
    let config = Config {
        guards: vec![
            guard("pede", "Bash", "command", &["deploy"], "ask", true),
            guard(
                "barra",
                "Bash",
                "command",
                &["deploy", "--allow-all-tools"],
                "deny",
                true,
            ),
        ],
        ..Config::default()
    };
    let ask = evaluate(
        dir.path(),
        &config,
        &call("Bash", json!({"command": "deploy.sh"}), dir.path()),
    )
    .expect("ask");
    let ask: Value = serde_json::from_str(&ask).unwrap();
    assert_eq!(ask["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(ask["hookSpecificOutput"]["permissionDecision"], "ask");
    let reason = ask["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("pede") && reason.contains("lei-pede") && reason.contains("mensagem pede")
    );
    // Both match: deny wins over ask.
    let both = evaluate(
        dir.path(),
        &config,
        &call(
            "Bash",
            json!({"command": "deploy.sh --allow-all-tools"}),
            dir.path(),
        ),
    )
    .unwrap();
    assert!(both.contains("\"deny\""), "{both}");
    // No match: allow (no answer).
    assert_eq!(
        evaluate(
            dir.path(),
            &config,
            &call("Bash", json!({"command": "ls"}), dir.path())
        ),
        None
    );
}

#[test]
fn an_invalid_guard_set_is_never_enforced() {
    let dir = TempDir::new().unwrap();
    let mut broken = guard("ruim", "Bash", "command", &[], "deny", true);
    broken.any.clear();
    let config = Config {
        guards: vec![
            guard("boa", "Bash", "command", &["x"], "deny", true),
            broken,
        ],
        ..Config::default()
    };
    assert!(!validate(&config.guards).is_empty());
    assert_eq!(
        evaluate(
            dir.path(),
            &config,
            &call("Bash", json!({"command": "x"}), dir.path())
        ),
        None
    );
    assert_eq!(log_lines(dir.path())[0]["enforced"], false);
}

#[test]
fn bad_input_allows() {
    let dir = TempDir::new().unwrap();
    let config = Config {
        guards: vec![guard("g", "*", "*", &["a"], "deny", true)],
        ..Config::default()
    };
    assert_eq!(evaluate(dir.path(), &config, "not json"), None);
    assert_eq!(elo_ctx::guard::hook("{oops"), None);
}

fn run_hook(dir: &Path, stdin: &str, live: bool) -> (Option<i32>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args([
            "hook",
            "pre-tool-use",
            if live { "--live" } else { "--shadow" },
        ])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

const TOML: &str = r#"
[[guard]]
id = "copilot-sem-allow-all"
law = "lei-copilot-sem-allow-all"
tool = "Bash"
field = "command"
all = ["copilot", "--allow-all-tools"]
action = "deny"
message = "liberar ferramentas uma a uma com shell(cmd:*)"
enforce = true

[[guard]]
id = "push-so-com-ordem"
law = "lei-0811-publicar-autorizacao"
tool = "Bash"
field = "command"
all = ["git push"]
action = "ask"
message = "publicar só com a ordem do dono"
"#;

#[test]
fn binary_end_to_end_from_a_subfolder() {
    let dir = workspace(TOML);
    let sub = dir.path().join("app/src");
    fs::create_dir_all(&sub).unwrap();
    let deny = call(
        "Bash",
        json!({"command": "copilot -p x --allow-all-tools"}),
        &sub,
    );
    let (code, out) = run_hook(dir.path(), &deny, true);
    assert_eq!(code, Some(0));
    let answer: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(answer["hookSpecificOutput"]["permissionDecision"], "deny");
    // Shadow guard (no enforce): recorded, silent.
    let push = call("Bash", json!({"command": "git push origin main"}), &sub);
    assert_eq!(run_hook(dir.path(), &push, true), (Some(0), String::new()));
    // --shadow never answers, even for an enforced guard.
    assert_eq!(run_hook(dir.path(), &deny, false), (Some(0), String::new()));
    let guards: Vec<String> = log_lines(dir.path())
        .iter()
        .map(|l| l["guard"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        guards,
        [
            "copilot-sem-allow-all",
            "push-so-com-ordem",
            "copilot-sem-allow-all"
        ]
    );
    // Broken stdin and a folder without ELO: allow silently.
    assert_eq!(run_hook(dir.path(), "{x", true), (Some(0), String::new()));
    let outside = TempDir::new().unwrap();
    assert_eq!(
        run_hook(
            outside.path(),
            &call(
                "Bash",
                json!({"command": "copilot --allow-all-tools"}),
                outside.path()
            ),
            true
        ),
        (Some(0), String::new())
    );
}

#[test]
fn a_broken_elo_toml_allows() {
    let dir = workspace("[[guard]]\nid = 1\n");
    let input = call(
        "Bash",
        json!({"command": "copilot --allow-all-tools"}),
        dir.path(),
    );
    assert_eq!(run_hook(dir.path(), &input, true), (Some(0), String::new()));
}

#[test]
fn empty_fields_are_invalid_and_logged_action_is_preserved() {
    for field in ["", " "] {
        let g = guard("g", "Bash", field, &["x"], "deny", true);
        assert!(validate(&[g]).iter().any(|p| p.contains("missing field")));
    }
    let dir = TempDir::new().unwrap();
    let config = Config {
        guards: vec![guard("g", "Bash", "command", &["x"], "audit", true)],
        ..Config::default()
    };
    let input = json!({"command":"x", "extra":"á"});
    assert_eq!(
        evaluate(
            dir.path(),
            &config,
            &call("Bash", input.clone(), dir.path())
        ),
        None
    );
    let lines = log_lines(dir.path());
    assert_eq!(lines[0]["action"], "audit");
    assert_eq!(lines[0]["enforced"], false);
    assert_eq!(lines[0]["input_bytes"], input.to_string().len());
}

#[test]
fn oversized_log_rotates_and_replaces_previous_backup() {
    let dir = TempDir::new().unwrap();
    let shadow = dir.path().join(".elo/shadow");
    fs::create_dir_all(&shadow).unwrap();
    let path = shadow.join("guards.jsonl");
    let backup = shadow.join("guards.jsonl.1");
    fs::write(&backup, "old backup").unwrap();
    let file = fs::File::create(&path).unwrap();
    file.set_len(5 * 1024 * 1024).unwrap();
    let config = Config {
        guards: vec![guard("g", "Bash", "command", &["x"], "ask", false)],
        ..Config::default()
    };
    let input = call("Bash", json!({"command":"x"}), dir.path());
    evaluate(dir.path(), &config, &input);
    assert_eq!(
        fs::read_to_string(&backup).unwrap(),
        "old backup",
        "exactly 5 MiB does not rotate"
    );
    let old_size = fs::metadata(&path).unwrap().len();
    assert!(old_size > 5 * 1024 * 1024);
    evaluate(dir.path(), &config, &input);
    assert_eq!(fs::metadata(&backup).unwrap().len(), old_size);
    assert_eq!(log_lines(dir.path()).len(), 1);
    assert_eq!(log_lines(dir.path())[0]["action"], "ask");
}

fn large_heredoc_time(bytes: usize, count: usize) -> std::time::Duration {
    let dir = TempDir::new().unwrap();
    let config = Config {
        guards: (0..count)
            .map(|i| guard(&format!("g{i}"), "Bash", "command", &["cat"], "deny", false))
            .collect(),
        ..Config::default()
    };
    let command = format!("cat <<'EOF'\n{}\nEOF", "x".repeat(bytes));
    let input = call("Bash", json!({"command":command}), dir.path());
    let start = Instant::now();
    assert_eq!(evaluate(dir.path(), &config, &input), None);
    let elapsed = start.elapsed();
    assert_eq!(log_lines(dir.path()).len(), count);
    elapsed
}

#[test]
fn many_guards_on_200_kb_finish_under_one_second() {
    let elapsed = large_heredoc_time(200 * 1024, 20);
    assert!(elapsed.as_secs_f64() < 1.0, "{elapsed:?}");
}

// Run: cargo test --release --test guard_test large_heredoc_release -- --ignored
#[test]
#[ignore = "performance budget measured in release"]
fn large_heredoc_release() {
    let elapsed = large_heredoc_time(2 * 1024 * 1024, 50);
    assert!(elapsed.as_millis() < 100, "{elapsed:?}");
}
