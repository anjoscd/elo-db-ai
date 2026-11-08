use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use tempfile::TempDir;

fn elo() -> Command {
    Command::new(env!("CARGO_BIN_EXE_elo"))
}

fn send(dir: &TempDir, cwd: &str, prompt: &str) -> std::process::Output {
    let mut child = elo()
        .args(["hook", "user-prompt-submit", "--shadow"])
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let input =
        serde_json::json!({"session_id": "s1", "cwd": dir.path().join(cwd), "prompt": prompt});
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.to_string().as_bytes())
        .expect("write");
    child.wait_with_output().expect("wait")
}

#[test]
fn shadow_hook_is_silent_scoped_and_never_fails() {
    let dir = TempDir::new().expect("tmp");
    for p in ["api", "web"] {
        fs::create_dir_all(dir.path().join(p)).expect("mkdir");
        fs::write(dir.path().join(p).join("package.json"), "{}").expect("manifest");
    }
    assert!(elo()
        .arg("init")
        .current_dir(dir.path())
        .status()
        .expect("init")
        .success());
    let jsonl = dir.path().join("in.jsonl");
    fs::write(
        &jsonl,
        [
            r#"{"id":"A","kind":"rule","scope":"project:api","title":"api","body":"api database rule","keywords":["database"]}"#,
            r#"{"id":"W","kind":"rule","scope":"project:web","title":"web","body":"web database rule","keywords":["database"]}"#,
        ]
        .join("\n"),
    )
    .expect("jsonl");
    assert!(elo()
        .args(["import", jsonl.to_str().expect("utf8"), "--as-user"])
        .current_dir(dir.path())
        .status()
        .expect("import")
        .success());

    let out = send(&dir, "api", "the database migration");
    assert!(out.status.success());
    assert!(out.stdout.is_empty(), "shadow mode prints nothing");
    let log = fs::read_to_string(dir.path().join(".elo/shadow/log.jsonl")).expect("log");
    let rec: serde_json::Value =
        serde_json::from_str(log.lines().next().expect("line")).expect("json");
    assert_eq!(rec["prompt"], "the database migration");
    assert_eq!(
        rec["full_ids"],
        serde_json::json!(["A"]),
        "only the active project's rule"
    );

    // Garbage input or a folder without .elo never breaks the session.
    let mut child = elo()
        .args(["hook", "user-prompt-submit", "--shadow"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"not json")
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success() && out.stdout.is_empty());
}

#[test]
fn parallel_events_of_one_session_keep_every_increment() {
    let dir = TempDir::new().expect("tmp");
    fs::create_dir_all(dir.path().join("api")).expect("mkdir");
    fs::write(dir.path().join("api/package.json"), "{}").expect("manifest");
    assert!(elo()
        .arg("init")
        .current_dir(dir.path())
        .status()
        .expect("init")
        .success());
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let mut child = elo()
                .args(["hook", "post-tool-use", "--shadow"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("spawn");
            let input = serde_json::json!({
                "session_id": "p1",
                "cwd": dir.path().join("api"),
                "tool_input": {"file_path": format!("src/f{i}.ts")}
            });
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(input.to_string().as_bytes())
                .expect("write");
            child
        })
        .collect();
    for h in handles {
        let out = h.wait_with_output().expect("wait");
        assert!(out.status.success() && out.stdout.is_empty());
    }
    let session: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path().join(".elo/sessions/p1.json")).expect("session"),
    )
    .expect("json");
    assert_eq!(session["seq"], 8, "no lost update");
    let touched = session["state"]["touched"].as_array().expect("touched");
    assert_eq!(touched.len(), 8);
    assert!(
        touched
            .iter()
            .all(|t| t[0].as_str().is_some_and(|p| p.starts_with("api/src/f"))),
        "relative paths resolved against cwd: {touched:?}"
    );
}

#[test]
fn log_do_hook_remove_segredo_e_corta_utf8() {
    let dir = TempDir::new().unwrap();
    elo_ctx::init::init(dir.path()).unwrap();
    for prompt in [
        "senha: valor privado".into(),
        format!("{}🦀", "á ".repeat(1333)),
    ] {
        assert!(send(&dir, ".", &prompt).status.success());
    }
    let log = fs::read_to_string(dir.path().join(".elo/shadow/log.jsonl")).unwrap();
    let records: Vec<serde_json::Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(records[0]["prompt"], "[secret removed]");
    assert_eq!(records[1]["prompt"].as_str().unwrap().len(), 3999);
    assert!(!log.contains("valor privado"));
}
