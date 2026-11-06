//! `elo context` (SPEC §3.3, §4.5, §9): the full text of the active items of a scope or
//! topic. It is an explicit request, so it may cross scopes, and it only reads the store.
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use chrono::Utc;
use serde_json::json;

use crate::bench::fixture::{Item, Kind, Status};
use crate::config::Config;
use crate::hook::deliverable;
use crate::live::{load_item, load_items};
use crate::recall::{recall, Query, RecallError};
use crate::store::{now_seconds, Store, StoreError, DATA_DIR, DB_FILE};

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Recall(#[from] RecallError),
}

#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub target: &'a str,
    pub words: &'a [String],
    pub ids: Option<&'a [String]>,
    pub budget: u64,
}

#[derive(Debug, Clone)]
pub struct Shown {
    pub id: String,
    pub kind: &'static str,
    pub scope: String,
    pub body: String,
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// Canonical label used in the overflow line (`project:api`, `global`, or the topic).
    pub label: String,
    pub shown: Vec<Shown>,
    /// `(id, reason)` for `--ids` entries that cannot be delivered.
    pub missing: Vec<(String, String)>,
    pub omitted: Vec<String>,
    /// Final "+N itens: ..." line, already fitted to the budget (empty when nothing is omitted).
    pub tail: String,
    /// Empty-result message, already fitted to the text budget.
    pub empty_message: String,
    pub used_bytes: usize,
}

enum Target {
    Scope(String),
    Topic(String),
}

/// Opens `<root>/.elo/elo.db` read-only.
pub fn open(root: &Path) -> Result<Store, ContextError> {
    if !root.join(DATA_DIR).join(DB_FILE).is_file() {
        return Err(ContextError::Usage(
            "no ELO store here (.elo/elo.db); run `elo init`".into(),
        ));
    }
    Ok(Store::open_read_only(root)?)
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Law => "law",
        Kind::Rule => "rule",
        Kind::Decision => "decision",
        Kind::Note => "note",
        Kind::Pitfall => "pitfall",
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn resolve(raw: &str, config: &Config, items: &[Item]) -> Result<Target, ContextError> {
    let invalid = || {
        ContextError::Usage(format!(
            "invalid target {raw:?}: use global, project:NAME, link:NAME, a project or link name, or a topic"
        ))
    };
    let target = raw.trim();
    if target == "global" {
        return Ok(Target::Scope("global".into()));
    }
    for prefix in ["project:", "link:"] {
        if let Some(name) = target.strip_prefix(prefix) {
            return if valid_name(name) {
                Ok(Target::Scope(target.into()))
            } else {
                Err(invalid())
            };
        }
    }
    if target.contains(':') || target.starts_with('-') || !valid_name(target) {
        return Err(invalid());
    }
    if !target.chars().any(char::is_alphanumeric) {
        return Err(invalid());
    }
    let scopes: BTreeSet<&str> = items.iter().map(|item| item.scope.as_str()).collect();
    let project = config
        .projects
        .iter()
        .find(|p| p.name == target || p.aliases.iter().any(|a| a == target))
        .map(|p| format!("project:{}", p.name))
        .or_else(|| Some(format!("project:{target}")).filter(|s| scopes.contains(s.as_str())));
    let link = config
        .links
        .iter()
        .find(|l| l.name == target)
        .map(|l| format!("link:{}", l.name))
        .or_else(|| Some(format!("link:{target}")).filter(|s| scopes.contains(s.as_str())));
    Ok(match project.or(link) {
        Some(scope) => Target::Scope(scope),
        None => Target::Topic(target.into()),
    })
}

/// Relevance rank (0 = best) of the items matching `text`, from the recall ranking.
fn ranks(
    store: &Store,
    text: &str,
    scope: Option<&str>,
) -> Result<HashMap<String, usize>, ContextError> {
    let query = Query {
        text: text.into(),
        history: false,
        scope: scope.map(str::to_owned),
        kind: None,
        limit: usize::MAX,
    };
    match recall(store, &query, now_seconds()) {
        Ok(hits) => Ok(hits
            .into_iter()
            .enumerate()
            .map(|(rank, hit)| (hit.id, rank))
            .collect()),
        // A query the full-text engine cannot parse ranks nothing instead of failing.
        Err(_) => Ok(HashMap::new()),
    }
}

fn inactive_reason(item: &Item) -> String {
    if let Some(next) = &item.superseded_by {
        return format!("superseded by {next}");
    }
    match item.status {
        Status::Superseded => "superseded".into(),
        Status::Archived => "archived".into(),
        Status::Expired | Status::Active => "expired".into(),
    }
}

/// Neutralizes what could forge the hook wrapper or another item's line.
fn escape(text: &str) -> String {
    text.replace("<elo", "‹elo")
        .replace("</elo", "‹/elo")
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' {
                '�'
            } else {
                c
            }
        })
        .collect::<String>()
        .replace('\n', "\n  ")
}

/// `escape` for text that must stay on a single line (ids, scopes, labels, reasons).
fn one_line(text: &str) -> String {
    escape(text).replace('\n', " ")
}

/// Longest prefix of `text` with at most `max` bytes, cut on a character boundary.
fn cut_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The "+N items" line listing as many omitted ids as fit in `room` bytes ("" if none fits).
fn fit_tail(omitted: &[String], label: &str, room: usize) -> String {
    if omitted.is_empty() {
        return String::new();
    }
    let head = format!("+{} items: elo context {label} --ids ", omitted.len());
    let mut ids = String::new();
    let mut listed = 0;
    for (k, id) in omitted.iter().enumerate() {
        let id = one_line(id);
        let sep = if k == 0 { "" } else { "," };
        let last = k + 1 == omitted.len();
        let end = if last { "\n" } else { ",…\n" };
        if head.len() + ids.len() + sep.len() + id.len() + end.len() > room {
            break;
        }
        ids.push_str(sep);
        ids.push_str(&id);
        listed += 1;
    }
    if listed == 0 {
        return String::new();
    }
    if listed == omitted.len() {
        format!("{head}{ids}\n")
    } else {
        format!("{head}{ids},…\n")
    }
}

fn line(item: &Shown) -> String {
    format!(
        "[{} · {}] {}: {}\n",
        item.kind,
        one_line(&item.scope),
        one_line(&item.id),
        escape(&item.body)
    )
}

/// Collects what `elo context` shows, cutting at `request.budget` bytes.
pub fn collect(
    store: &Store,
    config: &Config,
    request: &Request<'_>,
) -> Result<Outcome, ContextError> {
    if request.budget == 0 {
        return Err(ContextError::Usage(
            "--budget must be greater than zero".into(),
        ));
    }
    let now = Utc::now();
    let words = request.words.join(" ");
    let mut outcome = Outcome::default();
    let mut candidates: Vec<Item> = Vec::new();
    let mut rank: HashMap<String, usize> = HashMap::new();

    let all = load_items(store)?;
    let target = resolve(request.target, config, &all)?;
    match &target {
        Target::Scope(scope) => outcome.label = scope.clone(),
        Target::Topic(topic) => outcome.label = topic.clone(),
    }

    if let Some(ids) = request.ids {
        let mut seen = BTreeSet::new();
        for id in ids.iter().filter(|id| seen.insert(id.as_str())) {
            match load_item(store, id) {
                Ok(item) if deliverable(&item, now) => candidates.push(item),
                Ok(item) => outcome.missing.push((
                    id.clone(),
                    format!(
                        "inactive: {} ({})",
                        one_line(id),
                        one_line(&inactive_reason(&item))
                    ),
                )),
                Err(StoreError::UnknownItem(_)) => {
                    outcome
                        .missing
                        .push((id.clone(), format!("not found: {}", one_line(id))));
                }
                Err(other) => return Err(other.into()),
            }
        }
        if !words.is_empty() {
            rank = ranks(store, &words, None)?;
        }
    } else {
        match &target {
            Target::Scope(scope) => {
                candidates = all
                    .into_iter()
                    .filter(|item| &item.scope == scope && deliverable(item, now))
                    .collect();
                if !words.is_empty() {
                    rank = ranks(store, &words, Some(scope))?;
                }
            }
            Target::Topic(topic) => {
                rank = ranks(store, &format!("{topic} {words}"), None)?;
                candidates = all
                    .into_iter()
                    .filter(|item| rank.contains_key(&item.id) && deliverable(item, now))
                    .collect();
            }
        }
    }

    candidates.sort_by(|a, b| {
        let key = |item: &Item| {
            (
                item.kind != Kind::Law,
                rank.get(&item.id).copied().unwrap_or(usize::MAX),
            )
        };
        key(a)
            .cmp(&key(b))
            .then_with(|| b.importance.total_cmp(&a.importance))
            .then_with(|| a.id.cmp(&b.id))
    });
    let entries: Vec<Shown> = candidates
        .into_iter()
        .map(|item| Shown {
            id: item.id,
            kind: kind_name(item.kind),
            scope: item.scope,
            body: item.body,
        })
        .collect();

    let budget = usize::try_from(request.budget).unwrap_or(usize::MAX);
    let label = one_line(&outcome.label);
    // Room always reserved for an overflow line that is still a working command: the
    // omitted count plus at least the first omitted id (`first` = index of that entry).
    let ids: Vec<String> = entries.iter().map(|e| one_line(&e.id)).collect();
    let reserve = |first: usize| -> usize {
        let rest = ids.len().saturating_sub(first);
        match rest {
            0 => 0,
            1 => format!("+1 items: elo context {label} --ids {}\n", ids[first]).len(),
            _ => format!(
                "+{rest} items: elo context {label} --ids {},…\n",
                ids[first]
            )
            .len(),
        }
    };

    // Warnings first, as many as fit next to the overflow reserve.
    let mut used = 0;
    let mut kept_missing = Vec::new();
    for (id, text) in std::mem::take(&mut outcome.missing) {
        let length = text.len() + 1;
        if used + length + reserve(0) > budget {
            break;
        }
        used += length;
        kept_missing.push((id, text));
    }
    outcome.missing = kept_missing;

    let mut shown = Vec::new();
    let mut entries = entries.into_iter().peekable();
    while let Some(entry) = entries.peek() {
        let length = line(entry).len();
        if used + length + reserve(shown.len() + 1) > budget {
            break;
        }
        used += length;
        shown.push(entries.next().expect("peeked"));
    }
    // Not even the first item fits: show the start of its body, marked as cut.
    if shown.is_empty() {
        if let Some(entry) = entries.peek() {
            let marker = "…[cortado]";
            let mut cut = entry.clone();
            let mut body = cut_bytes(&entry.body, budget.saturating_sub(used)).to_string();
            loop {
                cut.body = format!("{body}{marker}");
                let length = line(&cut).len();
                if used + length + reserve(1) <= budget {
                    used += length;
                    entries.next().expect("peeked");
                    shown.push(cut);
                    break;
                }
                if body.is_empty() {
                    break;
                }
                body.truncate(cut_bytes(&body, body.len().saturating_sub(16)).len());
            }
        }
    }
    outcome.omitted = entries.map(|e| e.id).collect();
    let minimum = reserve(shown.len());
    if used + minimum > budget {
        return Err(ContextError::Usage(format!(
            "--budget too small: at least {} bytes are needed to include the command with the first omitted id",
            used + minimum
        )));
    }
    outcome.tail = fit_tail(&outcome.omitted, &label, budget.saturating_sub(used));
    used += outcome.tail.len();
    outcome.shown = shown;
    if outcome.shown.is_empty() && outcome.missing.is_empty() && outcome.omitted.is_empty() {
        let message = format!("No active item in {}.\n", one_line(request.target));
        outcome.empty_message = cut_bytes(&message, budget.saturating_sub(used)).to_string();
        used += outcome.empty_message.len();
    }
    outcome.used_bytes = used;
    Ok(outcome)
}

pub fn render_text(outcome: &Outcome, _target: &str) -> String {
    let mut out = String::new();
    for (_, text) in &outcome.missing {
        out.push_str(text);
        out.push('\n');
    }
    for item in &outcome.shown {
        out.push_str(&line(item));
    }
    out.push_str(&outcome.tail);
    out.push_str(&outcome.empty_message);
    out
}

pub fn render_json(outcome: &Outcome) -> String {
    json!({
        "target": outcome.label,
        "items": outcome.shown.iter().map(|i| json!({
            "id": i.id, "kind": i.kind, "scope": i.scope, "body": i.body,
        })).collect::<Vec<_>>(),
        "missing": outcome.missing.iter().map(|(id, text)| json!({"id": id, "message": text})).collect::<Vec<_>>(),
        "omitted": outcome.omitted,
        "used_bytes": outcome.used_bytes,
    })
    .to_string()
}
