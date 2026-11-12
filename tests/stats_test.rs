use elo_ctx::stats::{GroupBy, Mode};
use elo_ctx::store::{NewItem, Store, StoreError, SCHEMA_VERSION};
use rusqlite::Connection;
use tempfile::TempDir;

fn store_with_items(dir: &TempDir) -> Store {
    let store = Store::create_at(&dir.path().join("elo.db")).expect("open");
    for (id, kind, scope) in [
        ("L1", "law", "global"),
        ("R1", "rule", "project:api"),
        ("R2", "rule", "project:web"),
    ] {
        store
            .insert_item(&NewItem {
                id,
                kind,
                scope,
                title: id,
                body: "body",
                origin: "user",
            })
            .expect("insert");
    }
    store
}

#[test]
fn fresh_store_has_current_schema_with_scoreboard_tables() {
    let dir = TempDir::new().expect("tmp");
    let store = store_with_items(&dir);
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    store.check_schema().expect("schema complete");
}

#[test]
fn stats_count_injections_feedback_and_percentiles() {
    let dir = TempDir::new().expect("tmp");
    let mut store = store_with_items(&dir);
    assert!(store
        .record_injection("s1", 1, 100, &[("L1", Mode::Full), ("R1", Mode::Full)])
        .expect("record"));
    assert!(store
        .record_injection("s1", 2, 300, &[("R2", Mode::Pointer)])
        .expect("record"));
    store.record_feedback("R1", true).expect("good");
    store.record_feedback("R2", false).expect("bad");

    let st = store.stats(None, GroupBy::Kind).expect("stats");
    assert_eq!(st.events, 2);
    assert_eq!(st.full, 2);
    assert_eq!(st.pointers, 1);
    assert_eq!(st.laws_full, 1);
    assert_eq!(st.bytes_p50, Some(100));
    assert_eq!(st.bytes_p95, Some(300));
    assert_eq!((st.good, st.bad), (1, 1));
    assert_eq!(st.used_rate(), Some(0.5));
    assert_eq!(st.bad_rate(), Some(0.5));
    assert_eq!(st.groups["rule"].full, 1);
    assert_eq!(st.groups["rule"].pointers, 1);

    let by_scope = store.stats(None, GroupBy::Scope).expect("stats");
    assert_eq!(by_scope.groups["project:web"].bad, 1);
    assert_eq!(by_scope.groups["global"].full, 1);
}

#[test]
fn replaying_an_event_does_not_double_count() {
    let dir = TempDir::new().expect("tmp");
    let mut store = store_with_items(&dir);
    assert!(store
        .record_injection("s1", 1, 50, &[("R1", Mode::Full)])
        .expect("first"));
    assert!(!store
        .record_injection("s1", 1, 50, &[("R1", Mode::Full)])
        .expect("replay"));
    let st = store.stats(None, GroupBy::Kind).expect("stats");
    assert_eq!((st.events, st.full), (1, 1));
}

#[test]
fn unknown_items_are_rejected_without_partial_writes() {
    let dir = TempDir::new().expect("tmp");
    let mut store = store_with_items(&dir);
    let error = store
        .record_injection("s1", 1, 50, &[("R1", Mode::Full), ("NOPE", Mode::Full)])
        .expect_err("unknown item");
    assert!(matches!(error, StoreError::UnknownItem(id) if id == "NOPE"));
    let st = store.stats(None, GroupBy::Kind).expect("stats");
    assert_eq!((st.events, st.full), (0, 0), "transaction rolled back");
    assert!(matches!(
        store.record_feedback("NOPE", true),
        Err(StoreError::UnknownItem(_))
    ));
}

#[test]
fn empty_scoreboard_has_no_rates() {
    let dir = TempDir::new().expect("tmp");
    let store = store_with_items(&dir);
    let st = store.stats(None, GroupBy::Kind).expect("stats");
    assert_eq!(st.events, 0);
    assert_eq!(st.bytes_p50, None);
    assert_eq!(st.used_rate(), None);
    assert_eq!(st.bad_rate(), None);
}

#[test]
fn version_one_store_migrates_to_current_with_a_backup() {
    let dir = TempDir::new().expect("tmp");
    let path = dir.path().join("elo.db");
    drop(store_with_items(&dir));
    {
        // Downgrade to a faithful v1 file: drop the v2 tables and rewrite the version.
        let conn = Connection::open(&path).expect("raw open");
        conn.execute_batch(
            "DROP TABLE injected_events; DROP TABLE injections; DROP TABLE feedback;
             UPDATE meta SET value = '1' WHERE key = 'schema_version';",
        )
        .expect("downgrade");
    }
    let store = Store::open_at(&path).expect("migrates");
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    store.check_schema().expect("current schema complete");
    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .expect("list")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("backup-v1-"))
        .collect();
    assert_eq!(backups.len(), 1, "one v1 backup");
    let old = Connection::open(backups[0].path()).expect("backup opens");
    let items: i64 = old
        .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
        .expect("count");
    assert_eq!(items, 3, "backup keeps the v1 data");
}

#[test]
fn version_four_store_migrates_to_five_with_a_backup() {
    let dir = TempDir::new().expect("tmp");
    let path = dir.path().join("elo.db");
    drop(store_with_items(&dir));
    {
        // Downgrade to a faithful v4 file: drop the v5 objects and rewrite the version.
        let conn = Connection::open(&path).expect("raw open");
        conn.execute_batch(
            "DROP INDEX agent_feedback_item; DROP TABLE agent_feedback;
             DROP INDEX proposals_pending; DROP TABLE proposals;
             UPDATE meta SET value = '4' WHERE key = 'schema_version';",
        )
        .expect("downgrade");
    }
    let mut store = Store::open_at(&path).expect("migrates");
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    store.check_schema().expect("current schema complete");
    // The v5 objects exist and are usable right after the migration.
    store
        .record_injection("s1", 1, 10, &[("L1", Mode::Full)])
        .expect("record");
    let (accepted, rejected) = store
        .record_agent_feedback("s1", &["L1".into()], &[], 40)
        .expect("feedback");
    assert_eq!(accepted, vec!["L1".to_string()]);
    assert!(rejected.is_empty());
    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .expect("list")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("backup-v4-"))
        .collect();
    assert_eq!(backups.len(), 1, "one v4 backup");
    let old = Connection::open(backups[0].path()).expect("backup opens");
    let items: i64 = old
        .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
        .expect("count");
    assert_eq!(items, 3, "backup keeps the v4 data");
    let has_feedback_table: bool = old
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='agent_feedback')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!has_feedback_table, "the backup predates v5");
}

#[test]
fn feedback_history_survives_item_deletion() {
    let dir = TempDir::new().expect("tmp");
    let store = store_with_items(&dir);
    store.record_feedback("R1", true).expect("good");
    drop(store);
    let conn = Connection::open(dir.path().join("elo.db")).expect("raw");
    conn.execute("DELETE FROM items WHERE id = 'R1'", [])
        .expect("delete");
    drop(conn);
    let store = Store::open_at(&dir.path().join("elo.db")).expect("reopen");
    let st = store.stats(None, GroupBy::Scope).expect("stats");
    assert_eq!(st.good, 1);
    assert_eq!(st.groups["project:api"].good, 1);
}

#[test]
fn current_schema_repairs_missing_or_wrong_feedback_index() {
    for index in [
        "",
        "CREATE INDEX feedback_item_id ON feedback(kind);",
        "CREATE INDEX feedback_item_id ON items(id);",
        "CREATE INDEX feedback_item_id ON feedback(item_id) WHERE item_id = 'R1';",
    ] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("elo.db");
        let store = store_with_items(&dir);
        store.record_feedback("R1", true).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("DROP INDEX feedback_item_id;").unwrap();
        conn.execute_batch(index).unwrap();
        assert!(store.check_schema().is_err());
        drop(store);
        let store = Store::open_at(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        store.check_schema().unwrap();
        let conn = Connection::open(&path).unwrap();
        let columns: Vec<String> = conn
            .prepare("PRAGMA index_info(feedback_item_id)")
            .unwrap()
            .query_map([], |r| r.get(2))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(columns, ["item_id"]);
        assert_eq!(store.stats(None, GroupBy::Kind).unwrap().good, 1);
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("backup-v{SCHEMA_VERSION}-")))
                .count(),
            1
        );
    }
}
