//! How often each word appears in the user's own prompts (real prompts only).
//!
//! Only folded words and counts are kept, never prompt text. The table is optional and
//! created on first use, so the schema version does not change and older binaries
//! ignore it. The selector uses it to tell a name the user types all the time
//! ("Apple", "Android") from a rarer one ("Strava").
use std::{
    collections::{BTreeMap, BTreeSet},
    io::BufRead,
    path::Path,
};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// Below this many counted prompts the shares are not trusted and nothing is common.
pub const MIN_PROMPTS: u64 = 500;
/// A word needs at least this many prompts to be common, whatever its share.
pub const MIN_COUNT: u64 = 10;
/// A word in at least this share of the user's prompts is common for them. Measured on
/// 2,202 real prompts: platform names sat at 1.41-4.36% ("Android" 1.45%, "Apple"
/// 4.09%), tool and service names at 1.27% or less.
pub const COMMON_SHARE: f64 = 0.014;
/// At most this many distinct words are counted per prompt.
const MAX_WORDS: usize = 300;
const TOTAL_KEY: &str = "prompt_words_total";

/// Counts for the words of one prompt.
#[derive(Debug, Clone, Default)]
pub struct PromptWords {
    pub total: u64,
    pub counts: BTreeMap<String, u64>,
}

impl PromptWords {
    /// True when `word` (already folded) is in at least `COMMON_SHARE` of the counted
    /// prompts and in at least `MIN_COUNT` of them. Unknown words, or fewer than
    /// `MIN_PROMPTS` prompts, are never common. A compound keyword ("apple-pay") is
    /// common when every part of it is.
    pub fn is_common(&self, word: &str) -> bool {
        if self.total < MIN_PROMPTS {
            return false;
        }
        let mut parts = word
            .split(|c: char| !c.is_alphanumeric())
            .filter(|p| !p.is_empty());
        let common = |part: &str| {
            let count = self.counts.get(part).copied().unwrap_or(0);
            count >= MIN_COUNT && count as f64 / self.total as f64 >= COMMON_SHARE
        };
        match parts.next() {
            Some(first) => common(first) && parts.all(common),
            None => false,
        }
    }
}

/// Distinct folded words of a prompt, outside code (2 to 40 characters), at most
/// `MAX_WORDS` of them in order of first appearance.
pub fn words_of(prompt: &str) -> BTreeSet<String> {
    let mut words = BTreeSet::new();
    for word in crate::bench::elo_selector::prose_text(prompt)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| (2..=40).contains(&w.chars().count()))
    {
        if words.len() == MAX_WORDS {
            break;
        }
        words.insert(word.to_string());
    }
    words
}

/// Folded alphanumeric parts of every item keyword: the only words whose counts the
/// selector can ask for.
pub fn keyword_parts(items: &[crate::bench::fixture::Item]) -> BTreeSet<String> {
    items
        .iter()
        .flat_map(|item| &item.triggers.keywords)
        .flat_map(|k| {
            crate::bench::elo_selector::prose_text(k)
                .split(|c: char| !c.is_alphanumeric())
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn ensure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS prompt_words (\
             word    TEXT PRIMARY KEY,\
             prompts INTEGER NOT NULL CHECK (prompts >= 0)\
         );",
    )
}

fn total(conn: &Connection) -> rusqlite::Result<Option<u64>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [TOTAL_KEY], |r| {
        r.get::<_, String>(0)
    })
    .optional()
    .map(|v| v.and_then(|v| v.parse().ok()))
}

/// Counts for `words` (only the words the caller needs). A store without the table yet
/// reads as empty.
pub fn load(conn: &Connection, words: &BTreeSet<String>) -> rusqlite::Result<PromptWords> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'prompt_words')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(PromptWords::default());
    }
    let mut out = PromptWords {
        total: total(conn)?.unwrap_or(0),
        counts: BTreeMap::new(),
    };
    let mut stmt = conn.prepare_cached("SELECT prompts FROM prompt_words WHERE word = ?1")?;
    for word in words {
        if let Some(n) = stmt.query_row([word], |r| r.get::<_, i64>(0)).optional()? {
            out.counts
                .insert(word.clone(), u64::try_from(n).unwrap_or(0));
        }
    }
    Ok(out)
}

fn add(conn: &Connection, words: &BTreeSet<String>) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO prompt_words (word, prompts) VALUES (?1, 1) \
         ON CONFLICT(word) DO UPDATE SET prompts = prompts + 1",
    )?;
    for word in words {
        stmt.execute([word])?;
    }
    Ok(())
}

/// Counts one real prompt. The write lock is taken up front and the total is
/// incremented in SQL, so two hooks at once never lose a count.
pub fn record(conn: &mut Connection, words: &BTreeSet<String>) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure(&tx)?;
    add(&tx, words)?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, '1') \
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
        [TOTAL_KEY],
    )?;
    tx.commit()
}

/// The first time, counts the real prompts already in the shadow log
/// (`<root>/.elo/shadow/log.jsonl`), so the shares are useful from day one. Older lines
/// have no `automatic` field and count unless the prompt itself is an agent-runtime
/// message. The check runs inside a write transaction, so two hooks at once seed once.
/// Later calls do nothing. Unreadable lines are skipped.
pub fn seed_if_missing(conn: &mut Connection, root: &Path) -> rusqlite::Result<()> {
    if total(conn)?.is_some() {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if total(&tx)?.is_some() {
        return Ok(());
    }
    ensure(&tx)?;
    tx.execute("DELETE FROM prompt_words", [])?;
    let mut prompts = 0u64;
    let log = root.join(".elo").join("shadow").join("log.jsonl");
    if let Ok(file) = std::fs::File::open(&log) {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value
                .get("automatic")
                .and_then(|a| a.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(prompt) = value.get("prompt").and_then(|p| p.as_str()) else {
                continue;
            };
            if prompt.trim().is_empty() || crate::hook::is_automatic_prompt(prompt) {
                continue;
            }
            add(&tx, &words_of(prompt))?;
            prompts += 1;
        }
    }
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![TOTAL_KEY, prompts.to_string()],
    )?;
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        conn
    }

    #[test]
    fn missing_table_reads_as_empty_and_nothing_is_common() {
        let conn = conn();
        let words = words_of("Apple Apple");
        let pw = load(&conn, &words).unwrap();
        assert_eq!(pw.total, 0);
        assert!(!pw.is_common("apple"));
    }

    #[test]
    fn records_count_distinct_words_once_per_prompt() {
        let mut conn = conn();
        for prompt in ["abre a Apple", "Apple e Apple de novo", "outro pedido"] {
            record(&mut conn, &words_of(prompt)).unwrap();
        }
        let pw = load(&conn, &words_of("apple outro")).unwrap();
        assert_eq!(pw.total, 3);
        assert_eq!(pw.counts["apple"], 2);
        assert_eq!(pw.counts["outro"], 1);
    }

    #[test]
    fn common_needs_enough_prompts_and_the_share() {
        let mut pw = PromptWords {
            total: MIN_PROMPTS - 1,
            counts: BTreeMap::from([("apple".into(), 100)]),
        };
        assert!(!pw.is_common("apple"));
        pw.total = 1000;
        pw.counts.insert("strava".into(), 12);
        pw.counts.insert("garmin".into(), 14);
        assert!(pw.is_common("apple"));
        assert!(!pw.is_common("strava"));
        assert!(pw.is_common("garmin"));
        assert!(!pw.is_common("unknown"));
        // 8 of 500 is 1.6%, but fewer than MIN_COUNT prompts is never common.
        pw.total = 500;
        pw.counts.insert("docker".into(), 8);
        assert!(!pw.is_common("docker"));
        // A compound keyword is common only when every part is.
        pw.total = 1000;
        pw.counts.insert("pay".into(), 20);
        assert!(pw.is_common("apple-pay"));
        assert!(!pw.is_common("apple-strava"));
        assert!(!pw.is_common("-"));
    }

    #[test]
    fn at_most_max_words_distinct_words_even_with_repetitions() {
        let mut prompt = "repete ".repeat(500);
        for n in 0..400 {
            prompt.push_str(&format!("w{n} "));
        }
        let words = words_of(&prompt);
        assert_eq!(words.len(), MAX_WORDS);
        assert!(words.contains("repete"));
        assert!(words.contains("w298"));
    }

    #[test]
    fn old_log_lines_without_the_automatic_field_are_counted() {
        let dir = tempfile::TempDir::new().unwrap();
        let shadow = dir.path().join(".elo").join("shadow");
        std::fs::create_dir_all(&shadow).unwrap();
        std::fs::write(
            shadow.join("log.jsonl"),
            [
                r#"{"prompt":"abre a Apple"}"#,
                r#"{"prompt":"<agent-message> Apple"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let mut conn = conn();
        seed_if_missing(&mut conn, dir.path()).unwrap();
        let pw = load(&conn, &words_of("apple")).unwrap();
        assert_eq!((pw.total, pw.counts["apple"]), (1, 1));
    }

    #[test]
    fn two_connections_seed_once_and_never_lose_a_count() {
        let dir = tempfile::TempDir::new().unwrap();
        let shadow = dir.path().join(".elo").join("shadow");
        std::fs::create_dir_all(&shadow).unwrap();
        let lines: Vec<String> = (0..300)
            .map(|n| format!(r#"{{"automatic":false,"prompt":"Apple {n}"}}"#))
            .collect();
        std::fs::write(shadow.join("log.jsonl"), lines.join("\n")).unwrap();
        let db = dir.path().join("db.sqlite");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .unwrap();
        }
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (db, root) = (db.clone(), dir.path().to_path_buf());
                std::thread::spawn(move || {
                    let mut c = Connection::open(&db).unwrap();
                    c.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
                    seed_if_missing(&mut c, &root).unwrap();
                    for _ in 0..25 {
                        record(&mut c, &words_of("Apple")).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let c = Connection::open(&db).unwrap();
        let pw = load(&c, &words_of("apple")).unwrap();
        // Seeded once (300) plus 4 x 25 recorded prompts, for the total and the word.
        assert_eq!((pw.total, pw.counts["apple"]), (400, 400));
    }

    #[test]
    fn words_skip_code_and_fold_accents() {
        let words = words_of("Ação no `Config` e no Play\n```\nlet Item = 1;\n```");
        assert!(words.contains("acao"));
        assert!(words.contains("play"));
        assert!(!words.contains("config"));
        assert!(!words.contains("item"));
    }

    #[test]
    fn seed_reads_only_real_prompts_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let shadow = dir.path().join(".elo").join("shadow");
        std::fs::create_dir_all(&shadow).unwrap();
        std::fs::write(
            shadow.join("log.jsonl"),
            [
                r#"{"automatic":false,"prompt":"abre a Apple"}"#,
                r#"{"automatic":true,"prompt":"Apple"}"#,
                r#"{"automatic":false,"prompt":"<task-notification> Apple"}"#,
                r#"{"automatic":false,"prompt":""}"#,
                "not json",
                r#"{"automatic":false,"prompt":"Apple de novo"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let mut conn = conn();
        seed_if_missing(&mut conn, dir.path()).unwrap();
        seed_if_missing(&mut conn, dir.path()).unwrap();
        let pw = load(&conn, &words_of("apple")).unwrap();
        assert_eq!(pw.total, 2);
        assert_eq!(pw.counts["apple"], 2);
    }

    #[test]
    fn seed_without_a_log_marks_the_store_as_seeded() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut conn = conn();
        seed_if_missing(&mut conn, dir.path()).unwrap();
        assert_eq!(total(&conn).unwrap(), Some(0));
        record(&mut conn, &words_of("Apple")).unwrap();
        assert_eq!(load(&conn, &words_of("apple")).unwrap().total, 1);
    }
}
