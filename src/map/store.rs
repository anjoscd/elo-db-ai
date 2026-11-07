//! Separate SQLite store for the code map: `.elo/map.db`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags};
use sha2::{Digest, Sha256};

use super::extract;
use super::resolve;
use super::{Entity, Kind, MapError, SyncReport};

const SCHEMA_VERSION: i64 = 3;

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Files modified this recently are always hashed: mtime alone cannot prove them unchanged.
const RACY_WINDOW_NANOS: i64 = 2_000_000_000;

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    ".git",
    "target",
    ".elo",
    ".next",
    ".turbo",
    "coverage",
    "vendor",
];

const TABLES: &[&str] = &[
    "files",
    "entities",
    "raw_calls",
    "raw_imports",
    "raw_bindings",
    "raw_exports",
    "raw_locals",
    "edges",
    "unresolved",
    "meta",
];

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
  path TEXT PRIMARY KEY,
  mtime INTEGER NOT NULL,
  size INTEGER NOT NULL,
  hash TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS entities (
  id TEXT PRIMARY KEY,
  file TEXT NOT NULL,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  line INTEGER NOT NULL,
  container TEXT,
  parent_id TEXT
);
CREATE INDEX IF NOT EXISTS entities_file ON entities(file);
CREATE INDEX IF NOT EXISTS entities_name ON entities(name);
CREATE TABLE IF NOT EXISTS raw_calls (
  file TEXT NOT NULL,
  name TEXT NOT NULL,
  line INTEGER NOT NULL,
  owner TEXT,
  owner_id TEXT,
  shape TEXT NOT NULL,
  class TEXT
);
CREATE INDEX IF NOT EXISTS raw_calls_file ON raw_calls(file);
CREATE TABLE IF NOT EXISTS raw_imports (
  file TEXT NOT NULL,
  spec TEXT NOT NULL,
  names TEXT NOT NULL,
  line INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS raw_imports_file ON raw_imports(file);
CREATE TABLE IF NOT EXISTS raw_bindings (
  file TEXT NOT NULL,
  local TEXT NOT NULL,
  spec TEXT NOT NULL,
  imported TEXT NOT NULL,
  kind TEXT NOT NULL,
  line INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS raw_bindings_file ON raw_bindings(file);
CREATE TABLE IF NOT EXISTS raw_exports (
  file TEXT NOT NULL,
  exported TEXT NOT NULL,
  local TEXT,
  spec TEXT,
  kind TEXT NOT NULL,
  line INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS raw_exports_file ON raw_exports(file);
CREATE TABLE IF NOT EXISTS raw_locals (
  file TEXT NOT NULL,
  scope_id TEXT NOT NULL,
  name TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS raw_locals_file ON raw_locals(file);
CREATE TABLE IF NOT EXISTS edges (
  kind TEXT NOT NULL,
  src_id TEXT NOT NULL,
  src_file TEXT NOT NULL,
  dst_id TEXT NOT NULL,
  dst_file TEXT NOT NULL,
  name TEXT NOT NULL,
  line INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS edges_src_file ON edges(src_file);
CREATE INDEX IF NOT EXISTS edges_dst_id ON edges(dst_id);
CREATE INDEX IF NOT EXISTS edges_dst_file ON edges(dst_file);
CREATE INDEX IF NOT EXISTS edges_src_id ON edges(src_id);
CREATE TABLE IF NOT EXISTS unresolved (
  file TEXT NOT NULL,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  line INTEGER NOT NULL,
  owner_id TEXT,
  reason TEXT NOT NULL,
  detail TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS unresolved_file ON unresolved(file);
CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";

pub struct MapStore {
    conn: Connection,
}

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

impl MapStore {
    pub fn open(root: &Path) -> Result<Self, MapError> {
        let dir = root.join(".elo");
        fs::create_dir_all(&dir)?;
        let conn = Connection::open(dir.join("map.db"))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let version: i64 = conn.query_row("PRAGMA user_version", params![], |row| row.get(0))?;
        if !(0..=SCHEMA_VERSION).contains(&version) {
            return Err(MapError::Schema(format!(
                "map.db has schema v{version}, expected v{SCHEMA_VERSION}"
            )));
        }
        if version != SCHEMA_VERSION {
            // The map is a rebuildable cache: older schemas are dropped and reindexed.
            for table in TABLES {
                conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
            }
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self { conn })
    }

    /// Opens the map for queries only; creates or migrates it when it is missing or old.
    pub fn open_read(root: &Path) -> Result<Self, MapError> {
        let path = root.join(".elo").join("map.db");
        if !path.is_file() {
            return Self::open(root);
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let version: i64 = conn.query_row("PRAGMA user_version", params![], |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            drop(conn);
            return Self::open(root);
        }
        Ok(Self { conn })
    }

    pub(super) fn db(&self) -> &Connection {
        &self.conn
    }

    /// Hook update: no directory traversal, database creation, migration or busy wait.
    pub fn sync_file(root: &Path, file: &Path) -> Result<(), MapError> {
        let root = fs::canonicalize(root)?;
        let path = root.join(file);
        let name = path
            .file_name()
            .ok_or_else(|| MapError::Invalid("target has no name".into()))?;
        let parent = fs::canonicalize(path.parent().unwrap_or(&root))?;
        let path = parent.join(name);
        if !path.starts_with(&root) || !extract::is_supported(&path.to_string_lossy()) {
            return Err(MapError::Invalid(
                "file outside the root or unsupported".into(),
            ));
        }
        let rel = relative(&root, &path);
        let db_path = root.join(crate::store::DATA_DIR).join("map.db");
        if !db_path.is_file() {
            // No map yet: nothing to update, and no file read for nothing.
            return Err(MapError::Invalid("no map yet".into()));
        }
        let source = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
                    return Err(MapError::Invalid("file cannot be indexed".into()));
                }
                let source = fs::read_to_string(&path)?;
                if source.as_bytes().contains(&0) {
                    return Err(MapError::Invalid("binary file".into()));
                }
                Some((metadata, source))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::ZERO)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(MapError::Schema(format!(
                "map.db has schema v{version}, expected v{SCHEMA_VERSION}"
            )));
        }
        let store = Self { conn };
        let tx = store.conn.unchecked_transaction()?;
        if let Some((metadata, source)) = source {
            store.reindex(
                &rel,
                mtime_nanos(&metadata),
                metadata.len() as i64,
                &hash_of(source.as_bytes()),
                &source,
            )?;
            resolve::run(
                &store.conn,
                &root,
                &resolve::Plan {
                    changed: vec![rel],
                    removed: vec![],
                    dependents: vec![],
                    export_names: vec![],
                    membership_changed: false,
                    everything: false,
                },
            )?;
        } else {
            store.forget(&rel)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn sync(&mut self, root: &Path, target: Option<&Path>) -> Result<SyncReport, MapError> {
        let root = fs::canonicalize(root)?;
        let base = resolve_base(&root, target)?;
        let scope = scope_prefix(&root, &base);
        if let Err(error) = fs::read_dir(&base) {
            return Err(MapError::Invalid(format!(
                "could not read folder {}: {error}",
                base.display()
            )));
        }
        let mut report = SyncReport::default();
        let prior = self.stored_files()?;
        let tx = self.conn.unchecked_transaction()?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut changed: Vec<String> = Vec::new();
        let mut export_names: HashSet<String> = HashSet::new();
        let mut membership_changed = false;
        let mut configs: Vec<(String, String)> = Vec::new();
        let mut unreadable: Vec<String> = Vec::new();
        let mut visited: HashSet<PathBuf> = HashSet::new();
        let now = now_nanos();
        let mut stack = vec![base];
        while let Some(dir) = stack.pop() {
            let Ok(real) = fs::canonicalize(&dir) else {
                unreadable.push(format!("{}/", relative(&root, &dir)));
                continue;
            };
            if !visited.insert(real) {
                continue;
            }
            let Ok(entries) = fs::read_dir(&dir) else {
                unreadable.push(format!("{}/", relative(&root, &dir)));
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(metadata) = fs::symlink_metadata(&path) else {
                    continue;
                };
                // links could escape the root or loop; only real files and folders are indexed
                if metadata.file_type().is_symlink() {
                    continue;
                }
                if metadata.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_lowercase();
                    if !SKIP_DIRS.contains(&name.as_str()) {
                        stack.push(path);
                    }
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }
                if resolve::is_config_name(&entry.file_name().to_string_lossy()) {
                    if metadata.len() <= MAX_FILE_BYTES {
                        if let Ok(bytes) = fs::read(&path) {
                            configs.push((relative(&root, &path), hash_of(&bytes)));
                        }
                    }
                    continue;
                }
                if !extract::is_supported(&path.to_string_lossy()) {
                    continue;
                }
                let rel = relative(&root, &path);
                let mtime = mtime_nanos(&metadata);
                let size = metadata.len() as i64;
                if metadata.len() > MAX_FILE_BYTES {
                    report.skipped += 1;
                    continue;
                }
                let before = prior.get(&rel).cloned();
                let racy = now.saturating_sub(mtime) < RACY_WINDOW_NANOS;
                if let Some((mtime_before, size_before, _)) = &before {
                    if *mtime_before == mtime && *size_before == size && !racy {
                        seen.insert(rel);
                        report.unchanged += 1;
                        continue;
                    }
                }
                let Ok(bytes) = fs::read(&path) else {
                    continue;
                };
                let source = match String::from_utf8(bytes) {
                    Ok(text) if !text.as_bytes().contains(&0) => text,
                    _ => {
                        report.skipped += 1;
                        continue;
                    }
                };
                let hash = hash_of(source.as_bytes());
                seen.insert(rel.clone());
                if let Some((_, _, hash_before)) = &before {
                    if *hash_before == hash {
                        self.conn.execute(
                            "UPDATE files SET mtime = ?2, size = ?3 WHERE path = ?1",
                            params![rel, mtime, size],
                        )?;
                        report.unchanged += 1;
                        continue;
                    }
                } else {
                    membership_changed = true;
                }
                let old_exports = self.export_names(&rel)?;
                self.reindex(&rel, mtime, size, &hash, &source)?;
                let new_exports = self.export_names(&rel)?;
                export_names.extend(old_exports.symmetric_difference(&new_exports).cloned());
                changed.push(rel);
                report.indexed += 1;
            }
        }
        let mut removed: Vec<String> = prior
            .keys()
            .filter(|path| {
                path.starts_with(&scope)
                    && !seen.contains(*path)
                    && !unreadable.iter().any(|dir| path.starts_with(dir.as_str()))
            })
            .cloned()
            .collect();
        removed.sort();
        let touched: Vec<String> = changed.iter().chain(removed.iter()).cloned().collect();
        let dependents = resolve::dependents_of(&self.conn, &touched)?;
        for path in &removed {
            export_names.extend(self.export_names(path)?);
            self.forget(path)?;
            membership_changed = true;
            report.removed += 1;
        }
        let configs_changed = self.record_configs(&scope, &configs)?;
        let plan = resolve::Plan {
            changed,
            removed,
            dependents,
            export_names: export_names.into_iter().collect(),
            membership_changed,
            everything: configs_changed,
        };
        report.resolved = resolve::run(&self.conn, &root, &plan)?;
        tx.commit()?;
        Ok(report)
    }

    /// path -> (mtime, size, hash) of every indexed file.
    fn stored_files(&self) -> Result<HashMap<String, (i64, i64, String)>, MapError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime, size, hash FROM files")?;
        let rows = stmt.query_map(params![], |row| {
            Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (path, value) = row?;
            out.insert(path, value);
        }
        Ok(out)
    }

    fn export_names(&self, file: &str) -> Result<HashSet<String>, MapError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT exported FROM raw_exports WHERE file = ?1")?;
        let rows = stmt.query_map(params![file], |row| row.get::<_, String>(0))?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    /// Stores the hash of every tsconfig seen; true when any differs from the last sync.
    fn record_configs(&self, scope: &str, seen: &[(String, String)]) -> Result<bool, MapError> {
        let mut changed = false;
        let mut previous: Vec<(String, String)> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT key, value FROM meta WHERE key LIKE 'config:%'")?;
            let rows = stmt.query_map(params![], |row| Ok((row.get(0)?, row.get(1)?)))?;
            for row in rows {
                previous.push(row?);
            }
        }
        for (path, hash) in seen {
            let key = format!("config:{path}");
            let before = previous.iter().find(|(k, _)| *k == key);
            if before.map(|(_, v)| v) != Some(hash) {
                changed = true;
                self.conn.execute(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
                    params![key, hash],
                )?;
            }
        }
        for (key, _) in &previous {
            let path = key.trim_start_matches("config:");
            if path.starts_with(scope) && !seen.iter().any(|(p, _)| p == path) {
                changed = true;
                self.conn
                    .execute("DELETE FROM meta WHERE key = ?1", params![key])?;
            }
        }
        Ok(changed)
    }

    fn reindex(
        &self,
        rel: &str,
        mtime: i64,
        size: i64,
        hash: &str,
        source: &str,
    ) -> Result<(), MapError> {
        let extracted = extract::parse(rel, source);
        let tx = &self.conn;
        for table in [
            "entities",
            "raw_calls",
            "raw_imports",
            "raw_bindings",
            "raw_exports",
            "raw_locals",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE file = ?1"),
                params![rel],
            )?;
        }
        if let Some(extracted) = extracted {
            {
                let mut insert = tx.prepare_cached(
                    "INSERT OR REPLACE INTO entities (id, file, kind, name, line, container, parent_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for entity in &extracted.entities {
                    insert.execute(params![
                        entity.id,
                        entity.file,
                        entity.kind.as_str(),
                        entity.name,
                        entity.line as i64,
                        entity.container,
                        entity.parent_id
                    ])?;
                }
            }
            {
                let mut insert = tx.prepare_cached(
                    "INSERT INTO raw_calls (file, name, line, owner, owner_id, shape, class) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for call in &extracted.calls {
                    insert.execute(params![
                        rel,
                        call.name,
                        call.line as i64,
                        call.owner,
                        call.owner_id,
                        call.shape,
                        call.class
                    ])?;
                }
            }
            for import in &extracted.imports {
                tx.execute(
                    "INSERT INTO raw_imports (file, spec, names, line) VALUES (?1, ?2, ?3, ?4)",
                    params![rel, import.spec, import.names.join(","), import.line as i64],
                )?;
                for binding in &import.bindings {
                    tx.execute(
                        "INSERT INTO raw_bindings (file, local, spec, imported, kind, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            rel,
                            binding.local,
                            import.spec,
                            binding.imported,
                            binding.kind,
                            import.line as i64
                        ],
                    )?;
                }
            }
            for export in &extracted.exports {
                tx.execute(
                    "INSERT INTO raw_exports (file, exported, local, spec, kind, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        rel,
                        export.exported,
                        export.local,
                        export.spec,
                        export.kind,
                        export.line as i64
                    ],
                )?;
            }
            for local in &extracted.locals {
                tx.execute(
                    "INSERT INTO raw_locals (file, scope_id, name) VALUES (?1, ?2, ?3)",
                    params![rel, local.scope_id, local.name],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO files (path, mtime, size, hash) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(path) DO UPDATE SET mtime = ?2, size = ?3, hash = ?4",
            params![rel, mtime, size, hash],
        )?;
        Ok(())
    }

    fn forget(&self, path: &str) -> Result<(), MapError> {
        for table in [
            "entities",
            "raw_calls",
            "raw_imports",
            "raw_bindings",
            "raw_exports",
            "raw_locals",
            "unresolved",
        ] {
            self.conn.execute(
                &format!("DELETE FROM {table} WHERE file = ?1"),
                params![path],
            )?;
        }
        self.conn.execute(
            "DELETE FROM edges WHERE src_file = ?1 OR dst_file = ?1",
            params![path],
        )?;
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn entities_for_file(&self, file: &str) -> Result<Vec<Entity>, MapError> {
        let mut hits = self.query_entities(
            "SELECT id, kind, name, file, line, container FROM entities WHERE file = ?1 ORDER BY line LIMIT 1000",
            params![file],
        )?;
        if hits.is_empty() {
            hits = self.query_entities(
                "SELECT id, kind, name, file, line, container FROM entities WHERE file LIKE ?1 ESCAPE '\\' ORDER BY file, line LIMIT 1000",
                params![format!("%/{}", like_escape(file))],
            )?;
        }
        Ok(hits)
    }

    pub fn entities_containing(&self, name: &str) -> Result<Vec<Entity>, MapError> {
        self.query_entities(
            "SELECT id, kind, name, file, line, container FROM entities WHERE name LIKE ?1 ESCAPE '\\' ORDER BY file, line LIMIT 1000",
            params![format!("%{}%", like_escape(name))],
        )
    }

    /// Raw calls of the entity `owner`; a callback also matches without its `@L<line>` suffix.
    pub fn raw_calls(&self, owner: &str) -> Result<Vec<(String, u32)>, MapError> {
        let mut stmt = self.conn.prepare(
            "SELECT name, line FROM raw_calls WHERE owner = ?1 OR owner LIKE ?2 ESCAPE '\\' ORDER BY file, line",
        )?;
        let rows = stmt.query_map(
            params![owner, format!("{}@L%", like_escape(owner))],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (name, line) = row?;
            out.push((name, line as u32));
        }
        Ok(out)
    }

    /// Which of `files` changed on disk since they were indexed (mtime and size only).
    pub fn stale_among(&self, root: &Path, files: &[String]) -> Result<Vec<String>, MapError> {
        let mut out = Vec::new();
        let mut stmt = self
            .conn
            .prepare_cached("SELECT mtime, size FROM files WHERE path = ?1")?;
        for file in files {
            let mut rows = stmt.query_map(params![file], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?;
            let Some(row) = rows.next() else { continue };
            let (mtime, size) = row?;
            if !is_fresh(&root.join(file), mtime, size) {
                out.push(file.clone());
            }
        }
        Ok(out)
    }

    /// (indexed files, files whose mtime or size differ from the index); never reads file contents.
    pub fn counts(&self, root: &Path) -> Result<(usize, usize), MapError> {
        let mut stmt = self.conn.prepare("SELECT path, mtime, size FROM files")?;
        let rows = stmt.query_map(params![], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut indexed = 0;
        let mut stale = 0;
        for row in rows {
            let (path, mtime, size) = row?;
            indexed += 1;
            if !is_fresh(&root.join(&path), mtime, size) {
                stale += 1;
            }
        }
        Ok((indexed, stale))
    }

    fn query_entities(
        &self,
        sql: &str,
        args: impl rusqlite::Params,
    ) -> Result<Vec<Entity>, MapError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(args, entity_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

fn is_fresh(path: &Path, mtime: i64, size: i64) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => mtime_nanos(&metadata) == mtime && metadata.len() as i64 == size,
        Err(_) => false,
    }
}

/// Escapes `%`, `_` and `\` for a `LIKE ... ESCAPE '\'` pattern.
pub(super) fn like_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn entity_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entity> {
    let kind: String = row.get(1)?;
    let line: i64 = row.get(4)?;
    Ok(Entity {
        id: row.get(0)?,
        kind: Kind::parse(&kind).unwrap_or(Kind::Function),
        name: row.get(2)?,
        file: row.get(3)?,
        line: line as u32,
        container: row.get(5)?,
        parent_id: None,
    })
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The sync folder as an absolute path inside `root`; errors when it escapes the root.
fn resolve_base(root: &Path, target: Option<&Path>) -> Result<PathBuf, MapError> {
    let Some(target) = target.filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(root.to_path_buf());
    };
    let shown = target.display();
    let canonical = fs::canonicalize(root.join(target)).map_err(|error| {
        MapError::Invalid(format!("target '{shown}' could not be opened: {error}"))
    })?;
    if canonical.strip_prefix(root).is_err() {
        return Err(MapError::Invalid(format!(
            "target '{shown}' is outside the project root ({})",
            root.display()
        )));
    }
    if !canonical.is_dir() {
        return Err(MapError::Invalid(format!(
            "target '{shown}' is not a folder"
        )));
    }
    Ok(canonical)
}

/// Prefix (`dir/`) that the paths under `base` share; empty for the whole root.
fn scope_prefix(root: &Path, base: &Path) -> String {
    let rel = relative(root, base);
    if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    }
}

fn mtime_nanos(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|delta| delta.as_nanos() as i64)
        .unwrap_or(0)
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_nanos() as i64)
        .unwrap_or(0)
}

fn hash_of(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for &byte in digest.iter() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
