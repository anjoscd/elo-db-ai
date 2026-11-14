use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::json;
use tempfile::TempDir;

fn elo(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn hook(dir: &Path, event: &str, body: serde_json::Value) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(["hook", event, "--live"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn file_changed_keeps_an_existing_map_fresh() {
    let dir = TempDir::new().unwrap();
    elo(dir.path(), &["init"]);
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.ts"), "export function velha() {}\n").unwrap();
    elo(dir.path(), &["map", "sync"]);
    assert!(elo(dir.path(), &["map", "query", "--contains", "novaFuncao"]).contains("0 results"));
    fs::write(
        dir.path().join("src/a.ts"),
        "export function novaFuncao() {}\n",
    )
    .unwrap();
    let out = hook(
        dir.path(),
        "file-changed",
        json!({"cwd": dir.path(), "file_path": "src/a.ts"}),
    );
    assert_eq!(out, "", "file-changed prints nothing");
    let found = elo(dir.path(), &["map", "query", "--contains", "novaFuncao"]);
    assert!(found.contains("src/a.ts"), "{found}");
}

#[test]
fn file_changed_never_creates_a_map_and_ignores_non_code() {
    let dir = TempDir::new().unwrap();
    elo(dir.path(), &["init"]);
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.ts"), "export function x() {}\n").unwrap();
    hook(
        dir.path(),
        "file-changed",
        json!({"cwd": dir.path(), "file_path": "src/a.ts"}),
    );
    assert!(
        !dir.path().join(".elo/map.db").exists(),
        "the hook must not create the map"
    );
    elo(dir.path(), &["map", "sync"]);
    fs::write(dir.path().join("notas.md"), "x").unwrap();
    hook(
        dir.path(),
        "file-changed",
        json!({"cwd": dir.path(), "file_path": "notas.md"}),
    );
    // Broken input: silent.
    let mut child = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(["hook", "file-changed", "--live"])
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{x").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success() && out.stdout.is_empty());
}

#[test]
fn cwd_changed_shows_the_new_project_work_block() {
    let dir = TempDir::new().unwrap();
    elo(dir.path(), &["init"]);
    let app = dir.path().join("app");
    fs::create_dir_all(&app).unwrap();
    elo(dir.path(), &["project", "add", "app", "--path", "app"]);
    elo(
        dir.path(),
        &[
            "handoff",
            "new",
            "--project",
            "app",
            "--title",
            "continuar o mapa",
            "--body",
            "b",
        ],
    );
    let out = hook(dir.path(), "cwd-changed", json!({"cwd": app}));
    assert!(out.contains("continuar o mapa"), "{out}");
    // A folder without project: nothing.
    let other = dir.path().join("outra");
    fs::create_dir_all(&other).unwrap();
    assert_eq!(hook(dir.path(), "cwd-changed", json!({"cwd": other})), "");
}

#[test]
fn post_tool_use_refreshes_only_existing_maps_after_edits() {
    let dir = TempDir::new().unwrap();
    elo(dir.path(), &["init"]);
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.ts"), "export function velha() {}\n").unwrap();
    let body = json!({"session_id": "edicao", "cwd": dir.path(),
        "tool_name": "Edit", "tool_input": {"file_path": "src/a.ts"}});
    assert_eq!(hook(dir.path(), "post-tool-use", body.clone()), "");
    assert!(!dir.path().join(".elo/map.db").exists());
    elo(dir.path(), &["map", "sync"]);
    for (i, tool) in ["Edit", "Write", "MultiEdit", "NotebookEdit"]
        .iter()
        .enumerate()
    {
        let symbol = format!("novaFuncao{i}{}", "x".repeat(i));
        fs::write(
            dir.path().join("src/a.ts"),
            format!("export function {symbol}() {{}}\n"),
        )
        .unwrap();
        let mut event = body.clone();
        event["tool_name"] = json!(tool);
        if *tool == "NotebookEdit" {
            event["tool_input"] = json!({"notebook_path": "src/a.ts"});
        }
        assert_eq!(hook(dir.path(), "post-tool-use", event), "");
        let found = elo(dir.path(), &["map", "query", "--contains", &symbol]);
        assert!(found.contains("src/a.ts"), "{found}");
    }
    fs::write(dir.path().join(".elo/map.db"), "banco inválido").unwrap();
    assert_eq!(hook(dir.path(), "post-tool-use", body), "");
}

#[test]
fn prompt_shows_work_once_per_project_and_session_start_marks_it_shown() {
    let dir = TempDir::new().unwrap();
    elo(dir.path(), &["init"]);
    // Keep this test focused on ordering the work block before full context.
    let path = dir.path().join("elo.toml");
    let config = fs::read_to_string(&path).unwrap();
    fs::write(
        path,
        config.replace("[workspace]", "[workspace]\nfull_min_score = 0"),
    )
    .unwrap();
    for project in ["app", "outro"] {
        fs::create_dir_all(dir.path().join(project).join("src")).unwrap();
        elo(dir.path(), &["project", "add", project, "--path", project]);
        elo(
            dir.path(),
            &[
                "handoff",
                "new",
                "--project",
                project,
                "--title",
                &format!("passagem {project}"),
                "--body",
                "continuar",
            ],
        );
    }
    let rules = dir.path().join("regras.jsonl");
    fs::write(
        &rules,
        json!({"id": "R", "kind": "rule", "scope": "global",
        "title": "contexto normal", "body": "regra para continuar", "always": true})
        .to_string(),
    )
    .unwrap();
    elo(
        dir.path(),
        &["import", rules.to_str().unwrap(), "--as-user"],
    );
    let prompt = |session: &str, folder: &str| {
        hook(
            dir.path(),
            "user-prompt-submit",
            json!({
                "session_id": session, "cwd": dir.path().join(folder), "prompt": "continuar"
            }),
        )
    };
    // Old session files must keep their sequence and accept the new optional field.
    fs::create_dir_all(dir.path().join(".elo/sessions")).unwrap();
    fs::write(
        dir.path().join(".elo/sessions/antiga.json"),
        r#"{"seq":41,"state":{"served":[],"touched":[]}}"#,
    )
    .unwrap();
    let first = prompt("antiga", "app");
    assert!(first.contains("passagem app"), "{first}");
    assert!(
        first.find("passagem app").unwrap() < first.find("regra para continuar").expect(&first)
    );
    let session: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path().join(".elo/sessions/antiga.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(session["seq"], 42);
    assert!(!prompt("antiga", "app/src").contains("passagem app"));
    assert!(!prompt("antiga", "").contains("passagem app"));
    assert!(!prompt("antiga", "app").contains("passagem app"));
    assert!(prompt("antiga", "outro").contains("passagem outro"));
    assert!(!prompt("antiga", "outro").contains("passagem outro"));
    assert!(prompt("antiga", "app").contains("passagem app"));
    let start = hook(
        dir.path(),
        "session-start",
        json!({
            "session_id": "nova", "cwd": dir.path().join("app")
        }),
    );
    assert!(start.contains("passagem app"), "{start}");
    assert!(!prompt("nova", "app").contains("passagem app"));
}

#[test]
fn post_tool_use_reindexes_only_the_edited_root_file() {
    let dir = TempDir::new().unwrap();
    elo(dir.path(), &["init"]);
    for file in ["a", "b"] {
        fs::write(
            dir.path().join(format!("{file}.ts")),
            format!("export function old_{file}() {{ oldCall(); }}\n"),
        )
        .unwrap();
    }
    elo(dir.path(), &["map", "sync"]);
    for file in ["a", "b"] {
        fs::write(
            dir.path().join(format!("{file}.ts")),
            format!("export function new_{file}() {{ newCall(); }}\n"),
        )
        .unwrap();
    }
    let body = json!({"session_id": "isolated", "cwd": dir.path(),
        "tool_name": "Edit", "tool_input": {"file_path": "a.ts"}});
    assert_eq!(hook(dir.path(), "post-tool-use", body.clone()), "");
    let db = rusqlite::Connection::open(dir.path().join(".elo/map.db")).unwrap();
    let names = |table: &str, file: &str| -> Vec<String> {
        db.prepare(&format!(
            "SELECT name FROM {table} WHERE file = ?1 ORDER BY name"
        ))
        .unwrap()
        .query_map([file], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };
    assert!(names("entities", "a.ts").contains(&"new_a".into()));
    assert!(!names("entities", "a.ts").contains(&"old_a".into()));
    assert!(names("entities", "b.ts").contains(&"old_b".into()));
    assert!(!names("entities", "b.ts").contains(&"new_b".into()));
    assert_eq!(names("raw_calls", "a.ts"), ["newCall"]);
    assert_eq!(names("raw_calls", "b.ts"), ["oldCall"]);
    // Failure after deleting old rows must roll back the entire file update.
    db.execute_batch(
        "CREATE TRIGGER fail_insert BEFORE INSERT ON raw_calls
        BEGIN SELECT RAISE(ABORT, 'test failure'); END;",
    )
    .unwrap();
    fs::write(
        dir.path().join("a.ts"),
        "export function failed() { failedCall(); }\n",
    )
    .unwrap();
    assert_eq!(hook(dir.path(), "post-tool-use", body.clone()), "");
    assert!(names("entities", "a.ts").contains(&"new_a".into()));
    assert_eq!(names("raw_calls", "a.ts"), ["newCall"]);
    db.execute_batch("DROP TRIGGER fail_insert").unwrap();
    // Busy maps leave the old snapshot intact and the hook silent.
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    fs::remove_file(dir.path().join("a.ts")).unwrap();
    assert_eq!(hook(dir.path(), "post-tool-use", body.clone()), "");
    assert!(!names("entities", "a.ts").is_empty());
    db.execute_batch("ROLLBACK").unwrap();
    assert_eq!(hook(dir.path(), "post-tool-use", body), "");
    assert!(names("entities", "a.ts").is_empty());
    assert!(names("raw_calls", "a.ts").is_empty());
    assert!(names("entities", "b.ts").contains(&"old_b".into()));
}
