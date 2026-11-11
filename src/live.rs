//! Running the ELO selector on a real workspace: items come from the store and code
//! signals are read from the workspace itself (the bench uses the same selector).
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::bench::fixture::{Fixture, Item, Split, Triggers};
use crate::config::Config;
use crate::store::{Store, StoreError};

fn iso(seconds: i64) -> String {
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .unwrap_or_default()
        .to_rfc3339()
}

/// Triggers are stored as JSON; an empty array (older rows) means "no triggers".
fn triggers(text: &str) -> Triggers {
    match serde_json::from_str::<Triggers>(text) {
        Ok(t) => t,
        Err(_) => Triggers {
            keywords: Vec::new(),
            paths: Vec::new(),
            always: false,
        },
    }
}

fn enum_of<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, StoreError> {
    serde_json::from_value(Value::String(value.to_string()))
        .map_err(|_| StoreError::InvalidSchemaValue(format!("unknown value {value:?}")))
}

/// Every item in the store, as the selector expects them.
pub fn load_items(store: &Store) -> Result<Vec<Item>, StoreError> {
    load_filtered(store, None)
}

/// Fetch only the requested item, using the primary-key index.
pub fn load_item(store: &Store, id: &str) -> Result<Item, StoreError> {
    load_filtered(store, Some(id))?
        .pop()
        .ok_or_else(|| StoreError::UnknownItem(id.into()))
}

fn load_filtered(store: &Store, id: Option<&str>) -> Result<Vec<Item>, StoreError> {
    let sql = "SELECT id, kind, scope, title, body, triggers, importance, created_at, updated_at, \
         expires_at, status, superseded_by, origin FROM items WHERE id = ?1 ORDER BY id";
    let all_sql = sql.replace(" WHERE id = ?1", "");
    let mut statement = store
        .conn
        .prepare(if id.is_some() { sql } else { &all_sql })?;
    let params: Vec<&str> = id.into_iter().collect();
    let rows = statement.query_map(rusqlite::params_from_iter(params), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, f64>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, String>(10)?,
            row.get::<_, Option<String>>(11)?,
            row.get::<_, String>(12)?,
        ))
    })?;
    let mut feedback = store.feedback_totals_for(id)?;
    let mut items = Vec::new();
    for row in rows {
        let (
            id,
            kind,
            scope,
            title,
            body,
            trig,
            importance,
            created,
            updated,
            expires,
            status,
            sup,
            origin,
        ) = row?;
        items.push(Item {
            usage_window: Default::default(),
            feedback: feedback.remove(&id).unwrap_or_default(),
            id,
            kind: enum_of(&kind)?,
            scope,
            title,
            body,
            triggers: triggers(&trig),
            importance,
            created_at: iso(created),
            updated_at: iso(updated),
            expires_at: expires.map(iso),
            status: enum_of(&status)?,
            superseded_by: sup,
            origin: enum_of(&origin)?,
        });
    }
    Ok(items)
}

/// A selector context over the real workspace at `root` (no events, no split).
pub fn live_fixture(root: &Path, config: Config, items: Vec<Item>) -> Fixture {
    Fixture {
        dir: root.to_path_buf(),
        code_root: root.to_path_buf(),
        config,
        items,
        events: Vec::new(),
        split: Split {
            dev: Vec::new(),
            test: Vec::new(),
        },
    }
}
