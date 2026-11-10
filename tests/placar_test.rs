use elo_ctx::{
    hook,
    stats::{GroupBy, Mode},
    store::{NewItem, Store},
    usage,
};
use serde_json::json;
use tempfile::TempDir;

fn setup(kind: &str) -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .insert_item(&NewItem {
            id: "R",
            kind,
            scope: "global",
            title: "t",
            body: "b",
            origin: "user",
        })
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE items SET triggers=?1",
            [json!({"paths":["src/a.rs"],"keywords":["process_item"],"always":true}).to_string()],
        )
        .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "fn process_item() {}").unwrap();
    (dir, store)
}

fn event(dir: &TempDir, tool: &str, input: serde_json::Value) {
    assert!(hook::prepare(
        "post-tool-use",
        &json!({"cwd":dir.path(),"session_id":"s","tool_name":tool,"tool_input":input}).to_string()
    )
    .unwrap()
    .is_none());
}

fn deliver(dir: &TempDir) {
    assert!(hook::prepare(
        "user-prompt-submit",
        &json!({"cwd":dir.path(),"session_id":"s","prompt":"trabalhar"}).to_string()
    )
    .unwrap()
    .is_some());
}

#[test]
fn delivered_path_edit_counts_once_other_files_and_citations_do_not() {
    let (dir, store) = setup("rule");
    deliver(&dir);
    event(
        &dir,
        "Edit",
        json!({"file_path":"src/other.rs","new_string":"R"}),
    );
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().auto_used, 0);
    event(&dir, "Edit", json!({"file_path":"src/a.rs"}));
    event(&dir, "Edit", json!({"file_path":"src/a.rs"}));
    let stats = store.stats(None, GroupBy::Kind).unwrap();
    assert_eq!(stats.auto_used, 1);
    assert_eq!((stats.good, stats.bad), (0, 0));
    assert_eq!(stats.groups["rule"].auto_used, 1);
}

#[test]
fn citations_pointers_other_sessions_and_expired_deliveries_do_not_count() {
    let (dir, mut store) = setup("note");
    store
        .record_injection("s", 0, 10, &[("R", Mode::Pointer)])
        .unwrap();
    event(&dir, "Edit", json!({"file_path":"src/a.rs"}));
    store
        .record_injection("other", 1, 10, &[("R", Mode::Full)])
        .unwrap();
    event(&dir, "Bash", json!({"command":"echo R process_item"}));
    store
        .record_injection("s", 1, 10, &[("R", Mode::Full)])
        .unwrap();
    event(&dir, "Edit", json!({"text":"R process_item src/a.rs"}));
    // Past the window (usage::DEFAULT_WINDOW events), the delivery no longer counts.
    for _ in 0..usage::DEFAULT_WINDOW {
        event(&dir, "Edit", json!({}));
    }
    event(&dir, "Edit", json!({"file_path":"src/a.rs"}));
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().auto_used, 0);
}

#[test]
fn law_importance_and_owner_feedback_are_unchanged_and_explain_has_dates() {
    let (dir, store) = setup("law");
    deliver(&dir);
    event(&dir, "Edit", json!({"path":"src/a.rs"}));
    let importance: f64 = store
        .conn
        .query_row("SELECT importance FROM items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(importance, 0.5);
    assert!(store.feedback_totals().unwrap().is_empty());
    assert_eq!(store.demoted_count(1).unwrap(), 0);
    let item = elo_ctx::live::load_items(&store).unwrap().remove(0);
    let config = elo_ctx::config::Config::parse("", dir.path()).unwrap();
    let report = elo_ctx::explain::item_report(dir.path(), &config, &item);
    assert!(report.contains("used (auto): 1; when: 20"), "{report}");
}

#[test]
fn write_failure_is_silent_and_nonblocking() {
    let (dir, store) = setup("rule");
    deliver(&dir);
    store.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = std::time::Instant::now();
    event(&dir, "Edit", json!({"file_path":"src/a.rs"}));
    // The placar waits at most 100 ms (busy_timeout) for a parallel writer, then drops.
    assert!(start.elapsed().as_millis() < 500, "{:?}", start.elapsed());
    store.conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().auto_used, 0);
    assert!(!dir.path().join(".elo/shadow/errors.log").exists());
    event(&dir, "Edit", json!({"file_path":"src/a.rs"}));
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().auto_used, 1);
}

#[test]
fn extraction_identifiers_bash_paths_and_workspace_boundary() {
    let (dir, mut store) = setup("rule");
    store
        .record_injection("s", 1, 10, &[("R", Mode::Full)])
        .unwrap();
    let touches = usage::extract(
        dir.path(),
        dir.path(),
        "Edit",
        &json!({"notebook_path":"src/b.rs","old_string":"fn process_item() {}"}),
    );
    usage::record(dir.path(), "s", 2, 3, &touches).unwrap();
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().auto_used, 1);
    let touches = usage::extract(
        dir.path(),
        dir.path(),
        "Bash",
        &json!({"command":"cat 'src/a.rs' missing.rs /etc/passwd"}),
    );
    assert_eq!(touches.paths, ["src/a.rs"]);
    assert!(usage::extract(
        dir.path(),
        dir.path(),
        "Edit",
        &json!({"path":"../escape.rs"})
    )
    .paths
    .is_empty());
    assert_eq!(
        usage::extract(
            dir.path(),
            dir.path(),
            "Read",
            &json!({"paths":["src/a.rs","src/b.rs"]})
        )
        .paths
        .len(),
        2
    );
}

#[test]
fn migration_v3_backs_up_history_and_newer_schema_is_rejected() {
    let (dir, mut store) = setup("rule");
    store.record_feedback("R", true).unwrap();
    store
        .record_injection("s", 1, 10, &[("R", Mode::Full)])
        .unwrap();
    store.conn.execute_batch("DROP TABLE auto_usage; DROP INDEX injections_session_event; UPDATE meta SET value='3' WHERE key='schema_version'").unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.schema_version().unwrap(), 4);
    store.check_schema().unwrap();
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().good, 1);
    let backup = std::fs::read_dir(dir.path().join(".elo"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("backup-v3-"))
        .unwrap();
    let backup = rusqlite::Connection::open(backup.path()).unwrap();
    assert_eq!(
        backup
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "3"
    );
    store
        .conn
        .execute("UPDATE meta SET value='5' WHERE key='schema_version'", [])
        .unwrap();
    assert!(matches!(
        Store::open(dir.path()),
        Err(elo_ctx::store::StoreError::SchemaTooNew { .. })
    ));
}

#[test]
fn post_tool_use_latency() {
    let (dir, mut store) = setup("rule");
    let mut samples = Vec::new();
    for n in 1..=520 {
        store
            .record_injection("bench", n, 10, &[("R", Mode::Full)])
            .unwrap();
        let start = std::time::Instant::now();
        let touches = usage::extract(
            dir.path(),
            dir.path(),
            "Edit",
            &json!({"file_path":"src/a.rs"}),
        );
        usage::record(dir.path(), "bench", n as u64 + 1, 3, &touches).unwrap();
        if n > 20 {
            samples.push(start.elapsed());
        }
    }
    samples.sort();
    eprintln!(
        "auto extra p50={:?} p95={:?} max={:?}",
        samples[249], samples[474], samples[499]
    );
    assert!(samples[474] < std::time::Duration::from_millis(10));
    let start = std::time::Instant::now();
    event(&dir, "Edit", json!({"file_path":"src/a.rs"}));
    eprintln!("post-tool-use completo={:?}", start.elapsed());
}

#[test]
fn each_delivery_can_count_again_and_since_uses_delivery_cohort() {
    let (dir, mut store) = setup("rule");
    let touches = usage::extract(
        dir.path(),
        dir.path(),
        "Edit",
        &json!({"file_path":"src/a.rs"}),
    );
    for seq in [1, 5] {
        store
            .record_injection("s", seq, 10, &[("R", Mode::Full)])
            .unwrap();
        usage::record(dir.path(), "s", seq as u64 + 1, 3, &touches).unwrap();
    }
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().auto_used, 2);
    store
        .conn
        .execute("UPDATE injections SET at=1 WHERE event_seq=1", [])
        .unwrap();
    let stats = store.stats(Some(2), GroupBy::Kind).unwrap();
    assert_eq!((stats.full, stats.auto_used), (1, 1));
}

#[cfg(unix)]
#[test]
fn symlinks_outside_workspace_do_not_count_even_for_new_files() {
    let (dir, _) = setup("rule");
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
    let touches = usage::extract(
        dir.path(),
        dir.path(),
        "Edit",
        &json!({"file_path":"escape/new.rs","new_string":"process_item"}),
    );
    assert!(touches.paths.is_empty());
    assert!(touches.identifiers.is_empty());
}

#[test]
fn many_matching_deliveries_share_one_fast_commit() {
    let (dir, mut store) = setup("rule");
    for n in 0..40 {
        store
            .insert_item(&NewItem {
                id: &format!("R{n}"),
                kind: "rule",
                scope: "global",
                title: "t",
                body: "b",
                origin: "user",
            })
            .unwrap();
    }
    store
        .conn
        .execute(
            "UPDATE items SET triggers=?1",
            [json!({"paths":["src/*"],"keywords":[],"always":true}).to_string()],
        )
        .unwrap();
    let ids: Vec<String> = (0..40).map(|n| format!("R{n}")).collect();
    let entries: Vec<(&str, Mode)> = ids.iter().map(|id| (id.as_str(), Mode::Full)).collect();
    let touches = usage::extract(
        dir.path(),
        dir.path(),
        "Edit",
        &json!({"file_path":"src/a.rs"}),
    );
    store.record_injection("s", 1, 1000, &entries).unwrap();
    let start = std::time::Instant::now();
    usage::record(dir.path(), "s", 2, 3, &touches).unwrap();
    eprintln!("40 sinais em uma transação: {:?}", start.elapsed());
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().auto_used, 40);
}

/// Items imported from another memory tool carry file names as keywords, not path patterns.
#[test]
fn file_name_keywords_match_the_touched_path_by_whole_component() {
    let (dir, store) = setup("rule");
    store
        .conn
        .execute(
            "UPDATE items SET triggers=?1",
            [json!({"paths":[],"keywords":["guard.rs","travas"],"always":true}).to_string()],
        )
        .unwrap();
    std::fs::write(dir.path().join("src/guard.rs"), "x").unwrap();
    std::fs::write(dir.path().join("src/myguard.rs"), "x").unwrap();
    deliver(&dir);
    event(&dir, "Edit", json!({"file_path":"src/myguard.rs"}));
    assert_eq!(
        store.stats(None, GroupBy::Kind).unwrap().auto_used,
        0,
        "myguard.rs is not guard.rs"
    );
    event(&dir, "Edit", json!({"file_path":"src/guard.rs"}));
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().auto_used, 1);
}

/// Writing an identifier (comment, doc) is citing it, never using it.
#[test]
fn identifier_written_in_new_string_does_not_count() {
    let (dir, store) = setup("rule");
    store
        .conn
        .execute(
            "UPDATE items SET triggers=?1",
            [json!({"paths":[],"keywords":["process_item"],"always":true}).to_string()],
        )
        .unwrap();
    std::fs::write(dir.path().join("src/b.rs"), "fn main() {}").unwrap();
    deliver(&dir);
    event(
        &dir,
        "Edit",
        json!({"file_path":"src/b.rs","old_string":"fn main() {}","new_string":"// ver process_item\nfn main() {}"}),
    );
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().auto_used, 0);
    event(
        &dir,
        "Edit",
        json!({"file_path":"src/b.rs","old_string":"process_item();","new_string":"x();"}),
    );
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().auto_used, 1);
}

/// `Grep`/`Glob` with `path` only search; they do not touch the folder.
#[test]
fn search_tools_with_path_do_not_count() {
    let (dir, store) = setup("rule");
    deliver(&dir);
    event(&dir, "Grep", json!({"pattern":"x","path":"src/a.rs"}));
    event(&dir, "Glob", json!({"pattern":"*.rs","path":"src/a.rs"}));
    assert_eq!(store.stats(None, GroupBy::Kind).unwrap().auto_used, 0);
}
