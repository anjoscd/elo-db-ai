//! `elo maintain`: memory housekeeping, proposed first and applied only with `--apply`.
//!
//! Consolidation, not deletion (SPEC §6.1, Zep/Graphiti "invalidate, don't discard"):
//! duplicated items are superseded by their newest copy, overdue items are marked expired,
//! and only the automatic usefulness rows that point nowhere (or are older than 180 days)
//! are deleted. Laws are never touched automatically: duplicated laws are only reported.
use std::collections::BTreeMap;
use std::path::PathBuf;

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
