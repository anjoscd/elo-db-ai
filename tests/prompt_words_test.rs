//! The hook counts the user's own prompt words, and only real prompts.
use std::{io::Write, process::Command, process::Stdio};

use tempfile::TempDir;

fn hook(dir: &std::path::Path, prompt: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(["hook", "user-prompt-submit", "--live"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let body = serde_json::json!({"session_id": "s", "cwd": dir, "prompt": prompt});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.to_string().as_bytes())
        .unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
}

#[test]
fn nothing_is_counted_while_shadow_telemetry_is_off() {
    let dir = TempDir::new().unwrap();
    assert!(Command::new(env!("CARGO_BIN_EXE_elo"))
        .arg("init")
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let toml = dir.path().join("elo.toml");
    let mut text = std::fs::read_to_string(&toml).unwrap();
    text.push_str("\n[shadow]\nenabled = false\n");
    std::fs::write(&toml, text).unwrap();
    hook(dir.path(), "abre a Apple");
    let conn = rusqlite::Connection::open(dir.path().join(".elo").join("elo.db")).unwrap();
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'prompt_words')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!exists);
}

#[test]
fn real_prompts_are_counted_and_agent_messages_are_not() {
    let dir = TempDir::new().unwrap();
    assert!(Command::new(env!("CARGO_BIN_EXE_elo"))
        .arg("init")
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    hook(dir.path(), "abre a Apple");
    hook(dir.path(), "<task-notification> Apple Apple");
    hook(dir.path(), "Apple de novo, e o `Config`");
    let conn = rusqlite::Connection::open(dir.path().join(".elo").join("elo.db")).unwrap();
    let total: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'prompt_words_total'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total, "2");
    let apple: i64 = conn
        .query_row(
            "SELECT prompts FROM prompt_words WHERE word = 'apple'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(apple, 2);
    let config: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM prompt_words WHERE word = 'config'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(config, 0, "words inside code are not counted");
}
