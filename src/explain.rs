//! `elo explain` (SPEC §9): why an item exists and what the hook did last. Read-only.
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use chrono::Utc;
use serde_json::Value;

use crate::bench::fixture::{Item, Kind, Status};
use crate::config::Config;
use crate::hook::deliverable;
use crate::stats::demoted;
use crate::store::DATA_DIR;

const PROMPT_CHARS: usize = 200;

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Law => "law",
        Kind::Rule => "rule",
        Kind::Decision => "decision",
        Kind::Note => "note",
        Kind::Pitfall => "pitfall",
    }
}

fn effective_status(item: &Item) -> String {
    if let Some(next) = &item.superseded_by {
        return format!("superseded by {next}");
    }
    match item.status {
        Status::Superseded => "superseded".into(),
        Status::Archived => "archived".into(),
        Status::Expired => "expired".into(),
        Status::Active if deliverable(item, Utc::now()) => "active".into(),
        Status::Active => "expired".into(),
    }
}

fn list(values: &[String]) -> String {
    if values.is_empty() {
        "-".into()
    } else {
        values.join(", ")
    }
}

/// Project names the workspace knows: `elo.toml`, or what discovery finds without it.
fn known_projects(root: &Path, config: &Config) -> Vec<String> {
    if root.join("elo.toml").is_file() {
        config.projects.iter().map(|p| p.name.clone()).collect()
    } else {
        crate::discover::discover(root)
            .projects
            .into_iter()
            .map(|p| p.name)
            .collect()
    }
}

fn eligibility(item: &Item, projects: &[String], config: &Config) -> String {
    if !deliverable(item, Utc::now()) {
        return "none (inactive item)".into();
    }
    if item.scope == "global" {
        return if projects.is_empty() {
            "all (no project configured; applies in any directory)".into()
        } else {
            format!("all ({})", projects.join(", "))
        };
    }
    if let Some(name) = item.scope.strip_prefix("project:") {
        return if projects.iter().any(|p| p == name) {
            name.to_string()
        } else {
            format!("{name} (project not found in the configuration or the workspace)")
        };
    }
    if let Some(name) = item.scope.strip_prefix("link:") {
        return match config.links.iter().find(|l| l.name == name) {
            Some(link) => format!("{} (when link {name} is active)", list(&link.projects)),
            None => format!("none (link {name} not configured)"),
        };
    }
    "none (unknown scope)".into()
}

/// Everything about one item: kind, scope, status, triggers, eligibility and scoreboard.
pub fn item_report(root: &Path, config: &Config, item: &Item) -> String {
    let projects = known_projects(root, config);
    let mut out = String::new();
    let _ = writeln!(out, "id: {}", item.id);
    let _ = writeln!(out, "kind: {}", kind_name(item.kind));
    let _ = writeln!(out, "scope: {}", item.scope);
    let _ = writeln!(out, "status: {}", effective_status(item));
    let _ = writeln!(
        out,
        "triggers: keywords=[{}] paths=[{}] always={}",
        item.triggers.keywords.join(", "),
        item.triggers.paths.join(", "),
        if item.triggers.always { "yes" } else { "no" }
    );
    let _ = writeln!(out, "eligible in: {}", eligibility(item, &projects, config));
    let _ = writeln!(
        out,
        "demoted: {} (good={}, bad={}, limit={})",
        if demoted(item, config.workspace.demote_bad_min) {
            "yes; always disabled, needs a keyword or path"
        } else {
            "no"
        },
        item.feedback.good,
        item.feedback.bad,
        config.workspace.demote_bad_min
    );
    if let Ok(store) = crate::store::Store::open_read_only(root) {
        if let Ok(mut statement) = store
            .conn
            .prepare("SELECT at FROM auto_usage WHERE item_id=?1 ORDER BY at")
        {
            if let Ok(rows) = statement.query_map([&item.id], |r| r.get::<_, i64>(0)) {
                let times: Vec<String> = rows
                    .filter_map(Result::ok)
                    .filter_map(|at| chrono::DateTime::from_timestamp(at, 0))
                    .map(|at| at.to_rfc3339())
                    .collect();
                let _ = writeln!(out, "used (auto): {}; when: {}", times.len(), list(&times));
            }
        }
    }
    out
}

fn one_line(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > PROMPT_CHARS {
        let cut: String = flat.chars().take(PROMPT_CHARS).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

fn ids_of(record: &Value, key: &str) -> Vec<String> {
    record[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Reason tag of `id` in the rendered payload: `[project:api · via cwd] R4: ...`.
fn reason_of(rendered: &str, id: &str) -> Option<String> {
    let marker = format!("] {id}: ");
    rendered.lines().find_map(|line| {
        // The tag is the leading `[...]`: the first `]` of the line must close it right
        // before ` <id>: `, so a body quoting another item's tag cannot be mistaken for it.
        let rest = line.strip_prefix('[')?;
        let close = rest.find(']')?;
        rest[close..]
            .starts_with(&marker)
            .then(|| rest[..close].to_string())
    })
}

/// The log grows with every prompt; only its end is read.
const TAIL_BYTES: u64 = 256 * 1024;

/// The last `max` bytes of `path` as text, starting at a line boundary. Invalid UTF-8 is
/// replaced, so a line truncated by a crash only spoils itself.
fn tail_text(path: &Path, max: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path)?;
    let size = file.metadata()?.len();
    let start = size.saturating_sub(max);
    let partial = if start > 0 {
        file.seek(SeekFrom::Start(start - 1))?;
        let mut previous = [0];
        file.read_exact(&mut previous)?;
        previous[0] != b'\n'
    } else {
        false
    };
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(usize::try_from(size - start).unwrap_or(0));
    file.take(size - start).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(if partial {
        // Discard only a line that starts outside the window.
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default()
    } else {
        text
    })
}

/// Report for the last hook event in `.elo/shadow/log.jsonl`, optionally of one session.
pub fn last_report(root: &Path, session: Option<&str>) -> String {
    let path = root.join(DATA_DIR).join("shadow").join("log.jsonl");
    let text = match tail_text(&path, TAIL_BYTES) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return "No hook record (.elo/shadow/log.jsonl does not exist).\n".into();
        }
        Err(err) => {
            return format!("Could not read .elo/shadow/log.jsonl: {err}\n");
        }
    };
    let record = text
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|r| session.is_none_or(|s| r["session_id"].as_str() == Some(s)));
    let Some(record) = record else {
        return match session {
            Some(s) => format!("No hook record for session {s}.\n"),
            None => "No hook record (.elo/shadow/log.jsonl is empty).\n".into(),
        };
    };
    let rendered = record["rendered"].as_str().unwrap_or_default();
    let full = ids_of(&record, "full_ids");
    let pointers = ids_of(&record, "pointer_ids");
    let mut out = String::new();
    let _ = writeln!(
        out,
        "event: session {}, seq {}, at {}",
        record["session_id"].as_str().unwrap_or("?"),
        record["event_seq"],
        record["at"].as_str().unwrap_or("?")
    );
    let _ = writeln!(
        out,
        "request: {}",
        one_line(record["prompt"].as_str().unwrap_or("(no prompt)"))
    );
    if let Some(cwd) = record["cwd"].as_str() {
        let _ = writeln!(out, "cwd: {cwd}");
    }
    let _ = writeln!(out, "bytes used: {}", record["used_bytes"]);
    if full.is_empty() && pointers.is_empty() {
        let _ = writeln!(out, "nothing injected (no relevant item)");
    }
    let _ = writeln!(out, "delivered in full ({}):", full.len());
    for id in &full {
        match reason_of(rendered, id) {
            Some(reason) => {
                let _ = writeln!(out, "  {id}  [{reason}]");
            }
            None => {
                let _ = writeln!(out, "  {id}");
            }
        }
    }
    let _ = writeln!(out, "as index ({}): {}", pointers.len(), list(&pointers));
    for key in ["reason", "reasons", "motivo"] {
        if !record[key].is_null() {
            let _ = writeln!(out, "reason: {}", record[key]);
        }
    }
    out
}
