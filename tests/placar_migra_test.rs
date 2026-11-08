use std::fs;
use std::time::{Duration, Instant};

use elo_ctx::store::{Store, StoreError, SCHEMA_VERSION};
use tempfile::TempDir;

fn v4_store() -> TempDir {
    let dir = TempDir::new().unwrap();
    drop(Store::open(dir.path()).unwrap());
    dir
}

#[test]
fn schema_is_version_4() {
    assert_eq!(SCHEMA_VERSION, 4);
}

#[test]
fn up_to_date_store_opens_without_creating_the_lock_file() {
    let dir = v4_store();
    let lock = dir.path().join(".elo/elo.migration.lock");
    let _ = fs::remove_file(&lock);
    drop(Store::open(dir.path()).unwrap());
    assert!(
        !lock.exists(),
        "fast path must not create the migration lock"
    );
    drop(Store::open_no_migrate(dir.path()).unwrap());
}

#[cfg(unix)]
#[test]
fn up_to_date_store_opens_in_a_read_only_folder() {
    use std::os::unix::fs::PermissionsExt;
    let dir = v4_store();
    let data = dir.path().join(".elo");
    let _ = fs::remove_file(data.join("elo.migration.lock"));
    fs::set_permissions(&data, fs::Permissions::from_mode(0o555)).unwrap();
    let opened = Store::open(dir.path());
    fs::set_permissions(&data, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(opened.is_ok(), "{:?}", opened.err());
}

#[test]
fn hook_open_never_migrates_an_old_schema() {
    let dir = v4_store();
    let store = Store::open(dir.path()).unwrap();
    store
        .conn
        .execute("UPDATE meta SET value='3' WHERE key='schema_version'", [])
        .unwrap();
    drop(store);
    match Store::open_no_migrate(dir.path()) {
        Err(StoreError::MigrationPending {
            found: 3,
            supported: 4,
        }) => {}
        other => panic!("expected MigrationPending, got {:?}", other.map(|_| ())),
    }
    let check = Store::open_read_only(dir.path()).unwrap();
    let version: String = check
        .conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, "3", "the hook path left the schema alone");
}

#[test]
fn busy_migration_lock_fails_fast_instead_of_hanging() {
    let dir = v4_store();
    let store = Store::open(dir.path()).unwrap();
    store
        .conn
        .execute("UPDATE meta SET value='3' WHERE key='schema_version'", [])
        .unwrap();
    drop(store);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.path().join(".elo/elo.migration.lock"))
        .unwrap();
    lock.lock().unwrap();
    let start = Instant::now();
    let result = Store::open(dir.path());
    let waited = start.elapsed();
    assert!(
        matches!(result, Err(StoreError::MigrationBusy)),
        "{:?}",
        result.map(|_| ())
    );
    assert!(waited < Duration::from_secs(4), "waited {waited:?}");
    drop(lock);
    let migrated = Store::open(dir.path()).unwrap();
    let version: String = migrated
        .conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, "4");
}

#[test]
fn hook_with_pending_migration_is_silent_and_logs_nothing() {
    let dir = v4_store();
    let store = Store::open(dir.path()).unwrap();
    store
        .conn
        .execute("UPDATE meta SET value='3' WHERE key='schema_version'", [])
        .unwrap();
    drop(store);
    let input = serde_json::json!({"session_id":"s","cwd":dir.path(),"prompt":"qualquer coisa"})
        .to_string();
    let out = elo_ctx::hook::prepare("user-prompt-submit", &input).unwrap();
    assert!(out.is_none());
    assert!(
        !dir.path().join(".elo/shadow/errors.log").exists(),
        "pending migration is not an error"
    );
}

#[test]
fn hook_without_a_store_is_silent_and_logs_nothing() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".elo")).unwrap();
    let input = serde_json::json!({"session_id":"s","cwd":dir.path(),"prompt":"oi"}).to_string();
    assert!(elo_ctx::hook::prepare("user-prompt-submit", &input)
        .unwrap()
        .is_none());
    assert!(
        !dir.path().join(".elo/elo.db").exists(),
        "the hook never creates the store"
    );
    assert!(!dir.path().join(".elo/shadow/errors.log").exists());
}

#[test]
fn hook_while_another_writer_holds_the_store_is_silent_and_fast() {
    let dir = v4_store();
    let writer = Store::open(dir.path()).unwrap();
    writer.conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let input = serde_json::json!({"session_id":"s","cwd":dir.path(),"prompt":"oi"}).to_string();
    let start = Instant::now();
    let out = elo_ctx::hook::prepare("user-prompt-submit", &input).unwrap();
    let waited = start.elapsed();
    writer.conn.execute_batch("ROLLBACK").unwrap();
    assert!(out.is_none());
    assert!(waited < Duration::from_secs(2), "waited {waited:?}");
    assert!(
        !dir.path().join(".elo/shadow/errors.log").exists(),
        "a busy store is not an error"
    );
}

#[test]
fn check_schema_requires_the_new_indexes() {
    for index in ["auto_usage_item", "injections_session_event"] {
        let dir = v4_store();
        let store = Store::open(dir.path()).unwrap();
        store.check_schema().unwrap();
        store
            .conn
            .execute_batch(&format!("DROP INDEX {index}"))
            .unwrap();
        match store.check_schema() {
            Err(StoreError::MissingSchemaObject(name)) => assert_eq!(name, index),
            other => panic!("{index}: expected MissingSchemaObject, got {other:?}"),
        }
    }
}

#[test]
fn post_tool_use_event_keeps_only_the_named_file() {
    let dir = v4_store();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.rs"), "x").unwrap();
    fs::write(dir.path().join("src/b.rs"), "x").unwrap();
    let send = |tool: &str, input: serde_json::Value| {
        let json = serde_json::json!({
            "session_id":"s","cwd":dir.path(),"tool_name":tool,"tool_input":input
        })
        .to_string();
        assert!(elo_ctx::hook::prepare("post-tool-use", &json)
            .unwrap()
            .is_none());
    };
    let session_path = dir.path().join(".elo/sessions/s.json");
    // Edit with file_path: the event keeps exactly that file.
    send(
        "Edit",
        serde_json::json!({"file_path":"src/a.rs","old_string":"x","new_string":"y"}),
    );
    let session = fs::read_to_string(&session_path).expect("the hook writes the session state");
    assert!(session.contains("src/a.rs"), "{session}");
    // Bash naming two files: the event stays without paths (they serve the placar only).
    send("Bash", serde_json::json!({"command":"cat src/b.rs"}));
    let session = fs::read_to_string(&session_path).expect("session state");
    assert!(!session.contains("src/b.rs"), "{session}");
}
