//! Literal baselines of the protocol; B1 neither resolves scope nor filters status.
use super::fixture::{Event, Fixture};
use super::selector::{Selection, Selector, SessionState};
use super::unicode::normalize;

pub struct B0;
pub struct B1;

pub fn keyword_matches(prompt: &str, keyword: &str) -> bool {
    let prompt = normalize(prompt);
    let keyword = normalize(keyword);
    if keyword.is_empty() {
        return false;
    }
    let word = |c: char| c.is_alphanumeric() || c == '_';
    prompt.match_indices(&keyword).any(|(start, matched)| {
        !prompt[..start].chars().next_back().is_some_and(word)
            && !prompt[start + matched.len()..]
                .chars()
                .next()
                .is_some_and(word)
    })
}

impl Selector for B0 {
    fn name(&self) -> &str {
        "B0"
    }
    fn select(&self, fx: &Fixture, ev: &Event, _state: &mut SessionState) -> Selection {
        Selection::new(fx, ev, vec![], vec![])
    }
}

impl Selector for B1 {
    fn name(&self) -> &str {
        "B1"
    }
    fn select(&self, fx: &Fixture, ev: &Event, _state: &mut SessionState) -> Selection {
        let mut candidates: Vec<_> = fx
            .items
            .iter()
            .filter(|item| {
                ev.prompt.as_ref().is_some_and(|prompt| {
                    item.triggers
                        .keywords
                        .iter()
                        .any(|keyword| keyword_matches(prompt, keyword))
                })
            })
            .collect();
        candidates.sort_by(|a, b| {
            let timestamp = |value: &str| chrono::DateTime::parse_from_rfc3339(value).ok();
            timestamp(&b.updated_at)
                .cmp(&timestamp(&a.updated_at))
                .then(a.id.cmp(&b.id))
        });
        let mut selection = Selection::default();
        for item in candidates {
            let mut ids = selection.full_ids.clone();
            ids.push(item.id.clone());
            let next = Selection::new(fx, ev, ids, vec![]);
            if next.used_bytes as u64 > fx.config.workspace.budget_bytes {
                break;
            }
            selection = next;
        }
        selection
    }
}
