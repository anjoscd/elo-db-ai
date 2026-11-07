//! Code map base: entities, store and queries (SPEC §7, part 1).

pub mod extract;
pub mod query;
pub mod refs;
pub mod resolve;
pub mod store;

use std::path::Path;

use serde::Serialize;

/// Kind of a code entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Function,
    Method,
    Class,
    Component,
    Callback,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Function => "function",
            Kind::Method => "method",
            Kind::Class => "class",
            Kind::Component => "component",
            Kind::Callback => "callback",
        }
    }

    pub fn parse(text: &str) -> Option<Kind> {
        Some(match text {
            "file" => Kind::File,
            "function" => Kind::Function,
            "method" => Kind::Method,
            "class" => Kind::Class,
            "component" => Kind::Component,
            "callback" => Kind::Callback,
            _ => return None,
        })
    }
}

/// One indexed entity.
#[derive(Debug, Clone, Serialize)]
pub struct Entity {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub container: Option<String>,
    #[serde(skip)]
    pub parent_id: Option<String>,
}

/// Outcome of one `map sync` run.
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncReport {
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    /// Files left out: over the size limit, binary or not valid UTF-8.
    pub skipped: usize,
    /// Files whose references were (re)resolved in this run.
    pub resolved: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MapError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Schema(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Json(String),
}

/// Index `target` (default: the whole `root`), incrementally.
pub fn sync(root: &Path, target: Option<&Path>) -> Result<SyncReport, MapError> {
    store::MapStore::open(root)?.sync(root, target)
}

/// Reindex only `file` in an existing map; a deleted file is removed.
/// Never creates or migrates a map. Relative paths are resolved from `root`.
pub fn sync_file(root: &Path, file: &Path) -> Result<(), MapError> {
    store::MapStore::sync_file(root, file)
}

/// Entities declared in `file`.
pub fn query_file(root: &Path, file: &str) -> Result<query::Report, MapError> {
    query::file(root, file)
}

/// Entities whose name contains `name`.
pub fn query_contains(root: &Path, name: &str) -> Result<query::Report, MapError> {
    query::contains(root, name)
}

/// Raw calls recorded for the entity named `owner`.
pub fn raw_calls(root: &Path, owner: &str) -> Result<Vec<(String, u32)>, MapError> {
    store::MapStore::open_read(root)?.raw_calls(owner)
}

/// Callers of `name` (SPEC §7), with unresolved same-name references.
pub fn query_calls(root: &Path, name: &str) -> Result<refs::RefReport, MapError> {
    refs::calls(root, name)
}

/// What `name` calls.
pub fn query_callees(root: &Path, name: &str) -> Result<refs::RefReport, MapError> {
    refs::callees(root, name)
}

/// Files importing `file`.
pub fn query_imports(root: &Path, file: &str) -> Result<refs::RefReport, MapError> {
    refs::imports(root, file)
}

/// Transitive callers of `name`.
pub fn query_impact(root: &Path, name: &str, depth: usize) -> Result<refs::RefReport, MapError> {
    refs::impact(root, name, depth)
}
