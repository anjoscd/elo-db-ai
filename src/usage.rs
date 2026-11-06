//! Automatic usefulness is evidence from tool inputs, never an owner's verdict.
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::bench::fixture::Triggers;
use crate::store::{now_seconds, StoreError, DATA_DIR, DB_FILE, SCHEMA_VERSION};

/// Each tool call is two events (pre and post), so 7 covers about three tool calls.
pub const DEFAULT_WINDOW: u64 = 7;

#[derive(Default, Debug)]
pub struct Touches {
    pub paths: Vec<String>,
    pub identifiers: BTreeSet<String>,
}

fn identifier(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
}

/// Keywords imported from BASE are often file names (`guard.rs`, `src/hook.rs`).
fn file_like(word: &str) -> bool {
    !word.contains(char::is_whitespace)
        && (word.contains('/')
            || word.rsplit_once('.').is_some_and(|(stem, ext)| {
                !stem.is_empty() && !ext.is_empty() && ext.chars().all(char::is_alphanumeric)
            }))
}

/// `path` ends with the whole `suffix` component(s): `src/guard.rs` matches `guard.rs`,
/// `myguard.rs` does not.
fn path_ends_with(path: &str, suffix: &str) -> bool {
    let suffix = suffix.trim_start_matches("./").trim_start_matches('/');
    path == suffix || path.ends_with(&format!("/{suffix}"))
}

fn normalize(root: &Path, cwd: &Path, value: &str, existing: bool) -> Option<String> {
    let path = cwd.join(value);
    if existing && !path.is_file() {
        return None;
    }
    // Resolve existing ancestors as well: a new file inside an escaping symlink
    // must not turn into a workspace touch.
    let mut ancestor = path.as_path();
    let mut suffix = Vec::new();
    let mut resolved = loop {
        if let Ok(path) = ancestor.canonicalize() {
            break path;
        }
        suffix.push(ancestor.file_name()?);
        ancestor = ancestor.parent()?;
    };
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    let mut clean = PathBuf::new();
    for part in resolved.components() {
        match part {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    let root = root.canonicalize().ok()?;
    let relative = clean.strip_prefix(root).ok()?;
    (!relative.as_os_str().is_empty()).then(|| relative.to_string_lossy().replace('\\', "/"))
}

/// Small shell lexer: quotes/escapes are supported; nothing is evaluated.
fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for c in command.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if quote == Some(c) {
            quote = None;
            continue;
        }
        if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
            continue;
        }
        if quote.is_none() && (c.is_whitespace() || ";|&()<>=`".contains(c)) {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

pub fn extract(root: &Path, cwd: &Path, tool: &str, input: &Value) -> Touches {
    let mut touched = Touches::default();
    // Only file tools touch what their path names; `Grep`/`Glob` with `path` only search.
    let file_tool = ["read", "edit", "write", "multiedit", "notebookedit"]
        .iter()
        .any(|t| tool.eq_ignore_ascii_case(t));
    for key in ["file_path", "path", "notebook_path", "paths"] {
        if !file_tool && matches!(key, "path" | "paths") {
            continue;
        }
        let values: Vec<&str> = match &input[key] {
            Value::String(s) => vec![s],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        for value in values {
            if let Some(path) = normalize(root, cwd, value, false) {
                touched.paths.push(path);
            }
        }
    }
    if tool.eq_ignore_ascii_case("bash") {
        for word in shell_words(input["command"].as_str().unwrap_or_default()) {
            if let Some(path) = normalize(root, cwd, &word, true) {
                touched.paths.push(path);
            }
        }
    }
    if tool.eq_ignore_ascii_case("edit") && !touched.paths.is_empty() {
        // Only what the agent had to find counts; text it writes (new_string) is a citation.
        for key in ["old_string"] {
            for word in input[key]
                .as_str()
                .unwrap_or_default()
                .split(|c: char| c != '_' && !c.is_alphanumeric())
            {
                if identifier(word) {
                    touched.identifiers.insert(word.to_owned());
                }
            }
        }
    }
    touched.paths.sort();
    touched.paths.dedup();
    touched
}

/// No migration, no database creation, no lock wait on the hot path. One inserted
/// row per delivery; the primary key makes retries and concurrent tools harmless.
pub fn record(
    root: &Path,
    session: &str,
    seq: u64,
    window: u64,
    touches: &Touches,
) -> Result<(), StoreError> {
    if touches.paths.is_empty() && touches.identifiers.is_empty() {
        return Ok(());
    }
    let mut conn = Connection::open_with_flags(
        root.join(DATA_DIR).join(DB_FILE),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    // Short wait: parallel tools write at the same time; never long enough to stall a hook.
    conn.busy_timeout(Duration::from_millis(100))?;
    let version: String = conn.query_row(
        "SELECT value FROM meta WHERE key='schema_version'",
        [],
        |r| r.get(0),
    )?;
    if version != SCHEMA_VERSION.to_string() {
        return Ok(());
    }
    let seq = i64::try_from(seq).unwrap_or(i64::MAX);
    let lower = seq.saturating_sub(i64::try_from(window).unwrap_or(i64::MAX));
    let mut query = conn.prepare(
        "SELECT i.event_seq, i.item_id, i.kind, i.scope, t.triggers FROM injections i
         JOIN items t ON t.id=i.item_id
         WHERE i.session_id=?1 AND i.event_seq>=?2 AND i.event_seq<?3 AND i.mode='full'
         AND NOT EXISTS (SELECT 1 FROM auto_usage u WHERE u.session_id=i.session_id
         AND u.event_seq=i.event_seq AND u.item_id=i.item_id)",
    )?;
    let rows = query.query_map(rusqlite::params![session, lower, seq], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut signals = Vec::new();
    for row in rows {
        let (delivery, id, kind, scope, json) = row?;
        let Ok(triggers) = serde_json::from_str::<Triggers>(&json) else {
            continue;
        };
        let matched = triggers.paths.iter().any(|pattern| {
            touches.paths.iter().any(|path| {
                pattern
                    .strip_suffix('*')
                    .map_or(pattern == path, |prefix| path.starts_with(prefix))
            })
        }) || triggers.keywords.iter().any(|word| {
            (identifier(word) && touches.identifiers.contains(word))
                || (file_like(word) && touches.paths.iter().any(|path| path_ends_with(path, word)))
        });
        if matched {
            signals.push((delivery, id, kind, scope));
        }
    }
    drop(query);
    if !signals.is_empty() {
        // Batch matching deliveries in one commit: fsync once, not once per item.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut insert = tx.prepare("INSERT OR IGNORE INTO auto_usage(session_id,event_seq,item_id,kind,scope,at) VALUES (?1,?2,?3,?4,?5,?6)")?;
            let now = now_seconds();
            for (delivery, id, kind, scope) in signals {
                insert.execute(rusqlite::params![session, delivery, id, kind, scope, now])?;
            }
        }
        tx.commit()?;
    }
    Ok(())
}
