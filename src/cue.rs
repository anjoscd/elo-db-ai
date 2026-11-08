//! Delivery anchored to the file a tool is about to touch (pre-tool-use).
//!
//! Agents rarely look memory up by themselves; delivering the rule at the moment they
//! touch the file it cites works better (arXiv 2607.20972). This module only chooses and
//! renders; the hook (`hook::run`) owns the session lock, the event counter and the
//! injection records, exactly like for user-prompt-submit.
use std::collections::BTreeSet;

use crate::bench::fixture::{Item, Kind};

/// At most this many items per pre-tool-use event.
pub const MAX_ITEMS: usize = 3;
/// At most this many bytes for the whole envelope.
pub const MAX_BYTES: usize = 1500;
/// An item delivered in full in the last N events of the session is not repeated.
pub const DEDUP_WINDOW: u64 = 10;

/// The chosen items and the rendered envelope.
#[derive(Debug, Default, PartialEq)]
pub struct Cue {
    pub ids: Vec<String>,
    pub rendered: String,
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

fn kind_rank(kind: Kind) -> u8 {
    match kind {
        Kind::Law => 0,
        Kind::Pitfall => 1,
        Kind::Rule => 2,
        Kind::Decision => 3,
        Kind::Note => 4,
    }
}

/// A keyword that names a file (`guard.rs`, `src/hook.rs`).
fn file_like(word: &str) -> bool {
    !word.contains(char::is_whitespace)
        && (word.contains('/')
            || word.rsplit_once('.').is_some_and(|(_, ext)| {
                // `stem` may be empty: dotfiles like `.env` or `.gitignore` are files too.
                !ext.is_empty() && ext.chars().all(char::is_alphanumeric)
            }))
}

/// `path` ends with the whole component(s) of `suffix`.
fn ends_with_component(path: &str, suffix: &str) -> bool {
    let path = path.trim_start_matches("./").trim_start_matches('/');
    let suffix = suffix.trim_start_matches("./").trim_start_matches('/');
    !suffix.is_empty() && (path == suffix || path.ends_with(&format!("/{suffix}")))
}

/// The trigger of `item` that matches one of `paths`, if any.
fn anchor(item: &Item, paths: &[String]) -> Option<String> {
    for pattern in &item.triggers.paths {
        let hit = paths.iter().any(|p| match pattern.strip_suffix('*') {
            Some(prefix) => p.starts_with(prefix),
            None => p == pattern,
        });
        if hit {
            return Some(pattern.clone());
        }
    }
    item.triggers
        .keywords
        .iter()
        .find(|word| file_like(word) && paths.iter().any(|p| ends_with_component(p, word)))
        .cloned()
}

/// Same neutralization as the prompt hook: the text cannot close the wrapper or forge lines.
fn escape(text: &str) -> String {
    text.replace("<elo", "‹elo")
        .replace("</elo", "‹/elo")
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect::<String>()
        .replace('\n', "\n  ")
}

fn inline(text: &str) -> String {
    escape(text).replace('\n', " ").replace('"', "&quot;")
}

fn envelope(nonce: u64, event_id: &str, content: &str) -> String {
    let mut used = 0;
    loop {
        let out = format!(
            "<elo-{nonce:016x} event=\"{}\" budget=\"{MAX_BYTES}\" used=\"{used}\">\n{content}</elo-{nonce:016x}>\n",
            inline(event_id)
        );
        if out.len() == used {
            return out;
        }
        used = out.len();
    }
}

/// Chooses the items whose triggers name one of `paths`, skipping `recent` ones, and
/// renders them. `items` must already be filtered to deliverable ones.
pub fn select(
    paths: &[String],
    items: &[Item],
    recent: &BTreeSet<String>,
    event_id: &str,
    nonce: u64,
) -> Option<Cue> {
    if paths.is_empty() {
        return None;
    }
    let mut found: Vec<(&Item, String)> = items
        .iter()
        .filter(|item| !recent.contains(&item.id))
        .filter_map(|item| anchor(item, paths).map(|via| (item, via)))
        .collect();
    found.sort_by(|(a, _), (b, _)| {
        kind_rank(a.kind)
            .cmp(&kind_rank(b.kind))
            .then_with(|| b.importance.total_cmp(&a.importance))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut ids = Vec::new();
    let mut content = String::new();
    // The cap counts delivered items: one too big to fit does not take a small one's place.
    for (item, via) in found {
        if ids.len() >= MAX_ITEMS {
            break;
        }
        let line = format!(
            "[{} · {} · via file {}] {}: {}\n",
            kind_name(item.kind),
            inline(&item.scope),
            inline(&via),
            inline(&item.id),
            escape(&item.body)
        );
        let candidate = format!("{content}{line}");
        if envelope(nonce, event_id, &candidate).len() > MAX_BYTES {
            continue;
        }
        content = candidate;
        ids.push(item.id.clone());
    }
    if ids.is_empty() {
        return None;
    }
    Some(Cue {
        rendered: envelope(nonce, event_id, &content),
        ids,
    })
}

#[cfg(test)]
mod tests {
    use super::{ends_with_component, file_like};

    #[test]
    fn dotfiles_and_dot_slash_paths() {
        assert!(file_like(".env") && file_like(".gitignore") && file_like("guard.rs"));
        assert!(!file_like("travas") && !file_like("a b.rs"));
        assert!(ends_with_component("./src/guard.rs", "guard.rs"));
        assert!(ends_with_component("guard.rs", "./guard.rs"));
        assert!(!ends_with_component("src/myguard.rs", "guard.rs"));
    }
}
