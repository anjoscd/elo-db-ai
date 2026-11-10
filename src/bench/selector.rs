//! Common contract and rendering measured in UTF-8 bytes.
use std::collections::{BTreeMap, BTreeSet};

use super::fixture::{Event, Fixture, Kind};

pub trait Selector {
    fn name(&self) -> &str;
    fn select(&self, fx: &Fixture, ev: &Event, state: &mut SessionState) -> Selection;
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionState {
    pub served: BTreeSet<String>,
    #[serde(default)]
    pub recent_prompts: Vec<String>,
    /// Files touched in this session, with the event_seq of the last touch.
    pub touched: Vec<(String, u64)>,
}

impl SessionState {
    pub fn begin_event(&mut self, ev: &Event) {
        if matches!(ev.event_type.as_str(), "compaction" | "session-start") {
            self.served.clear();
            self.touched.clear();
            // The topic survives a compaction; only a new session forgets it.
            if ev.event_type == "session-start" {
                self.recent_prompts.clear();
            }
        }
        for path in ev.tool_paths.iter().flatten() {
            self.touched.retain(|(p, _)| p != path);
            self.touched.push((path.clone(), ev.event_seq));
        }
    }

    pub fn record(&mut self, selection: &Selection) {
        self.served.extend(selection.full_ids.iter().cloned());
    }
}

#[derive(Debug, Default, Clone)]
pub struct Selection {
    pub full_ids: Vec<String>,
    pub pointer_ids: Vec<String>,
    pub used_bytes: usize,
    pub omitted_notes: Vec<(usize, String)>,
}

impl Selection {
    pub fn new(fx: &Fixture, ev: &Event, full_ids: Vec<String>, pointer_ids: Vec<String>) -> Self {
        let mut selection = Self {
            full_ids,
            pointer_ids,
            used_bytes: 0,
            omitted_notes: vec![],
        };
        selection.used_bytes = render(fx, ev, &selection).len();
        selection
    }
}

// SplitMix64: fixed seed and stable position in the canonical event sequence.
// Benchmark only; the real hook must use unpredictable randomness.
fn nonce(fx: &Fixture, ev: &Event) -> u64 {
    let key = |e: &Event| (e.session_id.clone(), e.event_seq, e.id.clone());
    let index = fx.events.iter().filter(|e| key(e) < key(ev)).count() as u64;
    let mut z = 0x454c4f5f42454e43_u64
        .wrapping_add(0x9e3779b97f4a7c15_u64.wrapping_mul(index.wrapping_add(1)));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

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

fn inline(text: &str) -> String {
    escape(text).replace('\n', " ").replace('"', "&quot;")
}

pub fn render(fx: &Fixture, ev: &Event, selection: &Selection) -> String {
    if selection.full_ids.is_empty()
        && selection.pointer_ids.is_empty()
        && selection.omitted_notes.is_empty()
    {
        return String::new();
    }
    let content = render_content(fx, selection);
    let nonce = nonce(fx, ev);
    let mut used = 0;
    loop {
        let output = format!("<elo-{nonce:016x} event=\"{}\" budget=\"{}\" used=\"{used}\">\n{content}</elo-{nonce:016x}>\n",
            inline(&ev.id), fx.config.workspace.budget_bytes);
        if output.len() == used {
            return output;
        }
        used = output.len();
    }
}

/// Rendered payload bytes, including grouped pointers and omission notices.
pub(crate) fn render_content(fx: &Fixture, selection: &Selection) -> String {
    let mut content = String::new();
    for id in &selection.full_ids {
        if let Some(item) = fx.items.iter().find(|item| &item.id == id) {
            let reason = if item.kind == Kind::Law {
                "law · "
            } else {
                ""
            };
            content.push_str(&format!(
                "[{reason}{} · via keyword] {}: {}\n",
                inline(&item.scope),
                inline(id),
                escape(&item.body)
            ));
        }
    }
    let mut groups: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for id in &selection.pointer_ids {
        if let Some(item) = fx.items.iter().find(|item| &item.id == id) {
            groups.entry(&item.scope).or_default().push(format!(
                "{}: {}",
                inline(id),
                inline(&item.title.chars().take(80).collect::<String>())
            ));
        }
    }
    for (scope, ids) in groups {
        let scope = inline(scope);
        content.push_str(&format!(
            "+{} more on {scope} ({}) → elo context {scope}\n",
            ids.len(),
            ids.join(", ")
        ));
    }
    for (count, keyword) in &selection.omitted_notes {
        content.push_str(&format!(
            "+{count} notes (elo recall {})\n",
            inline(keyword)
        ));
    }
    content
}
