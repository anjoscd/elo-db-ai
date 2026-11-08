use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

pub fn command(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_elo"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}
pub fn ok(root: &Path, args: &[&str]) -> String {
    let out = command(root, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
pub fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    ok(dir.path(), &["init"]);
    let laws = [
        json!({"id":"always","kind":"law","scope":"global","title":"Preserve","body":"Preserve user data.","always":true}),
        json!({"id":"paths","kind":"law","scope":"global","title":"Migration","body":"Review migrations.","paths":["db/migrations*"]}),
        json!({"id":"keyword","kind":"law","scope":"global","title":"Keyword","body":"Check the schema.","keywords":["schema"]}),
    ];
    fs::write(
        dir.path().join("laws.jsonl"),
        laws.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    ok(dir.path(), &["import", "laws.jsonl", "--as-user"]);
    dir
}
#[allow(dead_code)]
pub fn hook(root: &Path, format: &str, event: &str, value: Value, live: bool) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_elo"))
        .current_dir(root)
        .args([
            "hook",
            event,
            "--formato",
            format,
            if live { "--live" } else { "--shadow" },
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(value.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}
