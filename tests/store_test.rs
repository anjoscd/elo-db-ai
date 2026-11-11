use std::path::PathBuf;

use elo_ctx::store::{NewItem, Store, StoreError, SCHEMA_VERSION};
use rusqlite::Connection;
use tempfile::TempDir;

fn database_path(dir: &TempDir) -> PathBuf {
    dir.path().join(".elo").join("elo.db")
}

fn item<'a>(id: &'a str, kind: &'a str, title: &'a str, body: &'a str) -> NewItem<'a> {
    NewItem {
        id,
        kind,
        scope: "api",
        title,
        body,
        origin: "user",
    }
}

#[test]
fn creates_a_v1_database() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = database_path(&dir);
    let store = Store::open_at(&path).expect("open store");
    assert_eq!(
        store.schema_version().expect("schema version"),
        SCHEMA_VERSION
    );
    assert!(path.is_file());
    assert_eq!(store.path(), path.as_path());
}

#[test]
fn a_newer_schema_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = database_path(&dir);
    drop(Store::open_at(&path).expect("open store"));

    let conn = Connection::open(&path).expect("raw connection");
    conn.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
        [(SCHEMA_VERSION + 1).to_string()],
    )
    .expect("bump version");
    drop(conn);

    let error = Store::open_at(&path).expect_err("newer schema must be refused");
    assert!(matches!(error, StoreError::SchemaTooNew { .. }));
}

#[test]
fn full_text_search_finds_an_inserted_item() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open_at(&database_path(&dir)).expect("open store");
    let entry = item(
        "item-1",
        "rule",
        "Never log secrets",
        "Redact tokens before logging",
    );
    store.insert_item(&entry).expect("insert item");

    let hits = store.search("redact").expect("search");
    assert_eq!(hits, vec!["item-1".to_string()]);
}

#[test]
fn an_invalid_kind_is_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open_at(&database_path(&dir)).expect("open store");
    let entry = item("item-1", "opinion", "Guessing", "Guessing is not a kind");
    assert!(store.insert_item(&entry).is_err());
}

#[test]
fn unknown_database_is_unchanged() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("unknown.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE foreign_data(value TEXT); INSERT INTO foreign_data VALUES ('keep');",
    )
    .unwrap();
    drop(conn);
    let before = std::fs::read(&path).unwrap();
    let error = Store::open_at(&path).expect_err("unknown database");
    assert!(error.to_string().contains("not an ELO store"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn existing_empty_database_is_initialized() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.db");
    drop(Connection::open(&path).unwrap());
    Store::open_at(&path).unwrap().check_schema().unwrap();
}

#[test]
fn missing_version_is_not_treated_as_version_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);")
        .unwrap();
    assert!(matches!(
        Store::open_at(&path),
        Err(StoreError::InvalidSchemaValue(_))
    ));
    let count: i64 = conn
        .query_row("SELECT count(*) FROM meta", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn migration_backs_up_committed_wal_and_rewrites_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_path(&dir);
    let store = Store::open_at(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0; UPDATE meta SET value = '0' WHERE key = 'schema_version';").unwrap();
    store
        .insert_item(&item(
            "wal-item",
            "note",
            "Snapshot",
            "Committed WAL content",
        ))
        .unwrap();

    let migrated = Store::open_at(&path).unwrap();
    assert_eq!(migrated.schema_version().unwrap(), SCHEMA_VERSION);
    migrated.check_schema().unwrap();
    let backups: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("backup-v0-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    let backup = Connection::open(&backups[0]).unwrap();
    let version: String = backup
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, "0");
    let title: String = backup
        .query_row("SELECT title FROM items WHERE id = 'wal-item'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(title, "Snapshot");
    let integrity: String = backup
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
}

#[test]
fn full_text_search_tracks_updates_and_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_at(&database_path(&dir)).unwrap();
    store
        .insert_item(&item("item-1", "note", "Original", "Body"))
        .unwrap();
    assert_eq!(store.search("Original").unwrap(), vec!["item-1"]);
    let conn = Connection::open(store.path()).unwrap();
    conn.execute(
        "UPDATE items SET title = 'Replacement' WHERE id = 'item-1'",
        [],
    )
    .unwrap();
    assert!(store.search("Original").unwrap().is_empty());
    assert_eq!(store.search("Replacement").unwrap(), vec!["item-1"]);
    conn.execute("DELETE FROM items WHERE id = 'item-1'", [])
        .unwrap();
    assert!(store.search("Replacement").unwrap().is_empty());
    // Check the index directly: a stale row would be hidden by search's JOIN.
    let count: i64 = conn
        .query_row("SELECT count(*) FROM items_fts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn v2_migrates_feedback_index_without_losing_data_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_path(&dir);
    let store = Store::open_at(&path).unwrap();
    store
        .insert_item(&item("keep", "note", "Saved", "searchable"))
        .unwrap();
    drop(store);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP INDEX feedback_item_id;
        UPDATE meta SET value='2' WHERE key='schema_version';
        INSERT INTO feedback VALUES ('keep','bad','note','global',123);
        INSERT INTO injected_events VALUES ('s',1,42,123);
        INSERT INTO injections VALUES ('s',1,'keep','pointer','note','global',123);",
    )
    .unwrap();
    drop(conn);
    for _ in 0..2 {
        let store = Store::open_at(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        store.check_schema().unwrap();
        assert_eq!(store.search("searchable").unwrap(), ["keep"]);
        let conn = Connection::open(&path).unwrap();
        let column: String = conn
            .query_row(
                "SELECT name FROM pragma_index_info('feedback_item_id')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(column, "item_id");
        for table in ["feedback", "injections", "injected_events"] {
            let count: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1);
        }
    }
    let backup = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("backup-v2-"))
        .unwrap();
    let conn = Connection::open(backup.path()).unwrap();
    let version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, "2");
}

#[test]
fn version_five_gains_optional_index_without_migration() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .conn
        .execute_batch(
            "DROP INDEX injections_unused_window;
         INSERT INTO injections VALUES ('s',1,'r','full','rule','global',1000000);",
        )
        .unwrap();
    drop(store);
    for _ in 0..2 {
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.schema_version().unwrap(), 5);
        assert_eq!(store.unused_windows(1000001, 14)["r"].full_events, 1);
        let index_count: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='injections_unused_window'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(index_count, 1);
    }
}

#[test]
fn optional_index_failure_does_not_prevent_opening() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .conn
        .execute_batch("DROP INDEX injections_unused_window; BEGIN IMMEDIATE;")
        .unwrap();
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(reopened.schema_version().unwrap(), 5);
    assert!(reopened.unused_windows(1000001, 14).is_empty());
    store.conn.execute_batch("ROLLBACK;").unwrap();
    drop(reopened);
    Store::open(dir.path()).unwrap();
}
