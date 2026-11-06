//! Local scoreboard (SPEC §6.5): append-only records of what was injected and
//! what the user judged useful. Nothing here leaves the workspace.
use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, TransactionBehavior};

use crate::store::{now_seconds, Store, StoreError};

/// How an item reached the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Full,
    Pointer,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::Pointer => "pointer",
        }
    }
}

/// Grouping for the breakdown table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Kind,
    Scope,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Breakdown {
    pub full: u64,
    pub pointers: u64,
    pub auto_used: u64,
    pub good: u64,
    pub bad: u64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    pub events: u64,
    pub full: u64,
    pub pointers: u64,
    pub laws_full: u64,
    pub bytes_p50: Option<u64>,
    pub bytes_p95: Option<u64>,
    pub auto_used: u64,
    pub good: u64,
    pub bad: u64,
    pub groups: BTreeMap<String, Breakdown>,
}

impl Stats {
    /// Confirmed-useful full items ÷ full items emitted.
    pub fn used_rate(&self) -> Option<f64> {
        (self.full > 0).then(|| self.good as f64 / self.full as f64)
    }

    /// Bad reports ÷ all reports.
    pub fn bad_rate(&self) -> Option<f64> {
        let total = self.good + self.bad;
        (total > 0).then(|| self.bad as f64 / total as f64)
    }
}

impl Store {
    /// Records one injected event. Replaying the same `(session_id, event_seq)`
    /// is a no-op, so retries never double count.
    pub fn record_injection(
        &mut self,
        session_id: &str,
        event_seq: i64,
        used_bytes: u64,
        items: &[(&str, Mode)],
    ) -> Result<bool, StoreError> {
        let now = now_seconds();
        let used = i64::try_from(used_bytes).unwrap_or(i64::MAX);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO injected_events (session_id, event_seq, used_bytes, at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_id, event_seq, used, now],
        )?;
        if inserted == 0 {
            return Ok(false);
        }
        for (item_id, mode) in items {
            let found: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind, scope FROM items WHERE id = ?1",
                    [item_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (kind, scope) = found.ok_or_else(|| StoreError::UnknownItem((*item_id).into()))?;
            tx.execute(
                "INSERT INTO injections (session_id, event_seq, item_id, mode, kind, scope, at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    session_id,
                    event_seq,
                    item_id,
                    mode.as_str(),
                    kind,
                    scope,
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Records the user's verdict on an item (`elo good` / `elo bad`).
    pub fn record_feedback(&self, item_id: &str, good: bool) -> Result<(), StoreError> {
        // Kind and scope are copied so history survives the item being deleted later.
        let found: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT kind, scope FROM items WHERE id = ?1",
                [item_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (kind, scope) = found.ok_or_else(|| StoreError::UnknownItem(item_id.into()))?;
        self.conn.execute(
            "INSERT INTO feedback (item_id, verdict, kind, scope, at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                item_id,
                if good { "good" } else { "bad" },
                kind,
                scope,
                now_seconds()
            ],
        )?;
        Ok(())
    }

    /// Aggregates the scoreboard, optionally since a unix timestamp (seconds).
    pub fn stats(&self, since: Option<i64>, group: GroupBy) -> Result<Stats, StoreError> {
        let since = since.unwrap_or(i64::MIN);
        let column = match group {
            GroupBy::Kind => "kind",
            GroupBy::Scope => "scope",
        };
        let mut stats = Stats::default();

        let mut bytes: Vec<u64> = Vec::new();
        let mut statement = self
            .conn
            .prepare("SELECT used_bytes FROM injected_events WHERE at >= ?1 ORDER BY used_bytes")?;
        for row in statement.query_map([since], |row| row.get::<_, i64>(0))? {
            bytes.push(u64::try_from(row?).unwrap_or(0));
        }
        stats.events = bytes.len() as u64;
        let percentile = |p: usize| {
            bytes
                .get((bytes.len() * p).div_ceil(100).saturating_sub(1))
                .copied()
        };
        stats.bytes_p50 = percentile(50);
        stats.bytes_p95 = percentile(95);

        let sql = format!(
            "SELECT {column}, mode, kind, COUNT(*) FROM injections WHERE at >= ?1 GROUP BY {column}, mode, kind"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map([since], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (key, mode, kind, count) = row?;
            let count = u64::try_from(count).unwrap_or(0);
            let entry = stats.groups.entry(key).or_default();
            if mode == "full" {
                stats.full += count;
                entry.full += count;
                if kind == "law" {
                    stats.laws_full += count;
                }
            } else {
                stats.pointers += count;
                entry.pointers += count;
            }
        }

        let sql = format!(
            "SELECT {column}, verdict, COUNT(*) FROM feedback WHERE at >= ?1 \
             GROUP BY {column}, verdict"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map([since], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (key, verdict, count) = row?;
            let count = u64::try_from(count).unwrap_or(0);
            let entry = stats.groups.entry(key).or_default();
            if verdict == "good" {
                stats.good += count;
                entry.good += count;
            } else {
                stats.bad += count;
                entry.bad += count;
            }
        }
        let mut statement = self.conn.prepare(&format!(
            "SELECT u.{column}, COUNT(*) FROM auto_usage u
             WHERE EXISTS (SELECT 1 FROM injections i WHERE i.session_id=u.session_id
             AND i.event_seq=u.event_seq AND i.item_id=u.item_id AND i.mode='full' AND i.at>=?1)
             GROUP BY u.{column}"
        ))?;
        for row in statement.query_map([since], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })? {
            let (key, count) = row?;
            stats.auto_used += count;
            stats.groups.entry(key).or_default().auto_used += count;
        }
        Ok(stats)
    }
}

/// Accumulated scoreboard used by the selector, independent of the stats time filter.
#[derive(Debug, Default, Clone)]
pub struct Feedback {
    pub good: u64,
    pub bad: u64,
}

pub fn demoted(item: &crate::bench::fixture::Item, minimum: u64) -> bool {
    use crate::bench::fixture::{Kind, Origin};
    minimum > 0
        && item.triggers.always
        && !(item.kind == Kind::Law && item.origin == Origin::User)
        && item.feedback.bad >= minimum
        && item.feedback.good == 0
}

impl Store {
    /// Counts demotions without decoding unrelated item fields.
    pub fn demoted_count(&self, minimum: u64) -> Result<usize, StoreError> {
        if minimum == 0 {
            return Ok(0);
        }
        let feedback = self.feedback_totals()?;
        let mut statement = self
            .conn
            .prepare("SELECT id, kind, origin, triggers FROM items")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        Ok(rows
            .filter_map(Result::ok)
            .filter(|(id, kind, origin, triggers)| {
                !(kind == "law" && origin == "user")
                    && serde_json::from_str::<crate::bench::fixture::Triggers>(triggers)
                        .is_ok_and(|t| t.always)
                    && feedback
                        .get(id)
                        .is_some_and(|f| f.bad >= minimum && f.good == 0)
            })
            .count())
    }

    pub fn feedback_totals(&self) -> Result<BTreeMap<String, Feedback>, StoreError> {
        self.feedback_totals_for(None)
    }

    pub(crate) fn feedback_totals_for(
        &self,
        id: Option<&str>,
    ) -> Result<BTreeMap<String, Feedback>, StoreError> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'feedback')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(BTreeMap::new());
        }
        let sql = match id {
            Some(_) => "SELECT item_id, SUM(verdict = 'good'), SUM(verdict = 'bad') FROM feedback WHERE item_id = ?1 GROUP BY item_id",
            None => "SELECT item_id, SUM(verdict = 'good'), SUM(verdict = 'bad') FROM feedback GROUP BY item_id",
        };
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(id), |row| {
            Ok((
                row.get(0)?,
                Feedback {
                    good: row.get(1)?,
                    bad: row.get(2)?,
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod feedback_tests {
    use crate::store::{NewItem, Store};

    #[test]
    fn totals_for_filters_one_item_and_totals_includes_every_item() {
        let dir = tempfile::tempdir().unwrap();
        crate::init::init(dir.path()).unwrap();
        let store = Store::open(dir.path()).unwrap();
        for id in ["a", "b", "empty"] {
            store
                .insert_item(&NewItem {
                    id,
                    kind: "rule",
                    scope: "global",
                    title: "t",
                    body: "b",
                    origin: "agent",
                })
                .unwrap();
        }
        store.record_feedback("a", true).unwrap();
        store.record_feedback("a", true).unwrap();
        store.record_feedback("a", false).unwrap();
        store.record_feedback("b", false).unwrap();
        let all = store.feedback_totals().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!((all["a"].good, all["a"].bad), (2, 1));
        assert_eq!((all["b"].good, all["b"].bad), (0, 1));
        for id in ["a", "b"] {
            let one = store.feedback_totals_for(Some(id)).unwrap();
            assert_eq!(one.len(), 1);
            assert_eq!((one[id].good, one[id].bad), (all[id].good, all[id].bad));
        }
        for id in ["empty", "missing", "a' OR 1=1 --"] {
            assert!(store.feedback_totals_for(Some(id)).unwrap().is_empty());
        }
    }
}
