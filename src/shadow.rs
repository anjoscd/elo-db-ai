//! Passive selector telemetry. No prompt, title, body or item id is persisted.
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::bench::{
    fixture::{Event, Fixture, Kind},
    selector::Selection,
};

const MAX_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowDecision {
    pub top: f64,
    pub gap: f64,
    pub matched_words: usize,
    pub would_abstain: bool,
}

impl ShadowDecision {
    pub fn new(top: f64, gap: f64, matched_words: usize, minimum: f64) -> Self {
        Self {
            top,
            gap,
            matched_words,
            would_abstain: top < minimum || (gap < 0.05 && matched_words < 2),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Record {
    pub session_id: String,
    pub event_seq: u64,
    pub at: String,
    #[serde(flatten)]
    pub decision: ShadowDecision,
    pub n_full_non_law: usize,
    pub n_pointers: usize,
    pub used_bytes: usize,
}

/// Best effort only: telemetry must never affect the hook result.
pub fn record(data_dir: &Path, fx: &Fixture, ev: &Event, selection: &Selection) {
    if !fx.config.shadow.enabled
        || !ev.allowed_scopes.is_empty()
        || ev.event_type != "user-prompt-submit"
    {
        return;
    }
    let Some(decision) = selection.shadow.clone() else {
        return;
    };
    let record = Record {
        session_id: ev.session_id.clone(),
        event_seq: ev.event_seq,
        at: ev.now.clone(),
        decision,
        n_full_non_law: selection
            .full_ids
            .iter()
            .filter(|id| {
                fx.items
                    .iter()
                    .any(|item| &item.id == *id && item.kind != Kind::Law)
            })
            .count(),
        n_pointers: selection.pointer_ids.len(),
        used_bytes: selection.used_bytes,
    };
    let _ = append(data_dir, &record);
}

fn append(data_dir: &Path, record: &Record) -> std::io::Result<()> {
    // NaN/Inf deliberately discard the entire record, just like an I/O failure.
    if !record.decision.top.is_finite() || !record.decision.gap.is_finite() {
        return Err(std::io::Error::other("non-finite shadow score"));
    }
    let dir = data_dir.join("shadow");
    fs::create_dir_all(&dir)?;
    // Nonblocking serialization across processes, including rotation. Contention skips
    // telemetry rather than adding latency to the hook.
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("selector.lock"))?;
    lock.try_lock().map_err(std::io::Error::other)?;
    let result = (|| {
        let path = dir.join("selector.jsonl");
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        if fs::metadata(&path).map_or(0, |m| m.len()) + line.len() as u64 > MAX_BYTES {
            fs::rename(&path, dir.join("selector.jsonl.1"))?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(&line)
    })();
    result
}

/// Include the retained rotation; malformed/truncated lines are ignored.
pub fn read(data_dir: &Path) -> Vec<Record> {
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(data_dir.join("shadow/selector.lock"))
        .ok();
    if let Some(lock) = &lock {
        let start = Instant::now();
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock)
                    if start.elapsed() < Duration::from_millis(200) =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                _ => break, // Best effort read after the bounded wait or an I/O error.
            }
        }
    }
    let mut records = Vec::new();
    for name in ["selector.jsonl.1", "selector.jsonl"] {
        if let Ok(file) = File::open(data_dir.join("shadow").join(name)) {
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                if let Ok(record) = serde_json::from_str::<Record>(&line) {
                    if record.decision.top.is_finite() && (0.0..=1.0).contains(&record.decision.top)
                    {
                        records.push(record);
                    }
                }
            }
        }
    }
    records
}

fn latest(records: &[Record]) -> Vec<&Record> {
    let mut unique = HashMap::new();
    for record in records {
        unique.insert((&record.session_id, record.event_seq), record);
    }
    unique.into_values().collect()
}

pub fn percentages(
    records: &[Record],
    now: i64,
    since: Option<i64>,
) -> (Option<f64>, Option<f64>, usize) {
    let mut skipped = 0;
    let recent: Vec<_> = latest(records)
        .into_iter()
        .filter(|r| match chrono::DateTime::parse_from_rfc3339(&r.at) {
            Ok(at) if at.timestamp() <= now => at.timestamp() >= since.unwrap_or(now - 30 * 86_400),
            _ => {
                skipped += 1;
                false
            }
        })
        .collect();
    let abstain: Vec<_> = recent.iter().filter(|r| r.decision.would_abstain).collect();
    (
        ratio(abstain.len(), recent.len()),
        ratio(
            abstain.iter().filter(|r| r.n_full_non_law > 0).count(),
            abstain.len(),
        ),
        skipped,
    )
}

fn ratio(n: usize, total: usize) -> Option<f64> {
    (total > 0).then(|| n as f64 / total as f64)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Bucket {
    pub events: usize,
    pub used: usize,
    pub ignored: usize,
    pub reported: usize,
}

/// Usage per event and the proportion of ignored reports among delivered items.
pub fn calibrate(
    conn: &rusqlite::Connection,
    records: &[Record],
) -> rusqlite::Result<[Bucket; 10]> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS shadow_calibration (
        session_id TEXT, event_seq INTEGER, bucket INTEGER,
        PRIMARY KEY(session_id, event_seq)); DELETE FROM shadow_calibration;",
    )?;
    {
        let mut insert = tx.prepare("INSERT INTO shadow_calibration VALUES (?1, ?2, ?3)")?;
        for record in latest(records) {
            let Ok(seq) = i64::try_from(record.event_seq) else {
                continue;
            };
            let bucket = ((record.decision.top * 10.0).floor() as usize).min(9);
            insert.execute(rusqlite::params![record.session_id, seq, bucket])?;
        }
    }
    let mut buckets = [Bucket::default(); 10];
    {
        let mut query = tx.prepare(
            "SELECT r.bucket, COUNT(*),
             SUM(EXISTS(SELECT 1 FROM auto_usage u
                 WHERE u.session_id=r.session_id AND u.event_seq=r.event_seq)),
             (SELECT COUNT(*) FROM agent_feedback f JOIN shadow_calibration c
                 ON c.session_id=f.session_id AND c.event_seq=f.event_seq
                 WHERE c.bucket=r.bucket AND f.verdict='ignored' AND EXISTS
                 (SELECT 1 FROM injections i WHERE i.session_id=f.session_id
                  AND i.event_seq=f.event_seq AND i.item_id=f.item_id)),
             (SELECT COUNT(*) FROM agent_feedback f JOIN shadow_calibration c
                 ON c.session_id=f.session_id AND c.event_seq=f.event_seq
                 WHERE c.bucket=r.bucket AND EXISTS
                 (SELECT 1 FROM injections i WHERE i.session_id=f.session_id
                  AND i.event_seq=f.event_seq AND i.item_id=f.item_id))
             FROM shadow_calibration r GROUP BY r.bucket",
        )?;
        let rows = query.query_map([], |r| {
            Ok((
                r.get::<_, usize>(0)?,
                Bucket {
                    events: r.get(1)?,
                    used: r.get(2)?,
                    ignored: r.get(3)?,
                    reported: r.get(4)?,
                },
            ))
        })?;
        for row in rows {
            let (index, bucket) = row?;
            buckets[index] = bucket;
        }
    }
    tx.execute_batch("DROP TABLE shadow_calibration")?;
    tx.commit()?;
    Ok(buckets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(top: f64, seq: u64) -> Record {
        Record {
            session_id: "session".into(),
            event_seq: seq,
            at: "2026-11-10T00:00:00Z".into(),
            decision: ShadowDecision::new(top, 0.1, 2, 0.55),
            n_full_non_law: 1,
            n_pointers: 2,
            used_bytes: 100,
        }
    }

    #[test]
    fn append_latency_100_calls() {
        let dir = tempfile::tempdir().unwrap();
        let mut times = Vec::new();
        for seq in 0..100 {
            let start = Instant::now();
            append(dir.path(), &sample(0.2, seq)).unwrap();
            times.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        eprintln!(
            "shadow append: mean {:.6} ms, p95 {:.6} ms (100 calls)",
            times.iter().sum::<f64>() / 100.0,
            times[94]
        );
    }

    #[test]
    fn latest_record_wins_and_time_filter_is_explicit() {
        let mut rows = vec![
            sample(0.1, 1),
            sample(0.9, 1),
            sample(0.1, 2),
            sample(0.1, 3),
        ];
        let now = chrono::DateTime::parse_from_rfc3339(&rows[0].at)
            .unwrap()
            .timestamp();
        rows[2].at = "invalid".into();
        rows[3].at = "2099-01-01T00:00:00Z".into();
        assert_eq!(percentages(&rows, now, None), (Some(0.0), None, 2));
        assert_eq!(percentages(&rows, now + 31 * 86400, None), (None, None, 2));
        assert_eq!(
            percentages(&rows, now + 31 * 86400, Some(now)),
            (Some(0.0), None, 2)
        );
    }

    #[test]
    fn read_waits_for_lock_and_falls_back_after_timeout() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample(0.1, 1)).unwrap();
        let lock = OpenOptions::new()
            .write(true)
            .open(dir.path().join("shadow/selector.lock"))
            .unwrap();
        lock.lock().unwrap();
        assert!(append(dir.path(), &sample(0.2, 2)).is_err());
        let start = Instant::now();
        assert_eq!(read(dir.path()).len(), 1);
        assert!(start.elapsed() >= Duration::from_millis(200));
        lock.unlock().unwrap();
        append(dir.path(), &sample(0.2, 2)).unwrap();
        assert_eq!(read(dir.path()).len(), 2);
    }

    #[test]
    fn config_defaults_override_and_validation() {
        let parse = |text| crate::config::Config::parse(text, Path::new("test.toml"));
        let default = parse("").unwrap().shadow;
        assert!(default.enabled);
        assert_eq!(default.abstain_min_score, 0.55);
        let custom = parse("[shadow]\nenabled=false\nabstain_min_score=0.7")
            .unwrap()
            .shadow;
        assert!(!custom.enabled);
        assert_eq!(custom.abstain_min_score, 0.7);
        assert!(parse("[shadow]\nabstain_min_score=1.1").is_err());
        assert!(parse("[shadow]\nabstain_min_score=nan").is_err());
    }

    #[test]
    fn decision_threshold_and_ambiguity() {
        assert!(ShadowDecision::new(0.2, 0.2, 2, 0.55).would_abstain);
        assert!(!ShadowDecision::new(0.9, 0.1, 2, 0.55).would_abstain);
        assert!(ShadowDecision::new(0.9, 0.01, 1, 0.55).would_abstain);
        assert!(!ShadowDecision::new(0.9, 0.01, 2, 0.55).would_abstain);
    }

    #[test]
    fn private_lines_rotation_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample(0.2, 1)).unwrap();
        let path = dir.path().join("shadow/selector.jsonl");
        let line = fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 10);
        for field in ["prompt", "title", "body", "full_ids", "rendered"] {
            assert!(json.get(field).is_none());
        }
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_BYTES).unwrap();
        append(dir.path(), &sample(0.8, 2)).unwrap();
        assert_eq!(
            fs::metadata(path.with_file_name("selector.jsonl.1"))
                .unwrap()
                .len(),
            MAX_BYTES
        );
        assert!(fs::metadata(&path).unwrap().len() < MAX_BYTES);
        assert_eq!(read(dir.path()).last().unwrap().event_seq, 2);
        assert!(append(&path, &sample(0.1, 3)).is_err());
    }

    #[test]
    fn record_skips_disabled_and_benchmark_and_swallows_errors() {
        let mut fx = crate::bench::fixture::load_fixture(Path::new("bench/fixture/v0")).unwrap();
        let mut ev = fx.events[0].clone();
        ev.event_type = "user-prompt-submit".into();
        ev.allowed_scopes.clear();
        let selection = Selection {
            shadow: Some(sample(0.1, 1).decision),
            ..Selection::default()
        };
        let dir = tempfile::tempdir().unwrap();
        fx.config.shadow.enabled = false;
        record(dir.path(), &fx, &ev, &selection);
        assert!(!dir.path().join("shadow").exists());
        fx.config.shadow.enabled = true;
        ev.allowed_scopes.push("global".into());
        record(dir.path(), &fx, &ev, &selection);
        assert!(!dir.path().join("shadow").exists());
        ev.allowed_scopes.clear();
        fs::write(dir.path().join("shadow"), "blocked").unwrap();
        record(dir.path(), &fx, &ev, &selection);
    }

    #[test]
    fn reports_require_the_item_to_be_delivered_in_the_same_event() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE auto_usage(session_id TEXT, event_seq INTEGER);
            CREATE TABLE injections(session_id TEXT, event_seq INTEGER, item_id TEXT);
            CREATE TABLE agent_feedback(session_id TEXT, event_seq INTEGER, item_id TEXT, verdict TEXT);
            INSERT INTO injections VALUES ('session',1,'a'), ('session',1,'a'), ('session',1,'b');
            INSERT INTO agent_feedback VALUES ('session',1,'a','ignored'), ('session',1,'b','used'),
                ('session',2,'a','ignored'), ('other',1,'a','ignored'), ('session',1,'c','ignored');").unwrap();
        let buckets = calibrate(&conn, &[sample(0.1, 1), sample(0.2, 2)]).unwrap();
        assert_eq!((buckets[1].ignored, buckets[1].reported), (1, 2));
        assert_eq!((buckets[2].ignored, buckets[2].reported), (0, 0));
    }

    #[test]
    fn calibration_counts_events_and_exact_boundaries() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE auto_usage(session_id TEXT, event_seq INTEGER);
            CREATE TABLE injections(session_id TEXT, event_seq INTEGER, item_id TEXT, at INTEGER);
            CREATE TABLE agent_feedback(session_id TEXT, event_seq INTEGER, item_id TEXT, verdict TEXT);
            INSERT INTO auto_usage VALUES ('session', 2), ('session', 2), ('other', 3);
            INSERT INTO injections VALUES ('session', 2, 'a', 10), ('session', 3, 'b', 10);
            INSERT INTO agent_feedback VALUES ('session',2,'a','ignored');
        ",
        )
        .unwrap();
        let rows = vec![
            sample(0.8, 1),
            sample(0.0, 1),
            sample(0.1, 2),
            sample(0.9, 3),
            sample(1.0, 4),
        ];
        let buckets = calibrate(&conn, &rows).unwrap();
        assert_eq!(buckets[0].events, 1);
        assert_eq!(
            (buckets[1].events, buckets[1].used, buckets[1].ignored),
            (1, 1, 1)
        );
        assert_eq!(
            (buckets[9].events, buckets[9].used, buckets[9].ignored),
            (2, 0, 0)
        );
        let now = chrono::DateTime::parse_from_rfc3339(&rows[0].at)
            .unwrap()
            .timestamp();
        assert_eq!(percentages(&rows, now, None), (Some(0.5), Some(1.0), 0));
        assert_eq!(percentages(&rows, now + 31 * 86_400, None), (None, None, 0));
    }
}
