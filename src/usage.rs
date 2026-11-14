//! Automatic usefulness is evidence from tool inputs, never an owner's verdict.
use std::collections::{BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::bench::fixture::Triggers;
use crate::store::{now_seconds, StoreError, DATA_DIR, DB_FILE, SCHEMA_VERSION};

/// Each tool call is two events (pre and post), so 7 covers about three tool calls.
pub const DEFAULT_WINDOW: u64 = 7;

#[derive(Default, Debug)]
pub struct Touches {
    pub paths: Vec<String>,
    pub identifiers: BTreeSet<String>,
}

fn identifier(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
}

/// Keywords imported from other memory tools are often file names (`guard.rs`, `src/hook.rs`).
fn file_like(word: &str) -> bool {
    !word.contains(char::is_whitespace)
        && (word.contains('/')
            || word.rsplit_once('.').is_some_and(|(stem, ext)| {
                !stem.is_empty() && !ext.is_empty() && ext.chars().all(char::is_alphanumeric)
            }))
}

/// `path` ends with the whole `suffix` component(s): `src/guard.rs` matches `guard.rs`,
/// `myguard.rs` does not.
fn path_ends_with(path: &str, suffix: &str) -> bool {
    let suffix = suffix.trim_start_matches("./").trim_start_matches('/');
    path == suffix || path.ends_with(&format!("/{suffix}"))
}

fn normalize(root: &Path, cwd: &Path, value: &str, existing: bool) -> Option<String> {
    let path = cwd.join(value);
    if existing && !path.is_file() {
        return None;
    }
    // Resolve existing ancestors as well: a new file inside an escaping symlink
    // must not turn into a workspace touch.
    let mut ancestor = path.as_path();
    let mut suffix = Vec::new();
    let mut resolved = loop {
        if let Ok(path) = ancestor.canonicalize() {
            break path;
        }
        suffix.push(ancestor.file_name()?);
        ancestor = ancestor.parent()?;
    };
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    let mut clean = PathBuf::new();
    for part in resolved.components() {
        match part {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    let root = root.canonicalize().ok()?;
    let relative = clean.strip_prefix(root).ok()?;
    (!relative.as_os_str().is_empty()).then(|| relative.to_string_lossy().replace('\\', "/"))
}

/// Small shell lexer: quotes/escapes are supported; nothing is evaluated.
fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for c in command.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if quote == Some(c) {
            quote = None;
            continue;
        }
        if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
            continue;
        }
        if quote.is_none() && (c.is_whitespace() || ";|&()<>=`".contains(c)) {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

pub fn extract(root: &Path, cwd: &Path, tool: &str, input: &Value) -> Touches {
    let mut touched = Touches::default();
    // Only file tools touch what their path names; `Grep`/`Glob` with `path` only search.
    let file_tool = ["read", "edit", "write", "multiedit", "notebookedit"]
        .iter()
        .any(|t| tool.eq_ignore_ascii_case(t));
    for key in ["file_path", "path", "notebook_path", "paths"] {
        if !file_tool && matches!(key, "path" | "paths") {
            continue;
        }
        let values: Vec<&str> = match &input[key] {
            Value::String(s) => vec![s],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        for value in values {
            if let Some(path) = normalize(root, cwd, value, false) {
                touched.paths.push(path);
            }
        }
    }
    if tool.eq_ignore_ascii_case("bash") {
        for word in shell_words(input["command"].as_str().unwrap_or_default()) {
            if let Some(path) = normalize(root, cwd, &word, true) {
                touched.paths.push(path);
            }
        }
    }
    if tool.eq_ignore_ascii_case("edit") && !touched.paths.is_empty() {
        // Only what the agent had to find counts; text it writes (new_string) is a citation.
        for key in ["old_string"] {
            for word in input[key]
                .as_str()
                .unwrap_or_default()
                .split(|c: char| c != '_' && !c.is_alphanumeric())
            {
                if identifier(word) {
                    touched.identifiers.insert(word.to_owned());
                }
            }
        }
    }
    touched.paths.sort();
    touched.paths.dedup();
    touched
}

/// No migration, no database creation, no lock wait on the hot path. One inserted
/// row per delivery; the primary key makes retries and concurrent tools harmless.
pub fn record(
    root: &Path,
    session: &str,
    seq: u64,
    window: u64,
    touches: &Touches,
) -> Result<(), StoreError> {
    if touches.paths.is_empty() && touches.identifiers.is_empty() {
        return Ok(());
    }
    let mut conn = Connection::open_with_flags(
        root.join(DATA_DIR).join(DB_FILE),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    // Short wait: parallel tools write at the same time; never long enough to stall a hook.
    conn.busy_timeout(Duration::from_millis(100))?;
    let version: String = conn.query_row(
        "SELECT value FROM meta WHERE key='schema_version'",
        [],
        |r| r.get(0),
    )?;
    if version != SCHEMA_VERSION.to_string() {
        return Ok(());
    }
    let seq = i64::try_from(seq).unwrap_or(i64::MAX);
    let lower = seq.saturating_sub(i64::try_from(window).unwrap_or(i64::MAX));
    let mut query = conn.prepare(
        "SELECT i.event_seq, i.item_id, i.kind, i.scope, t.triggers FROM injections i
         JOIN items t ON t.id=i.item_id
         WHERE i.session_id=?1 AND i.event_seq>=?2 AND i.event_seq<?3 AND i.mode='full'
         AND NOT EXISTS (SELECT 1 FROM auto_usage u WHERE u.session_id=i.session_id
         AND u.event_seq=i.event_seq AND u.item_id=i.item_id)",
    )?;
    let rows = query.query_map(rusqlite::params![session, lower, seq], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut signals = Vec::new();
    for row in rows {
        let (delivery, id, kind, scope, json) = row?;
        let Ok(triggers) = serde_json::from_str::<Triggers>(&json) else {
            continue;
        };
        let matched = triggers.paths.iter().any(|pattern| {
            touches.paths.iter().any(|path| {
                pattern
                    .strip_suffix('*')
                    .map_or(pattern == path, |prefix| path.starts_with(prefix))
            })
        }) || triggers.keywords.iter().any(|word| {
            (identifier(word) && touches.identifiers.contains(word))
                || (file_like(word) && touches.paths.iter().any(|path| path_ends_with(path, word)))
        });
        if matched {
            signals.push((delivery, id, kind, scope));
        }
    }
    drop(query);
    if !signals.is_empty() {
        // Batch matching deliveries in one commit: fsync once, not once per item.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut insert = tx.prepare("INSERT OR IGNORE INTO auto_usage(session_id,event_seq,item_id,kind,scope,at) VALUES (?1,?2,?3,?4,?5,?6)")?;
            let now = now_seconds();
            for (delivery, id, kind, scope) in signals {
                insert.execute(rusqlite::params![session, delivery, id, kind, scope, now])?;
            }
        }
        tx.commit()?;
    }
    Ok(())
}

/// A response body is capped so the hook stays cheap even when the agent writes a lot.
const MAX_ANALYZED: usize = 64 * 1024;

/// Function words in English and Portuguese: too common to prove a rule was followed.
const COMMON: &[&str] = &[
    "about",
    "after",
    "against",
    "agora",
    "ainda",
    "alguma",
    "alguns",
    "aliás",
    "alterar",
    "always",
    "antes",
    "apenas",
    "around",
    "arquivo",
    "arquivos",
    "através",
    "because",
    "before",
    "between",
    "change",
    "changes",
    "codigo",
    "coisas",
    "comando",
    "commit",
    "como",
    "config",
    "conteúdo",
    "código",
    "depois",
    "deve",
    "devem",
    "different",
    "durante",
    "enquanto",
    "então",
    "everything",
    "exactly",
    "fazer",
    "finalmente",
    "funcao",
    "função",
    "given",
    "however",
    "including",
    "incluir",
    "instead",
    "maybe",
    "mesma",
    "mesmo",
    "multiple",
    "never",
    "nothing",
    "outras",
    "outros",
    "porque",
    "porém",
    "possível",
    "precisa",
    "primeiro",
    "project",
    "projeto",
    "qualquer",
    "quando",
    "really",
    "result",
    "resultado",
    "retorno",
    "return",
    "script",
    "sempre",
    "server",
    "servidor",
    "should",
    "similar",
    "sobre",
    "something",
    "sometimes",
    "string",
    "struct",
    "também",
    "teste",
    "testes",
    "tests",
    "through",
    "todas",
    "todos",
    "update",
    "usually",
    "various",
    "without",
    "would",
];

/// Token characters keep `src/hook.rs` and `full_notes_max` in one piece.
fn separator(c: char) -> bool {
    !(c.is_alphanumeric() || matches!(c, '/' | '_' | '.'))
}

/// Sentence punctuation is not part of a word: `shipping.` and `src/` are `shipping` and `src`.
fn trim_marks(token: &str) -> &str {
    token.trim_matches(|c| matches!(c, '/' | '_' | '.'))
}

/// Cuts at most `limit` bytes without splitting a character.
fn head(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Carries a digit or an inner `/`, `_`, `.`: an identifier, a path, a version.
fn marked(token: &str) -> bool {
    token.chars().any(|c| c.is_ascii_digit())
        || token.contains('/')
        || token.contains('_')
        || token
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty())
}

/// Long or marked words that are specific enough to be looked for in a response.
fn distinctive(lower: &str) -> bool {
    let length = lower.chars().count();
    if length >= 2 && lower.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    if length < 3 || !lower.chars().any(char::is_alphanumeric) {
        return false;
    }
    if marked(lower) {
        return true;
    }
    length >= 5 && lower.chars().all(char::is_alphabetic) && !COMMON.contains(&lower)
}

fn distinctive_terms(body: &str) -> BTreeSet<String> {
    let mut terms = BTreeSet::new();
    for token in head(body, MAX_ANALYZED).split(separator) {
        let lower = trim_marks(token).to_lowercase();
        if distinctive(&lower) {
            terms.insert(lower);
        }
    }
    terms
}

/// The id as a whole word: `n01` inside `version01` or `n0` inside `n0-b` is not a citation.
fn cites_id(item_id: &str, response: &str) -> bool {
    if item_id.is_empty() {
        return false;
    }
    let part = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_');
    response.match_indices(item_id).any(|(at, _)| {
        let before = response[..at].chars().next_back();
        let after = response[at + item_id.len()..].chars().next();
        !before.is_some_and(part) && !after.is_some_and(part)
    })
}

/// Long bodies are applied, not recited: this many shared terms is enough on its own.
const ENOUGH_TERMS: usize = 8;

/// Evidence that a response followed an item without the item naming a file:
/// the item's id or a large enough share of its distinctive terms.
pub fn response_uses(item_id: &str, body: &str, response: &str) -> bool {
    // One pass over the response, never a scan per term: this runs on the hook path.
    let response = head(response, MAX_ANALYZED);
    if response.trim().is_empty() {
        return false;
    }
    if cites_id(item_id, response) {
        return true;
    }
    let terms = distinctive_terms(body);
    // Two shared words are a coincidence, not a citation.
    if terms.len() < 3 {
        return false;
    }
    let mut seen = HashSet::new();
    for token in response.split(separator) {
        seen.insert(trim_marks(token).to_lowercase());
    }
    let mut matched = 0;
    let mut non_numeric = 0;
    for term in terms.iter().filter(|term| seen.contains(term.as_str())) {
        matched += 1;
        if !term.chars().all(char::is_numeric) {
            non_numeric += 1;
        }
    }
    // Numbers reinforce evidence but cannot establish it on their own.
    non_numeric >= 2 && matched >= 3 && (matched * 5 >= terms.len() || matched >= ENOUGH_TERMS)
}

/// Same opening, timeout and schema check as [`record`]; the evidence comes from the
/// response text instead of tool input, so laws and decisions stop scoring 0%.
pub fn record_response(
    root: &Path,
    session: &str,
    seq: u64,
    window: u64,
    response: &str,
) -> Result<usize, StoreError> {
    if response.trim().is_empty() {
        return Ok(0);
    }
    let mut conn = Connection::open_with_flags(
        root.join(DATA_DIR).join(DB_FILE),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    // Short wait: parallel tools write at the same time; never long enough to stall a hook.
    conn.busy_timeout(Duration::from_millis(100))?;
    let version: String = conn.query_row(
        "SELECT value FROM meta WHERE key='schema_version'",
        [],
        |r| r.get(0),
    )?;
    if version != SCHEMA_VERSION.to_string() {
        return Ok(0);
    }
    let seq = i64::try_from(seq).unwrap_or(i64::MAX);
    let lower = seq.saturating_sub(i64::try_from(window).unwrap_or(i64::MAX));
    let mut query = conn.prepare(
        "SELECT i.event_seq, i.item_id, i.kind, i.scope, t.body FROM injections i
         JOIN items t ON t.id=i.item_id
         WHERE i.session_id=?1 AND i.event_seq>=?2 AND i.event_seq<?3 AND i.mode='full'
         AND NOT EXISTS (SELECT 1 FROM auto_usage u WHERE u.session_id=i.session_id
         AND u.event_seq=i.event_seq AND u.item_id=i.item_id)",
    )?;
    let rows = query.query_map(rusqlite::params![session, lower, seq], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?.unwrap_or_default(),
        ))
    })?;
    let mut signals = Vec::new();
    for row in rows {
        let (delivery, id, kind, scope, body) = row?;
        if response_uses(&id, &body, response) {
            signals.push((delivery, id, kind, scope));
        }
    }
    drop(query);
    if signals.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut inserted = 0usize;
    {
        let mut insert = tx.prepare("INSERT OR IGNORE INTO auto_usage(session_id,event_seq,item_id,kind,scope,at) VALUES (?1,?2,?3,?4,?5,?6)")?;
        let now = now_seconds();
        for (delivery, id, kind, scope) in signals {
            inserted +=
                insert.execute(rusqlite::params![session, delivery, id, kind, scope, now])?;
        }
    }
    tx.commit()?;
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_only_reinforce_non_numeric_evidence() {
        let body = "Schedule shipment on 2026-11-05 with limit 42 and quota 75";
        assert!(!response_uses("item", body, "2026-11-05 42 75"));
        assert!(!response_uses("item", body, "shipment 2026-11-05 42 75"));
        assert!(response_uses("item", body, "limit quota 42"));
        assert!(!response_uses(
            "item",
            "limit quota 10 20 30 40 50 60 70 80",
            "10 20 30 40 50 60 70 80"
        ));
    }

    #[test]
    fn shorter_distinctive_terms_support_rephrased_responses() {
        assert!(distinctive("42"));
        assert!(distinctive("100"));
        assert!(distinctive("limit"));
        assert!(!distinctive("4"));
        assert!(!distinctive("a2"));
        assert!(!distinctive("four"));
        assert!(!distinctive("about"));
        assert!(response_uses(
            "item",
            "The limit is 42 while the quota is 75",
            "Set quota to 75 and limit to 42"
        ));
        assert!(!response_uses("item", "limit quota 42", "limit 42"));
    }

    #[test]
    fn one_fifth_of_terms_is_required_below_the_shortcut() {
        let body = "alpha bravo delta gamma theta kappa sigma omega cloud plane train crane stone brick steel";
        assert_eq!(distinctive_terms(body).len(), 15);
        assert!(response_uses("item", body, "alpha bravo delta"));
        assert!(!response_uses("item", body, "alpha bravo"));
        assert!(!response_uses(
            "item",
            &format!("{body} copper"),
            "alpha bravo delta"
        ));
    }

    #[test]
    fn literal_id_counts_even_without_a_body() {
        assert!(response_uses("lev-1", "", "the rule lev-1 applies here"));
        assert!(!response_uses("lev-1", "", "nothing to see"));
    }

    #[test]
    fn three_shared_terms_are_enough() {
        let body =
            "When compiling, always run the checksum 84276fb and read src/hook.rs before shipping.";
        let response = "I read src/hook.rs and verified 84276fb before shipping the crate.";
        assert!(response_uses("item-1", body, response));
    }

    #[test]
    fn shared_common_words_are_not_evidence() {
        let body = "because should também quando sempre depois antes nunca porque before after always never através";
        assert!(!response_uses("item-2", body, body));
    }

    #[test]
    fn bodies_with_few_terms_never_match() {
        let body = "deploy rollback";
        assert!(!response_uses(
            "item-3",
            body,
            "deploy rollback deploy rollback"
        ));
    }

    #[test]
    fn partial_words_do_not_match() {
        let body = "handler serializer validators";
        assert!(!response_uses(
            "item-4",
            body,
            "handlers serializers validators"
        ));
        assert!(response_uses(
            "item-4",
            body,
            "handler serializer validators"
        ));
    }

    #[test]
    fn the_utf8_cut_does_not_panic() {
        // 90 kB of three-byte characters: the 64 KiB cut lands inside a character.
        let body = "compiling checksum 84276fb shipping";
        let response = "€".repeat(30_000);
        assert!(!response_uses("item-5", body, &response));
    }

    #[test]
    fn id_counts_only_as_a_whole_word() {
        assert!(!response_uses("n01", "", "see version01 for details"));
        assert!(!response_uses("n0", "", "the n0-b item"));
        assert!(response_uses("n0", "", "item n0, applied"));
    }

    #[test]
    fn long_bodies_need_eight_terms_below_the_fraction() {
        let body: String = (0..60).map(|i| format!("termx{i:02}a ")).collect();
        let response: String = (0..8).map(|i| format!("termx{i:02}a ")).collect();
        assert!(response_uses("x", &body, &response));
        let few: String = (0..7).map(|i| format!("termx{i:02}a ")).collect();
        assert!(!response_uses("x", &body, &few));
    }
}
