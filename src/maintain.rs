//! `elo maintain`: memory housekeeping, proposed first and applied only with `--apply`.
//!
//! Consolidation, not deletion (SPEC §6.1, Zep/Graphiti "invalidate, don't discard"):
//! duplicated items are superseded by their newest copy, overdue items are marked expired,
//! and only the automatic usefulness rows that point nowhere (or are older than 180 days)
//! are deleted. Laws are never touched automatically: duplicated laws are only reported.
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;

use crate::store::{now_seconds, Store, StoreError};
use crate::write::{supersede_at, WriteError};

/// Automatic usefulness rows older than this are history nobody reads.
pub const USAGE_RETENTION_DAYS: i64 = 180;

#[derive(Debug, Default)]
pub struct Plan {
    /// (old, new): `old` will be superseded by `new` (same kind, scope and text).
    pub duplicates: Vec<(String, String)>,
    /// Groups of duplicated laws: reported only, the owner decides.
    pub duplicated_laws: Vec<Vec<String>>,
    pub expired: Vec<String>,
    pub orphan_usage: usize,
    pub old_usage: usize,
    /// (old, new, similarity): similar decisions/notes where the newer may replace the
    /// older. Suggestions only: never applied by `--apply`.
    pub similar: Vec<(String, String, f64)>,
}

#[derive(Debug, Default)]
pub struct Applied {
    pub backup: PathBuf,
    pub superseded: usize,
    pub skipped: Vec<(String, String)>,
    pub expired: usize,
    pub usage_deleted: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MaintainError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Text used to find duplicates: lowercase, whitespace collapsed.
fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Words of 4+ letters, lowercase: enough to compare two notes without noise.
fn words(text: &str) -> std::collections::BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_lowercase)
        .collect()
}

/// Share of words in common (Jaccard).
fn similarity(
    a: &std::collections::BTreeSet<String>,
    b: &std::collections::BTreeSet<String>,
) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// Minimum similarity for a suggestion.
pub const SIMILAR_MIN: f64 = 0.28;

/// What `elo maintain` would do. Reads only.
pub fn plan(store: &Store, now: i64) -> Result<Plan, MaintainError> {
    let mut plan = Plan::default();
    let mut groups: BTreeMap<(String, String, String), Vec<(i64, String)>> = BTreeMap::new();
    let mut stmt = store.conn.prepare(
        "SELECT id, kind, scope, body, updated_at, expires_at FROM items WHERE status = 'active'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<i64>>(5)?,
        ))
    })?;
    let mut comparable: Vec<(
        String,
        String,
        String,
        i64,
        std::collections::BTreeSet<String>,
    )> = Vec::new();
    for row in rows {
        let (id, kind, scope, body, updated, expires) = row?;
        if matches!(kind.as_str(), "decision" | "note") && expires.is_none_or(|e| e > now) {
            comparable.push((
                id.clone(),
                kind.clone(),
                scope.clone(),
                updated,
                words(&body),
            ));
        }
        if expires.is_some_and(|e| e <= now) {
            plan.expired.push(id);
            continue;
        }
        let text = normalized(&body);
        if text.is_empty() {
            continue;
        }
        groups
            .entry((kind, scope, text))
            .or_default()
            .push((updated, id));
    }
    for ((kind, _, _), mut members) in groups {
        if members.len() < 2 {
            continue;
        }
        // Newest first (then id, for a stable choice).
        members.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        if kind == "law" {
            plan.duplicated_laws
                .push(members.into_iter().map(|(_, id)| id).collect());
            continue;
        }
        let keep = members[0].1.clone();
        for (_, old) in members.into_iter().skip(1) {
            plan.duplicates.push((old, keep.clone()));
        }
    }
    let duplicated: std::collections::BTreeSet<&String> =
        plan.duplicates.iter().map(|(old, _)| old).collect();
    for (i, a) in comparable.iter().enumerate() {
        for b in &comparable[i + 1..] {
            if a.1 != b.1 || a.2 != b.2 || a.4.len() < 4 || b.4.len() < 4 {
                continue;
            }
            let score = similarity(&a.4, &b.4);
            if score < SIMILAR_MIN || score >= 0.999 {
                continue;
            }
            let (old, new) = if (a.3, &a.0) < (b.3, &b.0) {
                (a, b)
            } else {
                (b, a)
            };
            if !duplicated.contains(&old.0) && !duplicated.contains(&new.0) {
                plan.similar.push((old.0.clone(), new.0.clone(), score));
            }
        }
    }
    plan.similar
        .sort_by(|x, y| y.2.total_cmp(&x.2).then_with(|| x.0.cmp(&y.0)));
    plan.orphan_usage = store.conn.query_row(
        "SELECT COUNT(*) FROM auto_usage u WHERE NOT EXISTS (SELECT 1 FROM items i WHERE i.id = u.item_id)",
        [],
        |r| r.get::<_, i64>(0),
    )? as usize;
    plan.old_usage = store.conn.query_row(
        "SELECT COUNT(*) FROM auto_usage WHERE at < ?1",
        [now - USAGE_RETENTION_DAYS * 86_400],
        |r| r.get::<_, i64>(0),
    )? as usize;
    plan.expired.sort();
    Ok(plan)
}

/// Applies `plan` after copying the whole store next to it.
pub fn apply(store: &mut Store, plan: &Plan, now: i64) -> Result<Applied, MaintainError> {
    let backup = store
        .path
        .with_file_name(format!("elo.db.bak-maintain-{}", now_seconds()));
    store
        .conn
        .execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()])?;
    let mut applied = Applied {
        backup,
        ..Applied::default()
    };
    for (old, new) in &plan.duplicates {
        match supersede_at(store, old, new, now) {
            Ok(_) => applied.superseded += 1,
            Err(WriteError::Refused(reason)) => applied.skipped.push((old.clone(), reason)),
            Err(e) => applied.skipped.push((old.clone(), e.to_string())),
        }
    }
    applied.expired = store.conn.execute(
        "UPDATE items SET status = 'expired' \
         WHERE status = 'active' AND expires_at IS NOT NULL AND expires_at <= ?1",
        [now],
    )?;
    applied.usage_deleted = store.conn.execute(
        "DELETE FROM auto_usage WHERE at < ?1 \
         OR NOT EXISTS (SELECT 1 FROM items i WHERE i.id = auto_usage.item_id)",
        [now - USAGE_RETENTION_DAYS * 86_400],
    )?;
    Ok(applied)
}

/// Human report of a plan (and of what was applied, when given).
pub fn report(plan: &Plan, applied: Option<&Applied>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "duplicates to supersede: {}\n",
        plan.duplicates.len()
    ));
    for (old, new) in plan.duplicates.iter().take(20) {
        out.push_str(&format!("  {old} → superseded by {new}\n"));
    }
    if !plan.duplicated_laws.is_empty() {
        out.push_str(&format!(
            "duplicated laws (decide by hand, nothing is touched): {}\n",
            plan.duplicated_laws.len()
        ));
        for group in &plan.duplicated_laws {
            out.push_str(&format!("  {}\n", group.join(", ")));
        }
    }
    out.push_str(&format!(
        "overdue to mark as expired: {}\n",
        plan.expired.len()
    ));
    out.push_str(&format!(
        "automatic scoreboard: {} orphan rows, {} older than {} days\n",
        plan.orphan_usage, plan.old_usage, USAGE_RETENTION_DAYS
    ));
    if !plan.similar.is_empty() {
        out.push_str(&format!(
            "suggestions (similar items; nothing is applied automatically, decide with elo supersede): {}\n",
            plan.similar.len()
        ));
        for (old, new, score) in plan.similar.iter().take(15) {
            out.push_str(&format!(
                "  {:.0}%  elo supersede {old} {new}\n",
                score * 100.0
            ));
        }
    }
    match applied {
        None => out.push_str("nothing was changed; to apply: elo maintain --apply\n"),
        Some(a) => {
            out.push_str(&format!(
                "APPLIED: {} superseded, {} expired, {} scoreboard rows deleted; backup: {}\n",
                a.superseded,
                a.expired,
                a.usage_deleted,
                a.backup.display()
            ));
            for (id, reason) in &a.skipped {
                out.push_str(&format!("  not superseded {id}: {reason}\n"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Idle-cycle invitation and AI-driven catalog/propose cycle.
// ---------------------------------------------------------------------------

const DAY_SECONDS: i64 = 86_400;

/// Returns the invitation line and resets the idle-cycle counters when
/// 24 h have passed and at least 5 sessions started since the last cycle.
pub fn maybe_invite(store: &Store, now: i64) -> Result<Option<String>, MaintainError> {
    // The hook stays cheap and never waits long on a write lock.
    let _ = store
        .conn
        .busy_timeout(std::time::Duration::from_millis(25));
    let found: std::collections::HashMap<String, String> = match store
        .conn
        .prepare(
            "SELECT key, value FROM meta \
                  WHERE key IN ('maintain_sessions_since', 'maintain_last_cycle_at')",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<_, _>>()
        }) {
        Ok(map) => map,
        Err(rusqlite::Error::SqliteFailure(f, _))
            if matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let sessions_since: i64 = found
        .get("maintain_sessions_since")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let last_at: i64 = found
        .get("maintain_last_cycle_at")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if now - last_at < DAY_SECONDS || sessions_since < 5 {
        let _ = store.meta_set("maintain_sessions_since", &(sessions_since + 1).to_string());
        return Ok(None);
    }
    if store
        .meta_set("maintain_last_cycle_at", &now.to_string())
        .is_err()
        || store.meta_set("maintain_sessions_since", "1").is_err()
    {
        return Ok(None);
    }
    Ok(Some(
        "Idle maintenance window: run elo_maintain_catalog then elo_maintain_propose on a batch of up to 30 items."
            .to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Catalog: compact list of active items for the agent to review.
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
struct CatalogItem {
    id: String,
    kind: String,
    scope: String,
    title: String,
    keywords: Vec<String>,
    paths: Vec<String>,
    always: bool,
    delivered: u64,
    used: u64,
}

#[derive(Debug, Default, serde::Serialize)]
struct CatalogResponse {
    items: Vec<CatalogItem>,
    next_cursor: i64,
}

/// Returns up to 30 active items, sorted by maintenance priority.
/// `cursor` is the number of items already returned (offset).
pub fn catalog(store: &Store, cursor: i64) -> Result<String, MaintainError> {
    const LIMIT: i64 = 30;
    let mut stmt = store.conn.prepare(
        "SELECT i.id, i.kind, i.scope, i.title, i.triggers, \
                COALESCE(inj.delivered, 0) AS delivered, \
                COALESCE(au.used, 0) AS used \
         FROM items i \
         LEFT JOIN ( \
             SELECT item_id, COUNT(*) AS delivered FROM injections GROUP BY item_id \
         ) inj ON inj.item_id = i.id \
         LEFT JOIN ( \
             SELECT item_id, COUNT(*) AS used FROM auto_usage GROUP BY item_id \
         ) au ON au.item_id = i.id \
         WHERE i.status = 'active'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let mut items: Vec<CatalogItem> = Vec::new();
    for row in rows {
        let (id, kind, scope, title, triggers_json, delivered, used) = row?;
        let triggers = match serde_json::from_str::<serde_json::Value>(&triggers_json) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let keywords = triggers["keywords"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let paths = triggers["paths"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let always = triggers["always"].as_bool().unwrap_or(false);
        let delivered = u64::try_from(delivered).unwrap_or(0);
        let used = u64::try_from(used).unwrap_or(0);
        let title: String = title.chars().take(80).collect();
        items.push(CatalogItem {
            id,
            kind,
            scope,
            title,
            keywords,
            paths,
            always,
            delivered,
            used,
        });
    }
    items.sort_by(|a, b| {
        let priority = |item: &CatalogItem| {
            (
                !item.always,
                item.delivered > 0,
                !(item.delivered >= 3 && item.used == 0),
                !item.paths.is_empty(),
                item.id.clone(),
            )
        };
        priority(a).cmp(&priority(b))
    });
    let total = items.len();
    let offset = cursor.clamp(0, total as i64) as usize;
    let page: Vec<CatalogItem> = items
        .into_iter()
        .skip(offset)
        .take(LIMIT as usize)
        .collect();
    let returned = page.len();
    let next_cursor = if offset + returned < total {
        (offset + returned) as i64
    } else {
        -1
    };
    Ok(serde_json::to_string(&CatalogResponse {
        items: page,
        next_cursor,
    })?)
}

// ---------------------------------------------------------------------------
// Proposals: agent-suggested changes, stored but not applied until reviewed.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ProposalOp {
    SetPaths { id: String, globs: Vec<String> },
    DropAlways { id: String },
    SetScope { id: String, scope: String },
    Merge { from: String, into: String },
}

#[derive(Debug, Clone)]
pub struct Proposal {
    pub id: i64,
    pub op: String,
    pub item_id: Option<String>,
    pub payload: String,
}

/// Result of a proposal round: `(accepted item ids, (op id, reason) rejections)`.
pub type ProposalOutcome = (Vec<String>, Vec<(String, String)>);

/// Returns `(accepted_ids, rejected_reasons)`.
pub fn propose(
    root: &Path,
    store: &Store,
    operations: Vec<ProposalOp>,
) -> Result<ProposalOutcome, MaintainError> {
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    let valid_scopes = valid_scopes(root, store)?;
    for op in operations {
        match validate_proposal(root, store, &valid_scopes, &op) {
            Ok(item_id) => {
                let payload = match &op {
                    ProposalOp::SetPaths { id: _, globs } => {
                        serde_json::json!({"globs": globs}).to_string()
                    }
                    ProposalOp::DropAlways { id: _ } => "{}".to_string(),
                    ProposalOp::SetScope { id: _, scope } => {
                        serde_json::json!({"scope": scope}).to_string()
                    }
                    ProposalOp::Merge { from: _, into } => {
                        serde_json::json!({"into": into}).to_string()
                    }
                };
                store.conn.execute(
                    "INSERT INTO proposals (op, item_id, payload, created_at, applied_at) \
                     VALUES (?1, ?2, ?3, ?4, NULL)",
                    rusqlite::params![op_name(&op), &item_id, &payload, now_seconds()],
                )?;
                accepted.push(item_id);
            }
            Err(reason) => rejected.push((op_id(&op), reason)),
        }
    }
    Ok((accepted, rejected))
}

fn op_name(op: &ProposalOp) -> &'static str {
    match op {
        ProposalOp::SetPaths { .. } => "set_paths",
        ProposalOp::DropAlways { .. } => "drop_always",
        ProposalOp::SetScope { .. } => "set_scope",
        ProposalOp::Merge { .. } => "merge",
    }
}

fn op_id(op: &ProposalOp) -> String {
    match op {
        ProposalOp::SetPaths { id, .. } => id.clone(),
        ProposalOp::DropAlways { id } => id.clone(),
        ProposalOp::SetScope { id, .. } => id.clone(),
        ProposalOp::Merge { from, .. } => from.clone(),
    }
}

fn validate_proposal(
    root: &Path,
    store: &Store,
    valid_scopes: &HashSet<String>,
    op: &ProposalOp,
) -> Result<String, String> {
    validate_proposal_conn(root, &store.conn, valid_scopes, op)
}

fn validate_proposal_conn(
    root: &Path,
    conn: &rusqlite::Connection,
    valid_scopes: &HashSet<String>,
    op: &ProposalOp,
) -> Result<String, String> {
    match op {
        ProposalOp::SetPaths { id, globs } => {
            active_item_conn(conn, id)?;
            if globs.is_empty() {
                return Err("set_paths needs at least one glob".into());
            }
            for glob in globs {
                validate_glob(root, glob)?;
            }
            Ok(id.clone())
        }
        ProposalOp::DropAlways { id } => {
            active_item_conn(conn, id)?;
            Ok(id.clone())
        }
        ProposalOp::SetScope { id, scope } => {
            active_item_conn(conn, id)?;
            if !valid_scopes.contains(scope) {
                return Err(format!(
                    "scope {scope:?} is not used by any item or discovered project"
                ));
            }
            Ok(id.clone())
        }
        ProposalOp::Merge { from, into } => {
            if from == into {
                return Err("cannot merge an item into itself".into());
            }
            let from_row = active_item_conn(conn, from)?;
            let into_row = active_item_conn(conn, into)?;
            if from_row.0 != into_row.0 {
                return Err("merge requires the same kind".into());
            }
            if from_row.1 != into_row.1 {
                return Err("merge requires the same scope".into());
            }
            if from_row.0 == "law" {
                return Err("laws cannot be merged automatically".into());
            }
            Ok(from.clone())
        }
    }
}

fn active_item_conn(conn: &rusqlite::Connection, id: &str) -> Result<(String, String), String> {
    conn.query_row(
        "SELECT kind, scope FROM items WHERE id = ?1 AND status = 'active'",
        [id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()
    .map_err(|e| e.to_string())?
    .ok_or_else(|| format!("{id} is not an active item"))
}

fn valid_scopes(root: &Path, store: &Store) -> Result<HashSet<String>, MaintainError> {
    valid_scopes_conn(root, &store.conn)
}

fn valid_scopes_conn(
    root: &Path,
    conn: &rusqlite::Connection,
) -> Result<HashSet<String>, MaintainError> {
    let mut scopes: HashSet<String> = conn
        .prepare("SELECT DISTINCT scope FROM items WHERE status = 'active'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    for project in crate::discover::discover(root).projects {
        scopes.insert(format!("project:{}", project.name));
    }
    Ok(scopes)
}

/// Star positions other than a single trailing `*` cannot be expressed by the
/// delivery matcher, so they are refused with this message.
const GLOB_PREFIX_ONLY: &str = "only a trailing * (prefix) is supported, e.g. dir/*";

/// Build/vendored directories the prefix search prunes so large workspaces stay fast.
const SKIP_DIRS: [&str; 4] = ["node_modules", ".git", "target", "dist"];

fn validate_glob(root: &Path, glob: &str) -> Result<(), String> {
    if glob.is_empty() {
        return Err("empty glob".into());
    }
    let path = Path::new(glob);
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("glob must be relative and cannot contain '..'".into());
    }
    if glob.split('/').any(str::is_empty) {
        return Err(format!("glob {glob:?} did not match any existing file"));
    }
    // Delivery (`path_matches`) understands only a single trailing `*`, a prefix of
    // the whole relative path; anything without it must be an exact file path.
    // Every other star position (`**`, mid-name) is refused before any fs access.
    let prefix = match glob.strip_suffix('*') {
        Some(prefix) if !prefix.contains('*') => prefix,
        _ if glob.contains('*') => return Err(GLOB_PREFIX_ONLY.into()),
        _ => glob,
    };
    let Ok(canon_root) = std::fs::canonicalize(root) else {
        return Err(format!("glob {glob:?} did not match any existing file"));
    };
    let matched = if prefix != glob {
        file_under_prefix(&canon_root, prefix)
    } else {
        exact_file_inside(&canon_root, glob)
    };
    if !matched {
        return Err(format!("glob {glob:?} did not match any existing file"));
    }
    Ok(())
}

/// True when `root/glob` is an existing regular file (a directory or a symlink
/// never counts) and stays inside the root.
fn exact_file_inside(canon_root: &Path, glob: &str) -> bool {
    let path = canon_root.join(glob);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return false;
    }
    let Ok(canon) = std::fs::canonicalize(&path) else {
        return false;
    };
    canon.starts_with(canon_root)
}

/// True when at least one regular file (never a symlink, never a directory
/// alone) under `canon_root` has a relative path starting with `prefix`.
/// Stops at the first such file; `SKIP_DIRS` are pruned along with any
/// directory that cannot contain a relative path beginning with `prefix`.
fn file_under_prefix(canonical_root: &Path, prefix: &str) -> bool {
    let mut stack = vec![(canonical_root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        if !directory_may_hold_files(&rel, prefix) {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            let kind = meta.file_type();
            // Symlinks are skipped entirely, so nothing can leave the root.
            if kind.is_symlink() {
                continue;
            }
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if kind.is_file() {
                if child_rel.starts_with(prefix) {
                    return true;
                }
            } else if kind.is_dir()
                && !SKIP_DIRS.iter().any(|skip| *skip == name)
                && directory_may_hold_files(&child_rel, prefix)
            {
                stack.push((entry.path(), child_rel));
            }
        }
    }
    false
}

/// Whether a file stored in the directory at relative path `dir` (paths
/// `dir/...`) can start with `prefix`.
fn directory_may_hold_files(dir: &str, prefix: &str) -> bool {
    if dir.is_empty() || prefix.is_empty() || dir.starts_with(prefix) {
        return true;
    }
    prefix.starts_with(dir) && prefix.as_bytes()[dir.len()] == b'/'
}

// ---------------------------------------------------------------------------
// Review and apply pending proposals.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct AppliedProposals {
    pub applied: Vec<i64>,
    pub skipped: Vec<(i64, String)>,
}

/// Lists pending proposals.
pub fn list_proposals(store: &Store) -> Result<Vec<Proposal>, MaintainError> {
    let mut stmt = store.conn.prepare(
        "SELECT id, op, item_id, payload FROM proposals WHERE applied_at IS NULL ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Proposal {
            id: row.get(0)?,
            op: row.get(1)?,
            item_id: row.get(2)?,
            payload: row.get(3)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(MaintainError::from)
}

/// Applies pending proposals. If `ids` is non-empty, only those ids are touched;
/// otherwise, when `nonlaw` is true, all pending non-law proposals are applied.
/// Laws are only applied when explicitly listed in `ids`.
pub fn apply_proposals(
    store: &mut Store,
    ids: &[i64],
    nonlaw: bool,
    now: i64,
) -> Result<AppliedProposals, MaintainError> {
    let mut applied = AppliedProposals::default();
    let pending = list_proposals(store)?;
    if ids.is_empty() && !nonlaw {
        return Ok(applied);
    }
    let target: Vec<i64> = if !ids.is_empty() {
        ids.to_vec()
    } else {
        pending
            .iter()
            .filter(|p| !is_law(store, p.item_id.as_deref().unwrap_or_default()))
            .map(|p| p.id)
            .collect()
    };
    if target.is_empty() {
        return Ok(applied);
    }
    let root = workspace_root(&store.path);
    for proposal in pending {
        if !target.contains(&proposal.id) {
            continue;
        }
        if let Err(reason) = apply_one(&root, store, &proposal, now) {
            applied.skipped.push((proposal.id, reason));
        } else {
            applied.applied.push(proposal.id);
        }
    }
    // Explicit ids that matched nothing pending are reported, never dropped in silence.
    for id in ids {
        if !applied.applied.contains(id) && !applied.skipped.iter().any(|(s, _)| s == id) {
            applied
                .skipped
                .push((*id, "not pending (unknown or already applied)".into()));
        }
    }
    Ok(applied)
}

fn workspace_root(store_path: &Path) -> PathBuf {
    let Some(parent) = store_path.parent() else {
        return PathBuf::from(".");
    };
    if parent.file_name().and_then(|n| n.to_str()) == Some(crate::store::DATA_DIR) {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    }
}

fn is_law(store: &Store, id: &str) -> bool {
    store
        .conn
        .query_row("SELECT kind FROM items WHERE id = ?1", [id], |row| {
            row.get::<_, String>(0)
        })
        .map(|k: String| k == "law")
        .unwrap_or(false)
}

fn proposal_to_op(proposal: &Proposal) -> Result<ProposalOp, String> {
    let id = proposal.item_id.clone().unwrap_or_default();
    match proposal.op.as_str() {
        "set_paths" => {
            let payload: serde_json::Value =
                serde_json::from_str(&proposal.payload).map_err(|e| e.to_string())?;
            let globs: Vec<String> = payload["globs"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            Ok(ProposalOp::SetPaths { id, globs })
        }
        "drop_always" => Ok(ProposalOp::DropAlways { id }),
        "set_scope" => {
            let payload: serde_json::Value =
                serde_json::from_str(&proposal.payload).map_err(|e| e.to_string())?;
            let scope = payload["scope"].as_str().unwrap_or("").to_string();
            Ok(ProposalOp::SetScope { id, scope })
        }
        "merge" => {
            let payload: serde_json::Value =
                serde_json::from_str(&proposal.payload).map_err(|e| e.to_string())?;
            let into = payload["into"].as_str().unwrap_or("").to_string();
            if id.is_empty() || into.is_empty() {
                return Err("merge missing ids".into());
            }
            Ok(ProposalOp::Merge { from: id, into })
        }
        _ => Err(format!("unknown proposal op {}", proposal.op)),
    }
}

fn apply_one(root: &Path, store: &mut Store, proposal: &Proposal, now: i64) -> Result<(), String> {
    let op = proposal_to_op(proposal)?;
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    crate::write::sweep_expired(&tx, now).map_err(|e| e.to_string())?;
    let valid_scopes = valid_scopes_conn(root, &tx).map_err(|e| e.to_string())?;
    validate_proposal_conn(root, &tx, &valid_scopes, &op)?;
    match op {
        ProposalOp::SetPaths { id, globs } => {
            update_triggers_conn(&tx, &id, now, |triggers| {
                triggers["paths"] = serde_json::Value::Array(
                    globs
                        .clone()
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                );
            })?;
            append_evidence_conn(&tx, &id, "set_paths applied from agent proposal", now)?;
        }
        ProposalOp::DropAlways { id } => {
            update_triggers_conn(&tx, &id, now, |triggers| {
                triggers["always"] = serde_json::Value::Bool(false);
            })?;
            append_evidence_conn(&tx, &id, "drop_always applied from agent proposal", now)?;
        }
        ProposalOp::SetScope { id, scope } => {
            let changed = tx
                .execute(
                    "UPDATE items SET scope = ?1, updated_at = ?2 WHERE id = ?3 AND status = 'active'",
                    rusqlite::params![scope, now, id],
                )
                .map_err(|e| e.to_string())?;
            if changed == 0 {
                return Err(format!("{id} could not be updated"));
            }
            append_evidence_conn(&tx, &id, "set_scope applied from agent proposal", now)?;
        }
        ProposalOp::Merge { from, into } => {
            let target: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind, scope FROM items WHERE id = ?1",
                    [&into],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            let Some((kind, scope)) = target else {
                return Err(crate::write::WriteError::NotFound(into).to_string());
            };
            let link = crate::write::check_supersession(&tx, &from, &into, &kind, &scope, now)
                .map_err(|e| e.to_string())?;
            if link == crate::write::Link::Apply {
                crate::write::apply_supersession(&tx, &from, &into, now)
                    .map_err(|e| e.to_string())?;
            }
            append_evidence_conn(&tx, &from, "merge applied from agent proposal", now)?;
        }
    }
    tx.execute(
        "UPDATE proposals SET applied_at = ?1 WHERE id = ?2",
        rusqlite::params![now, proposal.id],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

fn update_triggers_conn<F>(
    conn: &rusqlite::Connection,
    id: &str,
    now: i64,
    mut f: F,
) -> Result<(), String>
where
    F: FnMut(&mut serde_json::Value),
{
    let triggers: String = conn
        .query_row(
            "SELECT triggers FROM items WHERE id = ?1 AND status = 'active'",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{id} not found"))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&triggers).map_err(|e| e.to_string())?;
    f(&mut value);
    let updated = value.to_string();
    let changed = conn
        .execute(
            "UPDATE items SET triggers = ?1, updated_at = ?2 WHERE id = ?3 AND status = 'active'",
            rusqlite::params![updated, now, id],
        )
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        return Err(format!("{id} not found"));
    }
    Ok(())
}

fn append_evidence_conn(
    conn: &rusqlite::Connection,
    id: &str,
    note: &str,
    now: i64,
) -> Result<(), String> {
    let stamp = chrono::DateTime::<chrono::Utc>::from_timestamp(now, 0)
        .unwrap_or_default()
        .to_rfc3339();
    let line = format!("{note} at {stamp}");
    conn.execute(
        "UPDATE items SET evidence = CASE WHEN evidence IS NULL OR evidence = '' THEN ?1 \
         ELSE evidence || '\n' || ?1 END, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![line, now, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
