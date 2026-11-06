//! Memory writes (SPEC §6.1, §6.2): `elo add`, `supersede`, `promote` and `forget`.
//! Every write is one `BEGIN IMMEDIATE` transaction, so the invariants are checked and
//! applied under the same write lock and concurrent writers queue up.
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use unicode_normalization::UnicodeNormalization;

use crate::import::{valid_scope, KINDS, LAW_MAX_BYTES};
use crate::store::{now_seconds, Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("{0}")]
    Refused(String),
    #[error("unknown item id: {0}")]
    NotFound(String),
    #[error("{old} is already superseded by {successor}")]
    AlreadySuperseded { old: String, successor: String },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("sqlite error: {0}")]
    Sql(#[from] rusqlite::Error),
}

fn refuse<T>(message: impl Into<String>) -> Result<T, WriteError> {
    Err(WriteError::Refused(message.into()))
}

/// What `elo add` receives, already parsed.
#[derive(Debug, Clone)]
pub struct NewEntry {
    pub kind: String,
    pub scope: String,
    pub title: String,
    pub body: String,
    pub triggers: Vec<String>,
    pub importance: f64,
    pub supersedes: Option<String>,
    pub expires_at: Option<i64>,
    pub id: Option<String>,
    pub as_user: bool,
}

/// Kinds that can be superseded (SPEC §6.1: corrections of decisions and notes).
fn supersedable(kind: &str) -> bool {
    matches!(kind, "decision" | "note")
}

/// A successor must live in the same scope or a wider one: only `global` is wider.
fn covers(successor: &str, old: &str) -> bool {
    successor == old || successor == "global"
}

/// Rejects entries that break the rules of §6.1/§6.2 before touching the store.
fn validate(entry: &NewEntry, now: i64) -> Result<(), WriteError> {
    if !KINDS.contains(&entry.kind.as_str()) {
        return refuse(format!("unknown kind {:?}", entry.kind));
    }
    if !valid_scope(&entry.scope) {
        return refuse(format!(
            "invalid scope {:?} (use global, project:NAME or link:NAME)",
            entry.scope
        ));
    }
    if entry.title.trim().is_empty() || entry.body.trim().is_empty() {
        return refuse("title and body must not be empty");
    }
    if !(0.0..=1.0).contains(&entry.importance) {
        return refuse(format!("importance {} outside [0, 1]", entry.importance));
    }
    if entry.kind == "law" {
        if !entry.as_user {
            return refuse("a law needs --as-user (SPEC §6.2)");
        }
        let bytes = entry.body.trim().len();
        if bytes > LAW_MAX_BYTES {
            return refuse(format!(
                "law body has {bytes} bytes, the limit is {LAW_MAX_BYTES}"
            ));
        }
    }
    if let Some(expires) = entry.expires_at {
        if matches!(entry.kind.as_str(), "law" | "rule") {
            return refuse(format!("a {} cannot have --expires", entry.kind));
        }
        if expires <= now {
            return refuse("--expires is not in the future");
        }
    }
    if entry.supersedes.is_some() && !supersedable(&entry.kind) {
        return refuse(format!("a {} cannot supersede anything", entry.kind));
    }
    if let Some(id) = &entry.id {
        let ok = !id.is_empty()
            && id.len() <= 80
            && id
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
        if !ok {
            return refuse(format!("invalid --id {id:?}"));
        }
    }
    Ok(())
}

/// Lowercase ASCII slug without accents, at most 40 characters.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.nfd().filter(|c| c.is_ascii() || c.is_alphanumeric()) {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    // Combining marks were dropped above; non-ASCII letters act as separators.
    let cut: String = out.chars().take(40).collect();
    let cut = cut.trim_matches('-').to_string();
    if cut.is_empty() {
        "item".into()
    } else {
        cut
    }
}

fn date(seconds: i64) -> String {
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .unwrap_or_default()
        .format("%Y%m%d")
        .to_string()
}

fn exists(conn: &Connection, id: &str) -> Result<bool, WriteError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM items WHERE id = ?1)",
        [id],
        |r| r.get(0),
    )?)
}

fn fresh_id(conn: &Connection, entry: &NewEntry, now: i64) -> Result<String, WriteError> {
    if let Some(id) = &entry.id {
        if exists(conn, id)? {
            return refuse(format!("id {id} already exists; ids are never overwritten"));
        }
        return Ok(id.clone());
    }
    let base = format!("{}-{}-{}", entry.kind, date(now), slug(&entry.title));
    let mut candidate = base.clone();
    let mut n = 2;
    while exists(conn, &candidate)? {
        candidate = format!("{base}-{n}");
        n += 1;
    }
    Ok(candidate)
}

/// Stored `active` rows whose `expires_at` has passed become `expired` (write paths only).
pub(crate) fn sweep_expired(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE items SET status = 'expired' \
         WHERE status = 'active' AND expires_at IS NOT NULL AND expires_at <= ?1",
        [now],
    )
}

/// Outcome of a successful supersession check.
#[derive(Debug, PartialEq, Eq)]
pub enum Link {
    Apply,
    /// `old` already points to `new`: nothing to do.
    AlreadyLinked,
}

/// Checks that `old` may be superseded by `new_id`, which may not exist yet (`new_kind` and
/// `new_scope` describe it then). Stays read-only.
pub(crate) fn check_supersession(
    conn: &Connection,
    old_id: &str,
    new_id: &str,
    new_kind: &str,
    new_scope: &str,
    now: i64,
) -> Result<Link, WriteError> {
    if old_id == new_id {
        return refuse("an item cannot supersede itself");
    }
    type Row = (String, String, String, Option<String>, Option<i64>);
    let old: Option<Row> = conn
        .query_row(
            "SELECT kind, scope, status, superseded_by, expires_at FROM items WHERE id = ?1",
            [old_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((kind, scope, status, successor, expires)) = old else {
        return Err(WriteError::NotFound(old_id.into()));
    };
    if !supersedable(&kind) {
        return refuse(format!(
            "{old_id} is a {kind}; only decisions and notes can be superseded"
        ));
    }
    if kind != new_kind {
        return refuse(format!(
            "{old_id} is a {kind} but the successor is a {new_kind}: kinds must match"
        ));
    }
    if !covers(new_scope, &scope) {
        return refuse(format!(
            "successor scope {new_scope} is narrower than {scope} of {old_id}"
        ));
    }
    if let Some(successor) = successor {
        if successor == new_id {
            return Ok(Link::AlreadyLinked);
        }
        return Err(WriteError::AlreadySuperseded {
            old: old_id.into(),
            successor,
        });
    }
    if status != "active" {
        return refuse(format!(
            "{old_id} is {status}, only active items can be superseded"
        ));
    }
    if expires.is_some_and(|e| e <= now) {
        return refuse(format!(
            "{old_id} is expired, only active items can be superseded"
        ));
    }
    // The successor, when it already exists, must itself be live and must not lead back.
    let mut seen = std::collections::BTreeSet::new();
    let mut new_state: Option<(String, Option<i64>)> = None;
    let mut cursor = Some(new_id.to_string());
    while let Some(id) = cursor {
        if id == old_id {
            return refuse(format!(
                "superseding {old_id} with {new_id} would create a cycle"
            ));
        }
        if !seen.insert(id.clone()) {
            return refuse("supersession chain already contains a cycle");
        }
        let row: Option<(String, Option<String>, Option<i64>)> = conn
            .query_row(
                "SELECT status, superseded_by, expires_at FROM items WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((status, next, expires)) = row else {
            break;
        };
        if id == new_id {
            new_state = Some((status, expires));
        }
        cursor = next;
    }
    if let Some((status, expires)) = new_state {
        if status != "active" || expires.is_some_and(|e| e <= now) {
            return refuse(format!("successor {new_id} is {status}, not active"));
        }
    }
    Ok(Link::Apply)
}

pub(crate) fn apply_supersession(
    conn: &Connection,
    old_id: &str,
    new_id: &str,
    now: i64,
) -> Result<(), WriteError> {
    let changed = conn.execute(
        "UPDATE items SET status = 'superseded', superseded_by = ?2, updated_at = ?3 \
         WHERE id = ?1 AND status = 'active' AND superseded_by IS NULL",
        rusqlite::params![old_id, new_id, now],
    )?;
    if changed != 1 {
        return refuse(format!("{old_id} could not be superseded"));
    }
    Ok(())
}

/// Adds an item; returns its id.
pub fn add(store: &mut Store, entry: &NewEntry) -> Result<String, WriteError> {
    add_at(store, entry, now_seconds())
}

pub fn add_at(store: &mut Store, entry: &NewEntry, now: i64) -> Result<String, WriteError> {
    add_with_paths_at(store, entry, &[], now)
}

/// Write path triggers in the same transaction used by `add`.
pub fn add_with_paths(
    store: &mut Store,
    entry: &NewEntry,
    paths: &[String],
) -> Result<String, WriteError> {
    add_with_paths_at(store, entry, paths, now_seconds())
}

fn add_with_paths_at(
    store: &mut Store,
    entry: &NewEntry,
    paths: &[String],
    now: i64,
) -> Result<String, WriteError> {
    validate(entry, now)?;
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    sweep_expired(&tx, now)?;
    let id = fresh_id(&tx, entry, now)?;
    let link = match &entry.supersedes {
        Some(old) => Some(check_supersession(
            &tx,
            old,
            &id,
            &entry.kind,
            &entry.scope,
            now,
        )?),
        None => None,
    };
    let triggers = serde_json::json!({
        "keywords": entry.triggers,
        "paths": paths,
        "always": false,
    })
    .to_string();
    tx.execute(
        "INSERT INTO items (id, kind, scope, title, body, triggers, importance, created_at, \
         updated_at, expires_at, status, superseded_by, origin, evidence) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, 'active', NULL, ?10, NULL)",
        rusqlite::params![
            id,
            entry.kind,
            entry.scope,
            entry.title.trim(),
            entry.body.trim(),
            triggers,
            entry.importance,
            now,
            entry.expires_at,
            if entry.as_user { "user" } else { "agent" },
        ],
    )?;
    if let (Some(old), Some(Link::Apply)) = (&entry.supersedes, link) {
        apply_supersession(&tx, old, &id, now)?;
    }
    tx.commit()?;
    Ok(id)
}

/// `elo supersede <old> <new>`: both must already exist.
/// `Link::AlreadyLinked` means `old` already pointed to `new` and nothing changed.
pub fn supersede(store: &mut Store, old: &str, new: &str) -> Result<Link, WriteError> {
    supersede_at(store, old, new, now_seconds())
}

pub fn supersede_at(store: &mut Store, old: &str, new: &str, now: i64) -> Result<Link, WriteError> {
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    sweep_expired(&tx, now)?;
    let target: Option<(String, String)> = tx
        .query_row("SELECT kind, scope FROM items WHERE id = ?1", [new], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let Some((kind, scope)) = target else {
        return Err(WriteError::NotFound(new.into()));
    };
    let link = check_supersession(&tx, old, new, &kind, &scope, now)?;
    if link == Link::Apply {
        apply_supersession(&tx, old, new, now)?;
    }
    tx.commit()?;
    Ok(link)
}

/// `elo promote <id>`: an item with `origin = user` becomes a law; the act is recorded in
/// `evidence`.
pub fn promote(store: &mut Store, id: &str) -> Result<(), WriteError> {
    promote_at(store, id, now_seconds())
}

pub fn promote_at(store: &mut Store, id: &str, now: i64) -> Result<(), WriteError> {
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    sweep_expired(&tx, now)?;
    type Row = (String, String, String, String, Option<i64>, Option<String>);
    let row: Option<Row> = tx
        .query_row(
            "SELECT kind, origin, status, body, expires_at, evidence FROM items WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, origin, status, body, expires, evidence)) = row else {
        return Err(WriteError::NotFound(id.into()));
    };
    if kind == "law" {
        return refuse(format!("{id} is already a law"));
    }
    if origin != "user" {
        return refuse(format!(
            "{id} has origin {origin}; only items with origin user can be promoted (re-add it with --as-user)"
        ));
    }
    let linked: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM items WHERE id = ?1 AND superseded_by IS NOT NULL) \
         OR EXISTS(SELECT 1 FROM items WHERE superseded_by = ?1)",
        [id],
        |r| r.get(0),
    )?;
    if linked {
        return refuse(format!(
            "{id} takes part in a supersession chain; it cannot be promoted to law"
        ));
    }
    if status != "active" {
        return refuse(format!(
            "{id} is {status}, only active items can be promoted"
        ));
    }
    if expires.is_some() {
        return refuse(format!("{id} has an expiry date; a law cannot expire"));
    }
    let bytes = body.trim().len();
    if bytes > LAW_MAX_BYTES {
        return refuse(format!(
            "{id} body has {bytes} bytes, the law limit is {LAW_MAX_BYTES}"
        ));
    }
    let stamp = DateTime::<Utc>::from_timestamp(now, 0)
        .unwrap_or_default()
        .to_rfc3339();
    let note = format!("promoted to law by user at {stamp} (was {kind})");
    let evidence = match evidence {
        Some(old) if !old.is_empty() => format!("{old}\n{note}"),
        _ => note,
    };
    tx.execute(
        "UPDATE items SET kind = 'law', evidence = ?2, updated_at = ?3 WHERE id = ?1",
        rusqlite::params![id, evidence, now],
    )?;
    tx.commit()?;
    Ok(())
}

/// `elo forget <id>`: leaves a tombstone (`status = archived`); the row stays. An item that
/// is the successor of another cannot be archived, or the replaced item would stay hidden.
pub fn forget(store: &mut Store, id: &str) -> Result<(), WriteError> {
    forget_at(store, id, now_seconds())
}

pub fn forget_at(store: &mut Store, id: &str, now: i64) -> Result<(), WriteError> {
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    sweep_expired(&tx, now)?;
    let status: Option<String> = tx
        .query_row("SELECT status FROM items WHERE id = ?1", [id], |r| r.get(0))
        .optional()?;
    match status.as_deref() {
        None => return Err(WriteError::NotFound(id.into())),
        Some("archived") => return refuse(format!("{id} is already archived")),
        Some(_) => {}
    }
    let replaced: Option<String> = tx
        .query_row(
            "SELECT id FROM items WHERE superseded_by = ?1 LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(replaced) = replaced {
        return refuse(format!(
            "{id} is the successor of {replaced}; archiving it would leave that item hidden"
        ));
    }
    tx.execute(
        "UPDATE items SET status = 'archived', updated_at = ?2 WHERE id = ?1",
        rusqlite::params![id, now],
    )?;
    tx.commit()?;
    Ok(())
}
