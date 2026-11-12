//! SQLite store for ELO items.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

/// Directory, relative to the workspace root, that holds the database.
pub const DATA_DIR: &str = ".elo";

/// Database file name inside [`DATA_DIR`].
pub const DB_FILE: &str = "elo.db";

/// Schema version written by this build.
pub const SCHEMA_VERSION: i64 = 5;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("unknown item id: {0}")]
    UnknownItem(String),
    #[error("cannot write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("not an ELO store")]
    NotEloStore,
    #[error("missing schema object: {0}")]
    MissingSchemaObject(&'static str),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("schema version {found} needs migration to {supported}: run `elo doctor`")]
    MigrationPending { found: i64, supported: i64 },
    #[error("another elo is migrating the store; try again in a moment")]
    MigrationBusy,
    #[error("schema version {found} is newer than supported {supported}; upgrade elo")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("meta.schema_version holds an invalid value: {0}")]
    InvalidSchemaValue(String),
}

/// Row to insert into `items`.
#[derive(Debug, Clone)]
pub struct NewItem<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub scope: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub origin: &'a str,
}

#[derive(Debug)]
pub struct Store {
    pub conn: Connection,
    pub(crate) path: PathBuf,
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS items (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL CHECK (kind IN ('law', 'rule', 'decision', 'note', 'pitfall')),
    scope         TEXT NOT NULL,
    title         TEXT NOT NULL,
    body          TEXT NOT NULL,
    triggers      TEXT NOT NULL DEFAULT '[]',
    importance    REAL NOT NULL DEFAULT 0.5 CHECK (importance >= 0.0 AND importance <= 1.0),
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    expires_at    INTEGER NULL,
    status        TEXT NOT NULL DEFAULT 'active'
                  CHECK (status IN ('active', 'superseded', 'expired', 'archived')),
    superseded_by TEXT NULL,
    origin        TEXT NOT NULL CHECK (origin IN ('user', 'agent', 'import')),
    evidence      TEXT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(title, body);

CREATE TRIGGER IF NOT EXISTS items_after_insert AFTER INSERT ON items BEGIN
    INSERT INTO items_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body);
END;

CREATE TRIGGER IF NOT EXISTS items_after_delete AFTER DELETE ON items BEGIN
    DELETE FROM items_fts WHERE rowid = old.rowid;
END;

CREATE TRIGGER IF NOT EXISTS items_after_update AFTER UPDATE ON items BEGIN
    DELETE FROM items_fts WHERE rowid = old.rowid;
    INSERT INTO items_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body);
END;
"#;

/// Version 2: append-only scoreboard (SPEC §6.5).
pub(crate) const V2_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS injected_events (
    session_id TEXT NOT NULL,
    event_seq  INTEGER NOT NULL,
    used_bytes INTEGER NOT NULL CHECK (used_bytes >= 0),
    at         INTEGER NOT NULL,
    PRIMARY KEY (session_id, event_seq)
);

CREATE TABLE IF NOT EXISTS injections (
    session_id TEXT NOT NULL,
    event_seq  INTEGER NOT NULL,
    item_id    TEXT NOT NULL,
    mode       TEXT NOT NULL CHECK (mode IN ('full', 'pointer')),
    kind       TEXT NOT NULL,
    scope      TEXT NOT NULL,
    at         INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS feedback (
    item_id TEXT NOT NULL,
    verdict TEXT NOT NULL CHECK (verdict IN ('good', 'bad')),
    kind    TEXT NOT NULL,
    scope   TEXT NOT NULL,
    at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS injected_events_at ON injected_events (at);
CREATE INDEX IF NOT EXISTS injections_at ON injections (at);
CREATE INDEX IF NOT EXISTS feedback_at ON feedback (at);
"#;

const V3_SQL: &str = "CREATE INDEX IF NOT EXISTS feedback_item_id ON feedback (item_id);";

const V4_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS auto_usage (
 session_id TEXT NOT NULL, event_seq INTEGER NOT NULL, item_id TEXT NOT NULL,
 kind TEXT NOT NULL, scope TEXT NOT NULL, at INTEGER NOT NULL,
 PRIMARY KEY(session_id, event_seq, item_id)
);
CREATE INDEX IF NOT EXISTS auto_usage_item ON auto_usage(item_id);
CREATE INDEX IF NOT EXISTS injections_session_event ON injections(session_id, event_seq);
"#;

const V5_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS agent_feedback (
    session_id TEXT NOT NULL,
    event_seq INTEGER NOT NULL,
    item_id TEXT NOT NULL,
    verdict TEXT NOT NULL CHECK (verdict IN ('used', 'ignored')),
    corroborated INTEGER NOT NULL DEFAULT 0,
    at INTEGER NOT NULL,
    PRIMARY KEY (session_id, event_seq, item_id)
);

CREATE INDEX IF NOT EXISTS agent_feedback_item ON agent_feedback(item_id);

CREATE TABLE IF NOT EXISTS proposals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    op TEXT NOT NULL,
    item_id TEXT,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    applied_at INTEGER
);

CREATE INDEX IF NOT EXISTS proposals_pending ON proposals(applied_at, created_at);
"#;

// Cover full, demotable deliveries without reading injection rows from the table.
const UNUSED_INDEX_SQL: &str = "
CREATE INDEX IF NOT EXISTS injections_unused_window
ON injections(mode, at, item_id, session_id, event_seq)
WHERE kind IN ('decision','rule','note');
";

impl Store {
    /// Opens the existing `<root>/.elo/elo.db`, migrating when needed.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        Self::open_at(&data_dir(root).join(DB_FILE))
    }

    /// Creates a store explicitly, or opens and migrates an existing one.
    pub fn create(root: &Path) -> Result<Self, StoreError> {
        Self::create_at(&data_dir(root).join(DB_FILE))
    }

    /// Creates a database at an explicit path.
    pub fn create_at(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        if path.exists() {
            return Self::open_at(path);
        }
        Self::open_locked(path, true)
    }

    /// Opens an existing database, preserving schema migrations and backups.
    pub fn open_at(path: &Path) -> Result<Self, StoreError> {
        // Fast path: an up-to-date store opens without the migration lock (works on a
        // read-only folder and never makes concurrent hooks wait).
        {
            let conn =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            if read_schema_version(&conn)? == Some(SCHEMA_VERSION) && feedback_index_valid(&conn)? {
                ensure_optional_indexes(&conn);
                return Ok(Self {
                    conn,
                    path: path.to_path_buf(),
                });
            }
        }
        Self::open_locked(path, false)
    }

    fn open_locked(path: &Path, create: bool) -> Result<Self, StoreError> {
        let _guard = migration_lock(path)?;
        let mut flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE;
        if create {
            flags |= rusqlite::OpenFlags::SQLITE_OPEN_CREATE;
        }
        let mut conn = Connection::open_with_flags(path, flags)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        match read_schema_version(&conn)? {
            Some(found) if found > SCHEMA_VERSION => {
                return Err(StoreError::SchemaTooNew {
                    found,
                    supported: SCHEMA_VERSION,
                });
            }
            Some(version) if version < SCHEMA_VERSION => {
                backup(&conn, path, version)?;
                migrate(&mut conn, version)?;
            }
            Some(version) => {
                if !feedback_index_valid(&conn)? {
                    backup(&conn, path, version)?;
                    migrate(&mut conn, version)?;
                }
            }
            None => initialize(&mut conn)?,
        }
        ensure_optional_indexes(&conn);
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    /// Opens an existing, up-to-date store for the hooks: never creates, migrates or waits
    /// on the migration lock. An older schema answers `MigrationPending`.
    pub fn open_no_migrate(root: &Path) -> Result<Self, StoreError> {
        let path = data_dir(root).join(DB_FILE);
        if !path.is_file() {
            return Err(StoreError::NotEloStore);
        }
        let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        // A hook must never freeze the prompt behind a long write.
        conn.busy_timeout(std::time::Duration::from_millis(200))?;
        match read_schema_version(&conn)? {
            Some(SCHEMA_VERSION) => Ok(Self { conn, path }),
            Some(found) if found > SCHEMA_VERSION => Err(StoreError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            }),
            Some(found) => Err(StoreError::MigrationPending {
                found,
                supported: SCHEMA_VERSION,
            }),
            None => Err(StoreError::NotEloStore),
        }
    }

    /// Opens the existing database without creating, migrating or writing data.
    pub fn open_read_only(root: &Path) -> Result<Self, StoreError> {
        let path = root.join(DATA_DIR).join(DB_FILE);
        let absolute = fs::canonicalize(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        let encoded: String = absolute
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .map(|byte| format!("%{byte:02X}"))
            .collect();
        let conn = Connection::open_with_flags(
            format!("file:{encoded}?mode=ro"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self { conn, path };
        let version = store.schema_version()?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version < 1 {
            return Err(StoreError::InvalidSchemaValue(version.to_string()));
        }
        // SELECT only: old stores need no migration or scoreboard to be read.
        store.conn.prepare(
            "SELECT id, kind, scope, title, body, triggers, importance,
            created_at, updated_at, expires_at, status, superseded_by, origin FROM items",
        )?;
        Ok(store)
    }

    /// Path of the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Schema version stored in the file.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        let value: String = self.conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )?;
        value
            .trim()
            .parse::<i64>()
            .map_err(|_| StoreError::InvalidSchemaValue(value))
    }

    /// Reads a value from the meta table, if it exists.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(StoreError::from)
    }

    /// Writes a value to the meta table.
    pub fn meta_set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Checks the version and every required table and FTS trigger.
    pub fn check_schema(&self) -> Result<(), StoreError> {
        let version = self.schema_version()?;
        if version != SCHEMA_VERSION {
            return Err(StoreError::InvalidSchemaValue(version.to_string()));
        }
        for (kind, name) in [
            ("table", "meta"),
            ("table", "items"),
            ("table", "items_fts"),
            ("trigger", "items_after_insert"),
            ("trigger", "items_after_update"),
            ("trigger", "items_after_delete"),
            ("table", "injected_events"),
            ("table", "injections"),
            ("table", "feedback"),
            ("table", "auto_usage"),
            ("index", "feedback_item_id"),
            ("index", "auto_usage_item"),
            ("index", "injections_session_event"),
            ("table", "agent_feedback"),
            ("table", "proposals"),
            ("index", "agent_feedback_item"),
            ("index", "proposals_pending"),
        ] {
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2)",
                [kind, name],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(StoreError::MissingSchemaObject(name));
            }
        }
        if !feedback_index_valid(&self.conn)? {
            return Err(StoreError::MissingSchemaObject("feedback_item_id"));
        }
        Ok(())
    }

    /// Inserts one item; the triggers keep the full text index in sync.
    pub fn insert_item(&self, item: &NewItem<'_>) -> Result<(), StoreError> {
        let now = now_seconds();
        self.conn.execute(
            "INSERT INTO items (id, kind, scope, title, body, triggers, importance, created_at, \
             updated_at, expires_at, status, superseded_by, origin, evidence) \
             VALUES (?1, ?2, ?3, ?4, ?5, '[]', 0.5, ?6, ?6, NULL, 'active', NULL, ?7, NULL)",
            rusqlite::params![
                item.id,
                item.kind,
                item.scope,
                item.title,
                item.body,
                now,
                item.origin,
            ],
        )?;
        Ok(())
    }

    /// Full text search over titles and bodies; returns ids by relevance.
    pub fn search(&self, query: &str) -> Result<Vec<String>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT items.id FROM items_fts JOIN items ON items.rowid = items_fts.rowid \
             WHERE items_fts MATCH ?1 ORDER BY rank",
        )?;
        let rows = statement.query_map([query], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        Ok(ids)
    }
}

/// `<root>/.elo`.
pub fn data_dir(root: &Path) -> PathBuf {
    root.join(DATA_DIR)
}

fn read_schema_version(conn: &Connection) -> Result<Option<i64>, StoreError> {
    let meta: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if meta.is_none() {
        let has_tables: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table')",
            [],
            |row| row.get(0),
        )?;
        if has_tables {
            return Err(StoreError::NotEloStore);
        }
        return Ok(None);
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match value {
        None => Err(StoreError::InvalidSchemaValue(
            "missing schema_version".into(),
        )),
        Some(value) => match value.trim().parse::<i64>() {
            Ok(version) => Ok(Some(version)),
            Err(_) => Err(StoreError::InvalidSchemaValue(value)),
        },
    }
}

/// Takes a consistent snapshot, including committed WAL contents.
fn backup(conn: &Connection, path: &Path, version: i64) -> Result<(), StoreError> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let target = directory.join(format!(
        "backup-v{version}-{unix_ms}-{}.db",
        std::process::id()
    ));
    // Reserve the name exclusively; VACUUM INTO accepts an empty destination.
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|source| StoreError::Io {
            path: target.clone(),
            source,
        })?;
    drop(file);
    conn.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])?;
    Ok(())
}

/// Initializes only a store with no tables.
fn initialize(conn: &mut Connection) -> Result<(), StoreError> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_SQL)?;
    transaction.execute_batch(V2_SQL)?;
    transaction.execute_batch(V3_SQL)?;
    transaction.execute_batch(V4_SQL)?;
    transaction.execute_batch(V5_SQL)?;
    write_version(&transaction)?;
    transaction.commit()?;
    ensure_optional_indexes(conn);
    Ok(())
}

/// Applies pending migrations after the backup, without a version-zero schema.
fn migrate(conn: &mut Connection, from: i64) -> Result<(), StoreError> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if from < 2 {
        transaction.execute_batch(V2_SQL)?;
    }
    if !feedback_index_valid(&transaction)? {
        transaction.execute_batch("DROP INDEX IF EXISTS feedback_item_id;")?;
        transaction.execute_batch(V3_SQL)?;
    }
    transaction.execute_batch(V4_SQL)?;
    if from < 5 {
        transaction.execute_batch(V5_SQL)?;
    }
    write_version(&transaction)?;
    transaction.commit()?;
    Ok(())
}

// Optional acceleration must never delay opening behind a writer or reject a store.
fn ensure_optional_indexes(conn: &Connection) {
    let _ = conn.busy_timeout(std::time::Duration::ZERO);
    let _ = conn.execute_batch(UNUSED_INDEX_SQL);
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
}

fn feedback_index_valid(conn: &Connection) -> Result<bool, StoreError> {
    let correct_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_index_list('feedback') WHERE name = 'feedback_item_id' AND partial = 0 AND [unique] = 0)",
        [], |row| row.get(0))?;
    let columns = conn
        .prepare("PRAGMA index_info(feedback_item_id)")?
        .query_map([], |row| row.get::<_, Option<String>>(2))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(correct_table && columns == [Some("item_id".into())])
}

fn write_version(conn: &Connection) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

pub fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Exclusive lock for schema migrations, with a 2 s limit instead of waiting forever.
fn migration_lock(path: &Path) -> Result<fs::File, StoreError> {
    let lock_path = path.with_extension("migration.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|source| StoreError::Io {
            path: lock_path.clone(),
            source,
        })?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(fs::TryLockError::WouldBlock) => return Err(StoreError::MigrationBusy),
            Err(fs::TryLockError::Error(source)) => {
                return Err(StoreError::Io {
                    path: lock_path,
                    source,
                })
            }
        }
    }
}
