use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use elo_ctx::store::{NewItem, Store};
use serde_json::{json, Value};
use tempfile::TempDir;

fn workspace(toml: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for (id, kind, body, keywords) in [
        (
            "R-guard",
            "rule",
            "Ao mexer nas travas, rode o replay.",
            vec!["guard.rs"],
        ),
        (
            "L-hook",
            "law",
            "Nunca mande src/hook.rs ao revisor externo.",
            vec!["src/hook.rs"],
        ),
        ("N-velha", "note", "Nota substituída.", vec!["guard.rs"]),
    ] {
        store
            .insert_item(&NewItem {
                id,
                kind,
                scope: "global",
                title: id,
                body,
                origin: "user",
            })
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE items SET triggers=?1 WHERE id=?2",
                [
                    json!({"paths":[],"keywords":keywords,"always":false}).to_string(),
                    id.into(),
                ],
            )
            .unwrap();
    }
    store
        .conn
        .execute(
            "UPDATE items SET status='superseded', superseded_by='R-guard' WHERE id='N-velha'",
            [],
        )
        .unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    for f in ["guard.rs", "hook.rs", "outro.rs"] {
        fs::write(dir.path().join("src").join(f), "x").unwrap();
    }
    fs::write(dir.path().join("elo.toml"), toml).unwrap();
    dir
}

fn pre(dir: &Path, session: &str, tool: &str, input: Value, live: bool) -> String {
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
    let body = json!({"session_id": session, "cwd": dir, "tool_name": tool, "tool_input": input});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn context(out: &str) -> String {
    let v: Value = serde_json::from_str(out.trim()).expect("json answer");
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn edit_on_a_cited_file_delivers_once_and_never_the_superseded() {
    let dir = workspace("");
    let first = pre(
        dir.path(),
        "s",
        "Edit",
        json!({"file_path":"src/guard.rs","old_string":"x"}),
        true,
    );
    let ctx = context(&first);
    assert!(ctx.contains("R-guard: Ao mexer nas travas"), "{ctx}");
    assert!(ctx.contains("via file guard.rs"), "{ctx}");
    assert!(
        !ctx.contains("N-velha"),
        "superseded item must never be delivered: {ctx}"
    );
    assert!(ctx.starts_with("<elo-") && ctx.len() <= 1500, "{ctx}");
    let v: Value = serde_json::from_str(first.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert!(v["hookSpecificOutput"].get("permissionDecision").is_none());
    // Same file again in the same session: not repeated.
    assert_eq!(
        pre(
            dir.path(),
            "s",
            "Edit",
            json!({"file_path":"src/guard.rs"}),
            true
        ),
        ""
    );
    // Another session gets it again.
    assert!(context(&pre(
        dir.path(),
        "s2",
        "Read",
        json!({"file_path":"src/guard.rs"}),
        true
    ))
    .contains("R-guard"));
}

#[test]
fn file_without_rule_and_search_tools_deliver_nothing() {
    let dir = workspace("");
    assert_eq!(
        pre(
            dir.path(),
            "s",
            "Edit",
            json!({"file_path":"src/outro.rs"}),
            true
        ),
        ""
    );
    assert_eq!(
        pre(
            dir.path(),
            "s",
            "Grep",
            json!({"pattern":"x","path":"src/guard.rs"}),
            true
        ),
        ""
    );
}

#[test]
fn delivery_is_recorded_for_the_placar_and_shadow_prints_nothing() {
    let dir = workspace("");
    assert_eq!(
        pre(
            dir.path(),
            "s",
            "Edit",
            json!({"file_path":"src/hook.rs"}),
            false
        ),
        ""
    );
    let store = Store::open(dir.path()).unwrap();
    let n: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM injections WHERE item_id='L-hook' AND mode='full'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "the delivery is recorded like a prompt delivery");
}

#[test]
fn guard_and_rule_share_one_answer() {
    let dir = workspace(
        r#"
[[guard]]
id = "revisor-sem-hook-rs"
law = "L-hook"
tool = "Read"
field = "file_path"
all = ["hook.rs"]
action = "ask"
message = "confirme"
enforce = true
"#,
    );
    let out = pre(
        dir.path(),
        "s",
        "Read",
        json!({"file_path":"src/hook.rs"}),
        true,
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "ask");
    assert!(context(&out).contains("L-hook"));
}

#[test]
fn at_most_three_items_and_1500_bytes() {
    let dir = workspace("");
    let store = Store::open(dir.path()).unwrap();
    for n in 0..6 {
        let id = format!("R{n}");
        store
            .insert_item(&NewItem {
                id: &id,
                kind: "rule",
                scope: "global",
                title: &id,
                body: &"texto longo ".repeat(40),
                origin: "user",
            })
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE items SET triggers=?1 WHERE id=?2",
                [
                    json!({"paths":["src/outro.rs"],"keywords":[],"always":false}).to_string(),
                    id,
                ],
            )
            .unwrap();
    }
    drop(store);
    let ctx = context(&pre(
        dir.path(),
        "s",
        "Edit",
        json!({"file_path":"src/outro.rs"}),
        true,
    ));
    assert!(ctx.len() <= 1500, "{}", ctx.len());
    let lines = ctx.lines().filter(|l| l.starts_with('[')).count();
    assert!((1..=3).contains(&lines), "{lines}: {ctx}");
}

#[test]
fn broken_store_prints_nothing() {
    let dir = workspace("");
    fs::write(dir.path().join(".elo/elo.db"), b"isto nao e sqlite").unwrap();
    assert_eq!(
        pre(
            dir.path(),
            "s",
            "Edit",
            json!({"file_path":"src/guard.rs"}),
            true
        ),
        ""
    );
}

#[test]
fn cap_is_three_items_when_all_fit_and_a_big_one_does_not_steal_a_place() {
    let dir = workspace("");
    let store = Store::open(dir.path()).unwrap();
    // One item bigger than the whole envelope ranks first (a law), then five short rules.
    let mut entries = vec![("A-grande".to_string(), "law", "x".repeat(2000))];
    for n in 0..5 {
        entries.push((format!("R-curta-{n}"), "rule", format!("regra curta {n}")));
    }
    for (id, kind, body) in &entries {
        store
            .insert_item(&NewItem {
                id,
                kind,
                scope: "global",
                title: id,
                body,
                origin: "user",
            })
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE items SET triggers=?1 WHERE id=?2",
                [
                    json!({"paths":["src/outro.rs"],"keywords":[],"always":false}).to_string(),
                    id.clone(),
                ],
            )
            .unwrap();
    }
    drop(store);
    let ctx = context(&pre(
        dir.path(),
        "s",
        "Edit",
        json!({"file_path":"src/outro.rs"}),
        true,
    ));
    let lines: Vec<&str> = ctx.lines().filter(|l| l.starts_with('[')).collect();
    assert_eq!(lines.len(), 3, "{ctx}");
    assert!(!ctx.contains("A-grande"), "too big to fit: {ctx}");
    assert!(ctx.len() <= 1500);
}
