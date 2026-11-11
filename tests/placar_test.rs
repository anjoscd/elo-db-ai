use elo_ctx::{
    hook,
    stats::{GroupBy, Mode},
    store::{NewItem, Store, StoreError},
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
fn agent_feedback_rejects_undelivered_foreign_and_stale_ids() {
    let (dir, mut store) = setup("rule");
    deliver(&dir); // seq 0 -> "R" delivered in session "s"
    event(&dir, "Edit", json!({"file_path":"src/a.rs"}));

    let (acc, rej) = store
        .record_agent_feedback("s", &["R".into(), "GHOST".into()], &[], 40)
        .unwrap();
    assert_eq!(acc, vec!["R".to_string()]);
    assert_eq!(rej, vec!["GHOST".to_string()]);

    // Delivered elsewhere is not delivered here.
    let (acc2, rej2) = store
        .record_agent_feedback("other", &["R".into()], &[], 40)
        .unwrap();
    assert!(acc2.is_empty());
    assert_eq!(rej2, vec!["R".to_string()]);

    // A delivery outside the recent window no longer counts as delivered.
    for id in ["W40", "W41"] {
        store
            .insert_item(&NewItem {
                id,
                kind: "rule",
                scope: "global",
                title: "t",
                body: "b",
                origin: "user",
            })
            .unwrap();
    }
    store
        .record_injection("s", 10, 3, &[("W41", Mode::Pointer)])
        .unwrap();
    store
        .record_injection("s", 11, 3, &[("W40", Mode::Pointer)])
        .unwrap();
    store
        .record_injection("s", 50, 3, &[("R", Mode::Full)])
        .unwrap();
    // At current seq 50 with window 40, [11, 50] has 40 positions: seq 11 is accepted, seq 10 is rejected.
    let (acc3, rej3) = store
        .record_agent_feedback("s", &["W40".into(), "W41".into()], &[], 40)
        .unwrap();
    assert_eq!(acc3, vec!["W40".to_string()]);
    assert_eq!(rej3, vec!["W41".to_string()]);

    // Advancing the session to seq 100 without delivering any item ages out seq 10 and seq 50.
    std::fs::create_dir_all(dir.path().join(".elo/sessions")).unwrap();
    std::fs::write(
        dir.path().join(".elo/sessions/s.json"),
        serde_json::json!({"seq": 100, "state": {}}).to_string(),
    )
    .unwrap();
    let (acc4, rej4) = store
        .record_agent_feedback("s", &["R".into()], &[], 40)
        .unwrap();
    assert!(acc4.is_empty());
    assert_eq!(rej4, vec!["R".to_string()]);
}

#[test]
fn agent_feedback_limits_each_list_to_twenty_ids() {
    let (_dir, store) = setup("rule");
    let ids: Vec<String> = (0..21).map(|i| format!("x{i}")).collect();
    let err = store
        .record_agent_feedback("s", &ids, &[], 40)
        .expect_err("twenty-one ids are too many");
    match &err {
        StoreError::Io { path, .. } => assert_eq!(path.to_string_lossy(), "feedback"),
        other => panic!("unexpected error {other:?}"),
    }
    let ids20: Vec<String> = (0..20).map(|i| format!("y{i}")).collect();
    // All rejected as undelivered, but the call itself must succeed at exactly 20.
    let (acc, rej) = store.record_agent_feedback("s", &ids20, &[], 40).unwrap();
    assert!(acc.is_empty());
    assert_eq!(rej.len(), 20);
}

#[test]
fn agent_feedback_used_is_corroborated_by_the_objective_signal() {
    let (dir, store) = setup("rule");
    deliver(&dir);
    event(&dir, "Edit", json!({"file_path":"src/a.rs"}));
    let (acc, _rej) = store
        .record_agent_feedback("s", &["R".into()], &[], 40)
        .unwrap();
    assert_eq!(acc, vec!["R".to_string()]);
    let corroborated: i64 = store
        .conn
        .query_row(
            "SELECT corroborated FROM agent_feedback WHERE item_id='R'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(corroborated, 1);
}

#[test]
fn agent_corroboration_is_recalculated_when_the_signal_arrives_later() {
    let (dir, store) = setup("rule");
    deliver(&dir);
    let (acc, _rej) = store
        .record_agent_feedback("s", &["R".into()], &[], 40)
        .unwrap();
    assert_eq!(acc.len(), 1);
    let stats = store.stats(None, GroupBy::Kind).unwrap();
    assert_eq!(
        (
            stats.agent_used,
            stats.agent_ignored,
            stats.agent_corroborated
        ),
        (1, 0, 0)
    );
    // The objective signal shows up after the report.
    event(&dir, "Edit", json!({"file_path":"src/a.rs"}));
    let stats = store.stats(None, GroupBy::Kind).unwrap();
    assert_eq!(
        (
            stats.agent_used,
            stats.agent_ignored,
            stats.agent_corroborated,
            stats.agent_contradicted,
        ),
        (1, 0, 1, 0)
    );
}

#[test]
fn ignored_verdict_with_objective_signal_is_contradicted_not_corroborated() {
    let (dir, store) = setup("rule");
    deliver(&dir);
    event(&dir, "Edit", json!({"file_path":"src/a.rs"}));
    let (acc, rej) = store
        .record_agent_feedback("s", &[], &["R".into()], 40)
        .unwrap();
    assert_eq!(acc, vec!["R".to_string()]);
    assert!(rej.is_empty());
    let corroborated: i64 = store
        .conn
        .query_row(
            "SELECT corroborated FROM agent_feedback WHERE item_id='R'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(corroborated, 0);
    let stats = store.stats(None, GroupBy::Kind).unwrap();
    assert_eq!(stats.agent_used, 0);
    assert_eq!(stats.agent_ignored, 1);
    assert_eq!(stats.agent_corroborated, 0);
    assert_eq!(stats.agent_contradicted, 1);

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_elo"))
        .arg("stats")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("agent corroborated n/a"), "{stdout}");
    assert!(stdout.contains("agent contradicted 1"), "{stdout}");
}

#[test]
fn agent_concordance_handles_multiple_owner_and_agent_verdicts_without_cartesian_product() {
    let (_dir, mut store) = setup("rule");
    // 2 good and 1 bad from the owner -> net owner verdict is "good".
    store.record_feedback("R", true).unwrap();
    store.record_feedback("R", true).unwrap();
    store.record_feedback("R", false).unwrap();
    // 3 deliveries across sessions: 2 "used" (agree) and 1 "ignored" (disagree).
    for (sid, used, ignored) in [
        ("s1", vec!["R".to_string()], vec![]),
        ("s2", vec!["R".to_string()], vec![]),
        ("s3", vec![], vec!["R".to_string()]),
    ] {
        store
            .record_injection(sid, 1, 10, &[("R", Mode::Full)])
            .unwrap();
        store
            .record_agent_feedback(sid, &used, &ignored, 40)
            .unwrap();
    }
    let stats = store.stats(None, GroupBy::Kind).unwrap();
    assert_eq!(stats.agent_agree, 2);
    assert_eq!(stats.agent_disagree, 1);
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
    assert_eq!(store.schema_version().unwrap(), 5);
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
        .execute("UPDATE meta SET value='6' WHERE key='schema_version'", [])
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
