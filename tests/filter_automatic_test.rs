use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use tempfile::TempDir;

fn elo() -> Command {
    Command::new(env!("CARGO_BIN_EXE_elo"))
}

fn send(dir: &TempDir, cwd: &str, prompt: &str) -> serde_json::Value {
    send_with_tool(dir, cwd, prompt, None)
}

fn send_with_tool(
    dir: &TempDir,
    cwd: &str,
    prompt: &str,
    tool_input: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut child = elo()
        .args(["hook", "user-prompt-submit", "--shadow"])
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut input =
        serde_json::json!({"session_id": "s1", "cwd": dir.path().join(cwd), "prompt": prompt});
    if let Some(tool_input) = tool_input {
        input["tool_input"] = tool_input;
    }
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.to_string().as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "hook failed: {:?}", out.stderr);
    assert!(out.stdout.is_empty(), "shadow mode prints nothing");
    let log = fs::read_to_string(dir.path().join(".elo/shadow/log.jsonl")).expect("log");
    let line = log.lines().last().expect("log line");
    serde_json::from_str(line).expect("json")
}

fn init_workspace_with_projects(projects: &[&str]) -> TempDir {
    let dir = TempDir::new().expect("tmp");
    for p in projects {
        fs::create_dir_all(dir.path().join(p)).expect("mkdir");
        fs::write(dir.path().join(p).join("package.json"), "{}").expect("manifest");
    }
    assert!(elo()
        .arg("init")
        .current_dir(dir.path())
        .status()
        .expect("init")
        .success());
    dir
}

fn import(dir: &TempDir, lines: &[&str]) {
    let jsonl = dir.path().join("in.jsonl");
    fs::write(&jsonl, lines.join("\n")).expect("jsonl");
    assert!(elo()
        .args(["import", jsonl.to_str().expect("utf8"), "--as-user"])
        .current_dir(dir.path())
        .status()
        .expect("import")
        .success());
}

#[test]
fn automatic_agent_message_does_not_inject_keyword_rules() {
    let dir = init_workspace_with_projects(&["app"]);
    import(
        &dir,
        &[
            r#"{"id":"R","kind":"rule","scope":"project:app","title":"billing","body":"billing rule","keywords":["billing"]}"#,
        ],
    );

    let rec = send(&dir, "app", "<agent-message> billing migration");
    assert_eq!(rec["automatic"], true, "automatic flag should be true");
    assert!(
        rec["full_ids"].as_array().unwrap().is_empty(),
        "no full rule for automatic prompt"
    );
    assert!(
        rec["pointer_ids"].as_array().unwrap().is_empty(),
        "no pointer for automatic prompt"
    );
}

#[test]
fn normal_prompt_keeps_injecting_keyword_rules() {
    let dir = init_workspace_with_projects(&["app"]);
    import(
        &dir,
        &[
            r#"{"id":"R","kind":"rule","scope":"project:app","title":"billing","body":"billing rule","keywords":["billing"]}"#,
        ],
    );

    let rec = send(&dir, "app", "billing migration");
    assert!(
        !rec["automatic"].as_bool().unwrap_or(true),
        "normal prompt is not automatic"
    );
    assert_eq!(
        rec["full_ids"],
        serde_json::json!(["R"]),
        "rule injected for normal prompt"
    );
}

#[test]
fn marker_in_middle_keeps_keyword_rules() {
    let dir = init_workspace_with_projects(&["app"]);
    import(
        &dir,
        &[
            r#"{"id":"R","kind":"rule","scope":"project:app","title":"billing","body":"billing rule","keywords":["billing"]}"#,
        ],
    );

    let rec = send(
        &dir,
        "app",
        "o que significa <system-reminder> no log sobre billing?",
    );
    assert_eq!(
        rec["automatic"], false,
        "marker in the middle is not automatic"
    );
    assert_eq!(
        rec["full_ids"],
        serde_json::json!(["R"]),
        "keyword rule is injected for normal prompt"
    );
}

#[test]
fn leading_whitespace_marks_automatic() {
    let dir = init_workspace_with_projects(&["app"]);
    import(
        &dir,
        &[
            r#"{"id":"R","kind":"rule","scope":"project:app","title":"billing","body":"billing rule","keywords":["billing"]}"#,
        ],
    );

    for prompt in [
        "   <system-reminder> billing",
        "\n\n<system-reminder> billing",
    ] {
        let rec = send(&dir, "app", prompt);
        assert_eq!(
            rec["automatic"], true,
            "leading whitespace is trimmed before marker check"
        );
        assert!(
            rec["full_ids"].as_array().unwrap().is_empty(),
            "automatic prompt drops keyword rules"
        );
    }
}

#[test]
fn own_project_rule_from_workspace_root_stays_full() {
    let dir = init_workspace_with_projects(&["api", "web"]);
    import(
        &dir,
        &[
            r#"{"id":"A","kind":"rule","scope":"project:api","title":"api","body":"api rule","keywords":["database"]}"#,
            r#"{"id":"W","kind":"rule","scope":"project:web","title":"web","body":"web rule","keywords":["database"]}"#,
        ],
    );

    // From the workspace root the primary project is absent, but the api project is in scope
    // because the agent recently touched a file inside it. The api rule stays full; the web rule
    // (only keyword-matched) becomes a pointer.
    let touch = serde_json::json!({"file_path": "api/package.json"});
    let rec = send_with_tool(&dir, ".", "database migration", Some(touch));
    assert_eq!(
        rec["full_ids"],
        serde_json::json!(["A"]),
        "own-project rule stays full even when primary is absent"
    );
    assert!(
        rec["pointer_ids"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("W")),
        "other project rule is a pointer"
    );
}

#[test]
fn automatic_keeps_always_and_project_block() {
    let dir = init_workspace_with_projects(&["app"]);
    import(
        &dir,
        &[
            r#"{"id":"R","kind":"rule","scope":"project:app","title":"billing","body":"billing rule","keywords":["billing"]}"#,
            r#"{"id":"A","kind":"rule","scope":"global","title":"always","body":"always rule","keywords":[],"always":true}"#,
            r#"{"id":"P","kind":"rule","scope":"project:app","title":"project block","body":"project block note","keywords":[],"always":true}"#,
        ],
    );

    let rec = send(&dir, "app", "<system-reminder> billing reminder");
    assert_eq!(rec["automatic"], true);
    assert!(
        rec["full_ids"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("A")),
        "always rule is kept"
    );
    assert!(
        !rec["full_ids"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("R")),
        "keyword rule is dropped"
    );
    assert!(
        rec["rendered"]
            .as_str()
            .unwrap()
            .contains("project block note"),
        "project/session block is rendered"
    );
}

#[test]
fn cross_project_keyword_becomes_pointer_and_limits_to_three() {
    let dir = init_workspace_with_projects(&["a", "b", "c", "d", "e"]);
    import(
        &dir,
        &[
            r#"{"id":"A","kind":"rule","scope":"project:a","title":"a","body":"a rule","keywords":["k"]}"#,
            r#"{"id":"B","kind":"rule","scope":"project:b","title":"b","body":"b rule","keywords":["k"]}"#,
            r#"{"id":"C","kind":"rule","scope":"project:c","title":"c","body":"c rule","keywords":["k"]}"#,
            r#"{"id":"D","kind":"rule","scope":"project:d","title":"d","body":"d rule","keywords":["k"]}"#,
            r#"{"id":"E","kind":"rule","scope":"project:e","title":"e","body":"e rule","keywords":["k","x"]}"#,
        ],
    );

    let rec = send(&dir, ".", "k x");
    assert!(
        rec["full_ids"].as_array().unwrap().is_empty(),
        "cross-project rules are never full"
    );
    let pointers = rec["pointer_ids"].as_array().unwrap();
    assert_eq!(pointers.len(), 3, "at most 3 cross-project pointers");
    assert_eq!(
        pointers,
        &vec![
            serde_json::json!("E"),
            serde_json::json!("A"),
            serde_json::json!("B")
        ],
        "most relevant pointers are kept (E matches two words, then ids in order)"
    );
}

#[test]
fn inside_project_keyword_remains_full_and_others_become_pointer() {
    let dir = init_workspace_with_projects(&["api", "web"]);
    import(
        &dir,
        &[
            r#"{"id":"A","kind":"rule","scope":"project:api","title":"api","body":"api rule","keywords":["database"]}"#,
            r#"{"id":"W","kind":"rule","scope":"project:web","title":"web","body":"web rule","keywords":["database"]}"#,
        ],
    );

    let rec = send(&dir, "api", "database migration");
    assert_eq!(
        rec["full_ids"],
        serde_json::json!(["A"]),
        "own project rule is full"
    );
    assert!(
        rec["pointer_ids"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("W")),
        "other project rule is a pointer"
    );
}
