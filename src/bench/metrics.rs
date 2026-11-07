//! Per-event metrics; empty denominators stay absent from the macro average.
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::fixture::{Event, Fixture, Kind, Status};
use super::selector::{Selection, SessionState};

#[derive(Debug, Serialize)]
pub struct EventMetrics {
    pub event_id: String,
    pub rates: BTreeMap<String, Option<f64>>,
    pub supersession_leaks: usize,
    pub used_bytes: usize,
}

fn ratio(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator != 0).then(|| numerator as f64 / denominator as f64)
}

pub fn evaluate(
    fx: &Fixture,
    ev: &Event,
    selection: &Selection,
    state: &SessionState,
) -> EventMetrics {
    let full: BTreeSet<_> = selection.full_ids.iter().collect();
    let emitted: BTreeSet<_> = full.iter().copied().chain(&selection.pointer_ids).collect();
    let required: BTreeSet<_> = ev.required.iter().collect();
    // Applicable laws are labels independent of the selector, like the other required ones.
    let laws: Vec<_> = fx
        .items
        .iter()
        .filter(|i| i.kind == Kind::Law && required.contains(&i.id))
        .collect();
    let items: Vec<_> = fx
        .items
        .iter()
        .filter(|i| emitted.contains(&i.id))
        .collect();
    let forbidden = ev.forbidden.iter().any(|id| emitted.contains(id));
    let rates = BTreeMap::from([
        ("forbidden_rate".into(), Some(f64::from(forbidden))),
        (
            "scope_violation_rate".into(),
            ratio(
                items
                    .iter()
                    .filter(|i| !ev.allowed_scopes.contains(&i.scope))
                    .count(),
                emitted.len(),
            ),
        ),
        (
            "relevance_precision".into(),
            ratio(full.intersection(&required).count(), full.len()),
        ),
        (
            "required_full_recall".into(),
            ratio(full.intersection(&required).count(), required.len()),
        ),
        (
            "required_reachable_recall".into(),
            ratio(emitted.intersection(&required).count(), required.len()),
        ),
        (
            // Delivered in full now or earlier in this session and compaction window,
            // the same credit laws get (SPEC §4.3 dedup; EVALUATION §2).
            "required_session_recall".into(),
            ratio(
                required
                    .iter()
                    .filter(|id| full.contains(*id) || state.served.contains(**id))
                    .count(),
                required.len(),
            ),
        ),
        (
            "law_full_coverage".into(),
            ratio(
                laws.iter()
                    .filter(|i| full.contains(&i.id) || state.served.contains(&i.id))
                    .count(),
                laws.len(),
            ),
        ),
        (
            "abstention_accuracy".into(),
            required
                .is_empty()
                .then(|| f64::from(items.iter().all(|i| i.kind == Kind::Law))),
        ),
    ]);
    let now = chrono::DateTime::parse_from_rfc3339(&ev.now).ok();
    let supersession_leaks = items
        .iter()
        .filter(|i| {
            matches!(i.status, Status::Superseded | Status::Expired)
                || i.expires_at.as_ref().is_some_and(|value| {
                    match (chrono::DateTime::parse_from_rfc3339(value).ok(), now) {
                        (Some(expiry), Some(now)) => expiry <= now,
                        _ => false,
                    }
                })
        })
        .count();
    EventMetrics {
        event_id: ev.id.clone(),
        rates,
        supersession_leaks,
        used_bytes: selection.used_bytes,
    }
}

#[derive(Debug, Serialize)]
pub struct MacroMetric {
    pub mean: Option<f64>,
    pub included_events: usize,
    pub excluded_events: usize,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub events: usize,
    pub rates: BTreeMap<String, MacroMetric>,
    pub supersession_leaks: MacroMetric,
    pub supersession_leaks_total: usize,
    pub bytes_p50: Option<usize>,
    pub bytes_p95: Option<usize>,
}

pub fn summarize(events: &[EventMetrics]) -> Summary {
    let mut rates = BTreeMap::new();
    for name in [
        "forbidden_rate",
        "scope_violation_rate",
        "relevance_precision",
        "required_full_recall",
        "required_reachable_recall",
        "required_session_recall",
        "law_full_coverage",
        "abstention_accuracy",
    ] {
        let values: Vec<_> = events
            .iter()
            .filter_map(|e| e.rates.get(name).copied().flatten())
            .collect();
        rates.insert(
            name.into(),
            MacroMetric {
                mean: (!values.is_empty())
                    .then(|| values.iter().sum::<f64>() / values.len() as f64),
                included_events: values.len(),
                excluded_events: events.len() - values.len(),
            },
        );
    }
    let mut bytes: Vec<_> = events.iter().map(|e| e.used_bytes).collect();
    bytes.sort_unstable();
    // Nearest-rank percentiles, including zero-byte events.
    let percentile = |p: usize| {
        bytes
            .get((bytes.len() * p).div_ceil(100).saturating_sub(1))
            .copied()
    };
    let total = events.iter().map(|e| e.supersession_leaks).sum();
    Summary {
        events: events.len(),
        rates,
        supersession_leaks: MacroMetric {
            mean: ratio(total, events.len()),
            included_events: events.len(),
            excluded_events: 0,
        },
        supersession_leaks_total: total,
        bytes_p50: percentile(50),
        bytes_p95: percentile(95),
    }
}
