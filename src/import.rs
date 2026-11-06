//! `elo import FILE.jsonl` (SPEC §6, §9): brings an existing knowledge base into the store.
//! Trust boundary (SPEC §6.2): imported items get `origin = import` and cannot be laws,
//! unless the user runs the import with `--as-user`. Nothing is changed silently: every
//! downgrade and rejection is listed in the report.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use rusqlite::OptionalExtension;
use serde::Deserialize;

use crate::store::{now_seconds, Store, StoreError};

/// Law bodies are capped (SPEC §4.4).
pub const LAW_MAX_BYTES: usize = 400;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    id: String,
    kind: String,
    scope: String,
    title: String,
    body: String,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    always: bool,
    #[serde(default = "half")]
    importance: f64,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    supersedes: Option<String>,
}

fn half() -> f64 {
    0.5
}

#[derive(Debug, Default)]
pub struct Report {
    pub imported: usize,
    pub by_kind: BTreeMap<String, usize>,
    pub by_scope: BTreeMap<String, usize>,
    /// `(line, id, reason)` for lines not imported.
    pub rejected: Vec<(usize, String, String)>,
    /// `(id, what changed)`: laws kept as rules, with the reason.
    pub downgraded: Vec<(String, String)>,
    /// Ids not changed because the stored item came from the user (needs `--as-user`).
    pub protected: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("cannot read {0}: {1}")]
    Read(String, std::io::Error),
    #[error("supersession failed during import: {0}")]
    Supersede(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

pub(crate) const KINDS: &[&str] = &["law", "rule", "decision", "note", "pitfall"];

pub(crate) fn valid_scope(scope: &str) -> bool {
    scope == "global"
        || scope
            .strip_prefix("project:")
            .or_else(|| scope.strip_prefix("link:"))
            .is_some_and(|name| !name.is_empty())
}

/// Reads `file` and upserts every valid line. With `dry_run` nothing is written.
pub fn import(
    store: &mut Store,
    file: &Path,
    as_user: bool,
    dry_run: bool,
) -> Result<Report, ImportError> {
    let text =
        fs::read_to_string(file).map_err(|e| ImportError::Read(file.display().to_string(), e))?;
    let mut report = Report::default();
    let now = now_seconds();
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    crate::write::sweep_expired(&tx, now)?;
    let mut seen = std::collections::BTreeSet::new();
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        let line: Line = match serde_json::from_str(raw) {
            Ok(line) => line,
            Err(e) => {
                report
                    .rejected
                    .push((number, String::new(), format!("invalid JSON: {e}")));
                continue;
            }
        };
        if line.id.trim().is_empty() || line.title.trim().is_empty() {
            report
                .rejected
                .push((number, line.id, "empty id or title".into()));
            continue;
        }
        if !KINDS.contains(&line.kind.as_str()) {
            report
                .rejected
                .push((number, line.id, format!("unknown kind {:?}", line.kind)));
            continue;
        }
        if !valid_scope(&line.scope) {
            report
                .rejected
                .push((number, line.id, format!("invalid scope {:?}", line.scope)));
            continue;
        }
        if !seen.insert(line.id.clone()) {
            report
                .rejected
                .push((number, line.id, "duplicate id in file".into()));
            continue;
        }
        if !as_user {
            let user_owned: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM items WHERE id = ?1 AND origin = 'user')",
                [&line.id],
                |row| row.get(0),
            )?;
            if user_owned {
                report.protected.push(line.id);
                continue;
            }
        }
        let mut kind = line.kind.clone();
        if kind == "law" && !as_user {
            kind = "rule".into();
            report.downgraded.push((
                line.id.clone(),
                "law kept as rule: laws need --as-user".into(),
            ));
        } else if kind == "law" && line.body.trim().len() > LAW_MAX_BYTES {
            kind = "rule".into();
            report.downgraded.push((
                line.id.clone(),
                format!(
                    "law kept as rule: body {} bytes > {LAW_MAX_BYTES}",
                    line.body.trim().len()
                ),
            ));
        }
        // Always-on is an injection privilege like a law: only for user-trusted imports.
        let always = line.always && as_user;
        if line.always && !as_user {
            report
                .downgraded
                .push((line.id.clone(), "always-on dropped: needs --as-user".into()));
        }
        let triggers = serde_json::json!({
            "keywords": line.keywords,
            "paths": line.paths,
            "always": always,
        })
        .to_string();
        let origin = if as_user { "user" } else { "import" };
        let created = line.created_at.unwrap_or(now);
        // An upsert must not move an item that takes part in a supersession chain.
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT kind, scope FROM items WHERE id = ?1",
                [&line.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if existing.is_some_and(|(k, s)| k != kind || s != line.scope) {
            let linked: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM items WHERE id = ?1 AND superseded_by IS NOT NULL) \
                 OR EXISTS(SELECT 1 FROM items WHERE superseded_by = ?1)",
                [&line.id],
                |row| row.get(0),
            )?;
            if linked {
                report.rejected.push((
                    number,
                    line.id,
                    "kind or scope change would break a supersession chain".into(),
                ));
                continue;
            }
        }
        let link = match &line.supersedes {
            None => None,
            Some(old) => {
                match crate::write::check_supersession(&tx, old, &line.id, &kind, &line.scope, now)
                {
                    Ok(link) => Some(link),
                    Err(crate::write::WriteError::Store(e)) => return Err(e.into()),
                    Err(crate::write::WriteError::Sql(e)) => return Err(e.into()),
                    Err(refusal) => {
                        report
                            .rejected
                            .push((number, line.id, format!("supersedes: {refusal}")));
                        continue;
                    }
                }
            }
        };
        tx.execute(
            "INSERT INTO items (id, kind, scope, title, body, triggers, importance, created_at, \
             updated_at, expires_at, status, superseded_by, origin, evidence) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, 'active', NULL, ?10, NULL) \
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, scope = excluded.scope, \
             title = excluded.title, body = excluded.body, triggers = excluded.triggers, \
             importance = excluded.importance, updated_at = excluded.updated_at, origin = excluded.origin",
            rusqlite::params![
                line.id,
                kind,
                line.scope,
                line.title,
                line.body,
                triggers,
                line.importance.clamp(0.0, 1.0),
                created,
                now,
                origin
            ],
        )?;
        if let (Some(old), Some(crate::write::Link::Apply)) = (&line.supersedes, link) {
            match crate::write::apply_supersession(&tx, old, &line.id, now) {
                Ok(()) => {}
                Err(crate::write::WriteError::Sql(e)) => return Err(e.into()),
                Err(other) => return Err(ImportError::Supersede(other.to_string())),
            }
        }
        report.imported += 1;
        *report.by_kind.entry(kind).or_default() += 1;
        *report.by_scope.entry(line.scope).or_default() += 1;
    }
    if dry_run {
        tx.rollback()?;
    } else {
        tx.commit()?;
    }
    Ok(report)
}
