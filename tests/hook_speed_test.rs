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
    run_elo(dir, &["hook", "pre-tool-use", "--live"], stdin);
}

fn run_elo(dir: &Path, args: &[&str], stdin: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(args)
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

fn timed(f: impl FnOnce()) -> u128 {
    let start = Instant::now();
    f();
    start.elapsed().as_millis()
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
    // The hook's own cost: its time minus the time to start the same binary for
    // `--version`, measured in alternation so both see the same machine load.
    // Starting any process can take 75 ms or more on a loaded machine, which says
    // nothing about the hook.
    let mut costs: Vec<i128> = Vec::new();
    for _ in 0..3 {
        let mut hook: Vec<u128> = Vec::new();
        let mut start: Vec<u128> = Vec::new();
        for _ in 0..20 {
            hook.push(timed(|| run_hook(dir.path(), &input)));
            start.push(timed(|| run_elo(dir.path(), &["--version"], "")));
        }
        hook.sort();
        start.sort();
        costs.push(hook[18] as i128 - start[18] as i128);
        eprintln!(
            "pre-tool-use p95 {} ms, process start p95 {} ms",
            hook[18], start[18]
        );
    }
    costs.sort();
    let cost = costs[1];
    eprintln!("pre-tool-use own cost at p95 (median of 3 batches): {cost} ms");
    let limit = if cfg!(debug_assertions) { 200 } else { 50 };
    assert!(cost < limit, "hook cost {cost} ms");
}
