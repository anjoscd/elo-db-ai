//! `elo recall` (SPEC §6.1, §9): full text search over the store with FTS5.
//! Ranking mixes BM25, recency and importance; without `--history` only items that are
//! effectively active are shown.
use chrono::{DateTime, Utc};
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::store::{now_seconds, Store, StoreError};

const W_BM25: f64 = 0.6;
const W_RECENCY: f64 = 0.2;
const W_IMPORTANCE: f64 = 0.2;
const HALF_LIFE_DAYS: f64 = 90.0;
const SNIPPET_WORDS: usize = 12;

#[derive(Debug, Clone, Default)]
pub struct Query {
    pub text: String,
    pub history: bool,
    pub scope: Option<String>,
    pub kind: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: String,
    pub kind: String,
    pub scope: String,
    pub date: String,
    pub status: String,
    pub superseded_by: Option<String>,
    pub score: f64,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RecallError {
    #[error("the query has no searchable words")]
    EmptyQuery,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("sqlite error: {0}")]
    Sql(#[from] rusqlite::Error),
}

/// Builds a safe FTS5 expression: every word is quoted (no operator injection), words of
/// four or more characters match by prefix. FTS5's tokenizer folds accents on both sides.
fn match_expression(text: &str) -> Option<String> {
    let normalized: String = text.nfc().collect();
    let words: Vec<String> = normalized
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            if w.chars().count() >= 4 {
                format!("\"{w}\"*")
            } else {
                format!("\"{w}\"")
            }
        })
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

fn snippet(body: &str) -> String {
    let clean: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let words: Vec<&str> = clean.split_whitespace().collect();
    let mut out = words
        .iter()
        .take(SNIPPET_WORDS)
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    if words.len() > SNIPPET_WORDS {
        out.push('…');
    }
    out
}

/// Searches at the instant `now` (unix seconds).
pub fn recall(store: &Store, query: &Query, now: i64) -> Result<Vec<Hit>, RecallError> {
    let expression = match_expression(&query.text).ok_or(RecallError::EmptyQuery)?;
    let mut sql = String::from(
        "SELECT i.id, i.kind, i.scope, i.body, i.importance, i.created_at, i.updated_at, \
         i.expires_at, i.status, i.superseded_by, bm25(items_fts, 2.0, 1.0) \
         FROM items_fts JOIN items i ON i.rowid = items_fts.rowid WHERE items_fts MATCH ?1",
    );
    let mut params: Vec<&str> = vec![&expression];
    if let Some(scope) = &query.scope {
        sql.push_str(" AND i.scope = ?2");
        params.push(scope);
    }
    if let Some(kind) = &query.kind {
        sql.push_str(&format!(" AND i.kind = ?{}", params.len() + 1));
        params.push(kind);
    }
    let mut statement = store.conn.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(params), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, f64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, Option<i64>>(7)?,
            r.get::<_, String>(8)?,
            r.get::<_, Option<String>>(9)?,
            r.get::<_, f64>(10)?,
        ))
    })?;
    let mut found = Vec::new();
    for row in rows {
        let (id, kind, scope, body, importance, created, updated, expires, status, sup, bm25) =
            row?;
        // Effective status (SPEC §6.1): a stored `active` past its expiry reads as `expired`.
        let status = if status == "active" && expires.is_some_and(|e| now >= e) {
            "expired".to_string()
        } else {
            status
        };
        if !query.history && (status != "active" || sup.is_some()) {
            continue;
        }
        found.push((
            Hit {
                id,
                kind,
                scope,
                date: DateTime::<Utc>::from_timestamp(created, 0)
                    .unwrap_or_default()
                    .format("%Y-%m-%d")
                    .to_string(),
                status,
                superseded_by: sup,
                score: 0.0,
                text: snippet(&body),
            },
            -bm25,
            importance,
            updated,
        ));
    }
    let best = found.iter().map(|f| f.1).fold(0.0_f64, f64::max);
    for (hit, relevance, importance, updated) in &mut found {
        let text_score = if best > 0.0 { *relevance / best } else { 0.5 };
        let age_days = (now - *updated).max(0) as f64 / 86_400.0;
        let recency = 0.5_f64.powf(age_days / HALF_LIFE_DAYS);
        hit.score = W_BM25 * text_score + W_RECENCY * recency + W_IMPORTANCE * *importance;
    }
    found.sort_by(|a, b| {
        b.0.score
            .total_cmp(&a.0.score)
            .then_with(|| a.0.id.cmp(&b.0.id))
    });
    let limit = if query.limit == 0 { 10 } else { query.limit };
    Ok(found.into_iter().take(limit).map(|f| f.0).collect())
}

/// One line per hit: id, kind, scope, date, first words (and the status in history mode).
pub fn render_text(hits: &[Hit], history: bool) -> String {
    let mut out = String::new();
    for hit in hits {
        let mut line = format!("{}  {}  {}  {}", hit.id, hit.kind, hit.scope, hit.date);
        if history {
            line.push_str(&format!("  [{}", hit.status));
            if let Some(next) = &hit.superseded_by {
                line.push_str(&format!(" → {next}"));
            }
            line.push(']');
        }
        line.push_str("  ");
        line.push_str(&hit.text);
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn render_json(hits: &[Hit]) -> String {
    serde_json::to_string(hits).unwrap_or_else(|_| "[]".into())
}

/// `recall` at the current time.
pub fn recall_now(store: &Store, query: &Query) -> Result<Vec<Hit>, RecallError> {
    recall(store, query, now_seconds())
}
