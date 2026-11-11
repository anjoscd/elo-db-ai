//! Hook latency, alone in its own test binary: Cargo runs test binaries one at a
//! time, so sibling tests spawning processes in parallel threads cannot skew it.
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use serde_json::{json, Value};
use tempfile::TempDir;

fn call(tool: &str, input: Value, cwd: &Path) -> String {
    json!({"session_id": "s1", "cwd": cwd, "tool_name": tool, "tool_input": input}).to_string()
}

fn workspace(toml: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".elo")).unwrap();
    fs::write(dir.path().join("elo.toml"), toml).unwrap();
    dir
}

fn run_hook(dir: &Path, stdin: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(["hook", "pre-tool-use", "--live"])
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
    child.wait_with_output().unwrap();
}

const TOML: &str = r#"
[[guard]]
id = "tool-sem-allow-all"
law = "lei-tool-sem-allow-all"
tool = "Bash"
field = "command"
all = ["sometool", "--allow-all-tools"]
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
fn hook_call_is_fast() {
    let dir = workspace(TOML);
    let input = call("Bash", json!({"command": "ls -la"}), dir.path());
    // Warm-up to stabilise caches.
    for _ in 0..5 {
        run_hook(dir.path(), &input);
    }
    let mut p95s: Vec<u128> = Vec::new();
    for _ in 0..3 {
        let mut times: Vec<u128> = (0..20)
            .map(|_| {
                let start = Instant::now();
                run_hook(dir.path(), &input);
                start.elapsed().as_millis()
            })
            .collect();
        times.sort();
        p95s.push(times[18]);
    }
    p95s.sort();
    let p95 = p95s[0];
    eprintln!("pre-tool-use p95 (lowest of 3 batches): {p95} ms");
    let limit = if cfg!(debug_assertions) { 300 } else { 100 };
    assert!(p95 < limit, "p95 {p95} ms");
}
