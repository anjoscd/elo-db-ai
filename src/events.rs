//! Claude Code events beyond the prompt: FileChanged and CwdChanged (2026).
//!
//! `file-changed` keeps the code map fresh: when a code file changes and a map already
//! exists, only that folder is re-synced (incremental: unchanged files are skipped by
//! mtime and size). The hook never creates a map. `cwd-changed` shows the work block of
//! the project the session just moved into. Any error is silent.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::store::DATA_DIR;

const CODE_EXTENSIONS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];

fn input(stdin: &str) -> Option<Value> {
    serde_json::from_str(stdin).ok()
}

/// Paths named by the event, absolute (relative ones are taken from `cwd`).
fn event_paths(v: &Value, cwd: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for key in ["file_path", "path", "paths", "files"] {
        let values: Vec<&str> = match &v[key] {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        out.extend(values.into_iter().map(|p| cwd.join(p)));
    }
    out
}

fn is_code(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| CODE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// `elo hook file-changed`: re-syncs the folders of changed code files when a map exists.
/// Returns how many folders were re-synced (for tests); prints nothing.
pub fn file_changed(stdin: &str) -> usize {
    let Some(v) = input(stdin) else { return 0 };
    let cwd = PathBuf::from(v["cwd"].as_str().unwrap_or("."));
    let Some(root) = crate::hook::find_root(&cwd) else {
        return 0;
    };
    sync_changed_paths(
        &root,
        event_paths(&v, &cwd)
            .into_iter()
            .filter_map(|p| contained_path(&root, &cwd, &p))
            .collect(),
    )
}

/// Refresh an existing map after a tool edits a code file. Errors are silent.
pub fn sync_changed_file(root: &Path, cwd: &Path, path: &Path) {
    if is_code(path) {
        if let Some(path) = contained_path(root, cwd, path) {
            let _ = crate::map::sync_file(root, &path);
        }
    }
}

/// Normalize an edited path and reject traversal and symlink escapes before syncing.
pub fn contained_path(root: &Path, cwd: &Path, path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let path = cwd.join(path);
    let normalized = if path.exists() {
        path.canonicalize().ok()?
    } else {
        path.parent()?.canonicalize().ok()?.join(path.file_name()?)
    };
    normalized.starts_with(root).then_some(normalized)
}

fn sync_changed_paths(root: &Path, paths: Vec<PathBuf>) -> usize {
    if !root.join(DATA_DIR).join("map.db").is_file() {
        return 0;
    }
    let Ok(canon_root) = root.canonicalize() else {
        return 0;
    };
    let folders: BTreeSet<PathBuf> = paths
        .into_iter()
        .filter(|p| is_code(p))
        .filter_map(|p| {
            let parent = p.parent()?.canonicalize().ok()?;
            parent.starts_with(&canon_root).then_some(parent)
        })
        .collect();
    folders
        .iter()
        .filter(|folder| crate::map::sync(root, Some(folder)).is_ok())
        .count()
}

/// `elo hook cwd-changed`: the work block (handoffs, tasks) of the new folder's project.
pub fn cwd_changed(stdin: &str) -> Option<String> {
    let v = input(stdin)?;
    let cwd = PathBuf::from(v["new_cwd"].as_str().or_else(|| v["cwd"].as_str())?);
    let root = crate::hook::find_root(&cwd)?;
    crate::work::session_start_block(&root, &cwd)
}
