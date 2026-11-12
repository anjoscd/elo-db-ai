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
    /// Median and p95 pointers emitted per event (zero-pointer events are included).
    pub pointers_p50: Option<u64>,
    pub pointers_p95: Option<u64>,
    /// Fraction of pointer-mode injections whose item was already pointed earlier
    /// in the same session. None when no pointers were emitted.
    pub repeated_pointers: Option<f64>,
    pub auto_used: u64,
    pub good: u64,
    pub bad: u64,
    pub groups: BTreeMap<String, Breakdown>,
    pub agent_used: u64,
    pub agent_ignored: u64,
    pub agent_corroborated: u64,
    pub agent_contradicted: u64,
    pub agent_agree: u64,
    pub agent_disagree: u64,
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
    /// Records an batch of agent-reported usefulness verdicts.
    /// Returns `(accepted, rejected)` item ids. An id is rejected if it was not
    /// delivered in the session within the last `window` events.
    pub fn record_agent_feedback(
        &self,
        session_id: &str,
        used: &[String],
        ignored: &[String],
        window: u64,
    ) -> Result<(Vec<String>, Vec<String>), StoreError> {
        if used.len() > 20 || ignored.len() > 20 {
            return Err(StoreError::Io {
                path: "feedback".into(),
                source: std::io::Error::other("too many ids (max 20 per list)"),
            });
        }
        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        let current_seq = if window == 0 {
            None
        } else {
            self.current_session_seq(session_id)?
        };
        let Some(current_seq) = current_seq else {
            rejected.extend(used.iter().chain(ignored.iter()).cloned());
            return Ok((accepted, rejected));
        };
        let lower =
            current_seq.saturating_sub(i64::try_from(window.saturating_sub(1)).unwrap_or(i64::MAX));
        let mut stmt = self.conn.prepare(
            "SELECT item_id, MAX(event_seq) FROM injections WHERE session_id = ?1 \
             AND event_seq >= ?2 AND event_seq <= ?3 GROUP BY item_id",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id, lower, current_seq], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let deliveries: std::collections::HashMap<String, i64> = rows.collect::<Result<_, _>>()?;
        let now = now_seconds();
        for id in used {
            match deliveries.get(id.as_str()) {
                Some(&event_seq) => {
                    self.record_verdict(session_id, event_seq, id, "used", now)?;
                    accepted.push(id.clone());
                }
                None => rejected.push(id.clone()),
            }
        }
        for id in ignored {
            match deliveries.get(id.as_str()) {
                Some(&event_seq) => {
                    self.record_verdict(session_id, event_seq, id, "ignored", now)?;
                    accepted.push(id.clone());
                }
                None => rejected.push(id.clone()),
            }
        }
        Ok((accepted, rejected))
    }

    /// Current sequence of the session, combining `.elo/sessions/<id>.json` and recorded tables
    /// so sessions that advance without new deliveries still age out old items.
    fn current_session_seq(&self, session_id: &str) -> Result<Option<i64>, StoreError> {
        let db_seq: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(seq) FROM ( \
                     SELECT MAX(event_seq) AS seq FROM injected_events WHERE session_id = ?1 \
                     UNION ALL \
                     SELECT MAX(event_seq) AS seq FROM injections WHERE session_id = ?1 \
                     UNION ALL \
                     SELECT MAX(event_seq) AS seq FROM auto_usage WHERE session_id = ?1 \
                 )",
                [session_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        let file_seq = self.session_file_seq(session_id);
        Ok(match (file_seq, db_seq) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        })
    }

    fn session_file_seq(&self, session_id: &str) -> Option<i64> {
        let parent = self.path.parent()?;
        let name = format!("{}.json", crate::hook::safe_id(session_id));
        for dir in [
            parent.join("sessions"),
            parent.join(crate::store::DATA_DIR).join("sessions"),
        ] {
            if let Ok(text) = std::fs::read_to_string(dir.join(&name)) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(seq) = v.get("seq").and_then(serde_json::Value::as_u64) {
                        return Some(i64::try_from(seq).unwrap_or(i64::MAX));
                    }
                }
            }
        }
        None
    }

    /// One objective signal for the same item in this session corroborates a `"used"` report.
    fn has_auto_usage(&self, session_id: &str, item_id: &str) -> Result<bool, StoreError> {
        let exists = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM auto_usage WHERE session_id = ?1 AND item_id = ?2)",
                rusqlite::params![session_id, item_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(exists.unwrap_or(false))
    }

    /// Verdicts are append-only per (session, delivery, item): retries never double count.
    fn record_verdict(
        &self,
        session_id: &str,
        event_seq: i64,
        item_id: &str,
        verdict: &'static str,
        now: i64,
    ) -> Result<(), StoreError> {
        let corroborated = verdict == "used" && self.has_auto_usage(session_id, item_id)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO agent_feedback \
             (session_id, event_seq, item_id, verdict, corroborated, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                session_id,
                event_seq,
                item_id,
                verdict,
                if corroborated { 1 } else { 0 },
                now
            ],
        )?;
        Ok(())
    }

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

        // Pointers per event: count zero-pointer events too, using injected_events as the base.
        let mut pointers_per_event: Vec<u64> = Vec::new();
        let mut statement = self.conn.prepare(
            "SELECT COUNT(i.item_id) \
             FROM injected_events e \
             LEFT JOIN injections i ON e.session_id = i.session_id AND e.event_seq = i.event_seq AND i.mode = 'pointer' \
             WHERE e.at >= ?1 \
             GROUP BY e.session_id, e.event_seq")?;
        for row in statement.query_map([since], |row| row.get::<_, i64>(0))? {
            pointers_per_event.push(u64::try_from(row?).unwrap_or(0));
        }
        pointers_per_event.sort_unstable();
        let percentile = |p: usize| {
            pointers_per_event
                .get(
                    (pointers_per_event.len() * p)
                        .div_ceil(100)
                        .saturating_sub(1),
                )
                .copied()
        };
        stats.pointers_p50 = percentile(50);
        stats.pointers_p95 = percentile(95);

        // Fraction of pointers whose item was already pointed earlier in the same session.
        // The LAG window spans the whole history so the previous emission can be seen even
        // when it falls outside the `--since` window. Compaction/session-start clear the
        // runtime `pointed` state, but that reset is not persisted, so the metric may
        // over-count repeats across compaction boundaries. This is noted in `elo stats` output.
        let repeated: Option<(i64, i64)> = self.conn.query_row(
            "SELECT COUNT(*) AS total, COUNT(CASE WHEN h.prev_seq IS NOT NULL THEN 1 END) AS repeated \
             FROM ( \
               SELECT session_id, event_seq, item_id, mode, at \
               FROM injections WHERE at >= ?1 AND mode = 'pointer' \
             ) f \
             JOIN ( \
               SELECT session_id, item_id, event_seq, \
                 LAG(event_seq) OVER (PARTITION BY session_id, item_id ORDER BY event_seq, rowid) AS prev_seq \
               FROM injections WHERE mode = 'pointer' \
             ) h \
             ON f.session_id = h.session_id AND f.event_seq = h.event_seq AND f.item_id = h.item_id",
            [since],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        stats.repeated_pointers = repeated
            .and_then(|(total, repeated)| (total > 0).then(|| repeated as f64 / total as f64));

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
        // Objective signals can arrive after the agent's report: lift the flag for "used"
        // reports now so the scoreboard stays honest without a second pass by hand.
        self.conn.execute(
            "UPDATE agent_feedback SET corroborated = 1 \
             WHERE verdict = 'used' AND corroborated = 0 \
               AND EXISTS (SELECT 1 FROM auto_usage au \
                           WHERE au.session_id = agent_feedback.session_id \
                             AND au.item_id = agent_feedback.item_id)",
            [],
        )?;
        self.conn.execute(
            "UPDATE agent_feedback SET corroborated = 0 \
             WHERE verdict = 'ignored' AND corroborated != 0",
            [],
        )?;
        self.load_agent_stats(since, &mut stats)?;
        Ok(stats)
    }

    fn load_agent_stats(&self, since: i64, stats: &mut Stats) -> Result<(), StoreError> {
        let (used, ignored, corroborated, contradicted) = self.conn.query_row(
            "SELECT SUM(verdict = 'used'), \
                    SUM(verdict = 'ignored'), \
                    SUM(verdict = 'used' AND corroborated != 0), \
                    SUM(verdict = 'ignored' AND EXISTS ( \
                        SELECT 1 FROM auto_usage au \
                        WHERE au.session_id = agent_feedback.session_id \
                          AND au.item_id = agent_feedback.item_id \
                    )) \
             FROM agent_feedback WHERE at >= ?1",
            [since],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(0) as u64,
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0) as u64,
                    row.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
                    row.get::<_, Option<i64>>(3)?.unwrap_or(0) as u64,
                ))
            },
        )?;
        stats.agent_used = used;
        stats.agent_ignored = ignored;
        stats.agent_corroborated = corroborated;
        stats.agent_contradicted = contradicted;
        let mut stmt = self.conn.prepare(
            "SELECT a.item_id, a.verdict, a.n, f.g, f.b \
             FROM ( \
                 SELECT item_id, verdict, COUNT(*) AS n \
                 FROM agent_feedback WHERE at >= ?1 \
                 GROUP BY item_id, verdict \
             ) a \
             JOIN ( \
                 SELECT item_id, \
                        COALESCE(SUM(verdict = 'good'), 0) AS g, \
                        COALESCE(SUM(verdict = 'bad'), 0) AS b \
                 FROM feedback \
                 GROUP BY item_id \
             ) f ON f.item_id = a.item_id",
        )?;
        let rows = stmt.query_map([since], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (_id, verdict, count, good, bad) = row?;
            let count = u64::try_from(count).unwrap_or(0);
            let owner = if good > bad {
                "good"
            } else if bad > good {
                "bad"
            } else {
                continue;
            };
            let agree =
                (verdict == "used" && owner == "good") || (verdict == "ignored" && owner == "bad");
            if agree {
                stats.agent_agree += count;
            } else {
                stats.agent_disagree += count;
            }
        }
        Ok(())
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

#[cfg(test)]
mod stats_query_tests {
    use crate::stats::{GroupBy, Mode};
    use crate::store::{NewItem, Store};

    fn store_with_rule(dir: &tempfile::TempDir) -> (Store, String) {
        let store = Store::create_at(&dir.path().join("elo.db")).unwrap();
        store
            .insert_item(&NewItem {
                id: "R1",
                kind: "rule",
                scope: "global",
                title: "t",
                body: "b",
                origin: "agent",
            })
            .unwrap();
        (store, "R1".to_string())
    }

    #[test]
    fn pointers_per_event_counts_zero_pointer_events() {
        let dir = tempfile::tempdir().unwrap();
        crate::init::init(dir.path()).unwrap();
        let (mut store, _) = store_with_rule(&dir);
        // Event 1: one pointer.
        store
            .record_injection("s1", 1, 100, &[("R1", Mode::Pointer)])
            .unwrap();
        // Event 2: full only (zero pointers).
        store
            .record_injection("s1", 2, 100, &[("R1", Mode::Full)])
            .unwrap();
        // Event 3: no items at all (still an recorded event).
        store.record_injection("s1", 3, 0, &[]).unwrap();

        let st = store.stats(None, GroupBy::Kind).unwrap();
        assert_eq!(st.events, 3);
        // Sorted counts: [0, 0, 1]. p50 = 0, p95 = 1.
        assert_eq!(st.pointers_p50, Some(0));
        assert_eq!(st.pointers_p95, Some(1));
    }

    #[test]
    fn repeated_pointers_see_history_outside_since_window() {
        let dir = tempfile::tempdir().unwrap();
        crate::init::init(dir.path()).unwrap();
        let (mut store, _) = store_with_rule(&dir);
        store
            .record_injection("s1", 1, 100, &[("R1", Mode::Pointer)])
            .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        store
            .record_injection("s1", 2, 100, &[("R1", Mode::Pointer)])
            .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        store
            .record_injection("s1", 3, 100, &[("R1", Mode::Pointer)])
            .unwrap();

        let since = store
            .conn
            .query_row(
                "SELECT at FROM injected_events ORDER BY at DESC LIMIT 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        // Query only the last event; the previous two fall before `since` but are still
        // visible to the LAG window, so the last pointer counts as repeated.
        let st = store.stats(Some(since), GroupBy::Kind).unwrap();
        assert_eq!(st.events, 1);
        assert_eq!(st.repeated_pointers, Some(1.0));
    }
}

/// Rolling window summary; fixtures do not load these data.
#[derive(Debug, Default, Clone)]
pub struct UsageWindow {
    pub full_events: u64,
    pub auto_used: u64,
    pub agent_used: u64,
}

impl UsageWindow {
    pub fn demoted(&self, kind: crate::bench::fixture::Kind, minimum: u64) -> bool {
        use crate::bench::fixture::Kind;
        minimum > 0
            && matches!(kind, Kind::Decision | Kind::Rule | Kind::Note)
            && self.full_events >= minimum
            && self.auto_used == 0
            && self.agent_used == 0
    }
}

const UNUSED_WINDOWS_SQL: &str = "WITH signals AS (
                    SELECT item_id, COUNT(*) AS full_count,
                        0 AS auto_count, 0 AS agent_count
                    FROM (
                        SELECT DISTINCT item_id, session_id, event_seq
                        FROM injections WHERE mode='full'
                            AND kind IN ('decision','rule','note') AND at >= ?1 AND at <= ?2
                    ) GROUP BY item_id
                    UNION ALL
                    SELECT item_id, 0, COUNT(*), 0 FROM auto_usage
                    WHERE at >= ?1 AND at <= ?2 GROUP BY item_id
                    UNION ALL
                    SELECT item_id, 0, 0, COUNT(*) FROM agent_feedback
                    WHERE at >= ?1 AND at <= ?2 AND verdict='used' GROUP BY item_id
                )
                SELECT item_id, SUM(full_count), SUM(auto_count), SUM(agent_count)
                FROM signals GROUP BY item_id";

impl Store {
    /// One aggregate query per event, without correlated queries per item.
    /// Old or incomplete schemas leave all items without demotion.
    pub fn unused_windows(&self, now: i64, days: u32) -> BTreeMap<String, UsageWindow> {
        let query = || -> rusqlite::Result<BTreeMap<String, UsageWindow>> {
            let mut statement = self.conn.prepare(UNUSED_WINDOWS_SQL)?;
            let rows = statement
                .query_map(
                    rusqlite::params![now.saturating_sub(i64::from(days) * 86400), now],
                    |row| {
                        Ok((
                            row.get(0)?,
                            UsageWindow {
                                full_events: row.get(1)?,
                                auto_used: row.get(2)?,
                                agent_used: row.get(3)?,
                            },
                        ))
                    },
                )?
                .collect();
            rows
        };
        query().unwrap_or_default()
    }
}

#[test]
fn unused_windows_plan_uses_indexed_time_range() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::create(dir.path()).unwrap();
    let plan = store
        .conn
        .prepare(&format!("EXPLAIN QUERY PLAN {UNUSED_WINDOWS_SQL}"))
        .unwrap()
        .query_map([0, 1000001], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let plan = plan.join("\n");
    eprintln!("{plan}");
    // SQLite versions differ in whether a partial index is labeled covering.
    assert!(
        plan.replace("USING COVERING INDEX", "USING INDEX")
            .contains(
                "SEARCH injections USING INDEX injections_unused_window (mode=? AND at>? AND at<?)"
            ),
        "{plan}"
    );
}
