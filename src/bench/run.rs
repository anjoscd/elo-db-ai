//! Reproducible run: fixture clock, empty state and canonical order.
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use clap::ValueEnum;
use serde::Serialize;

use super::baselines::{B0, B1};
use super::elo_selector::E1;
use super::fixture::{load_fixture, Fixture, FixtureError};
use super::metrics::{evaluate, summarize, EventMetrics, Summary};
use super::selector::{Selector, SessionState};

#[derive(Debug, Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    Dev,
    Test,
    All,
}

#[derive(Debug, Serialize)]
pub struct SelectorReport {
    pub selector: String,
    pub summary: Summary,
    pub events: Vec<EventMetrics>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub commit: Option<String>,
    /// Logical date: the largest selected `now`, never the machine clock.
    pub date: Option<String>,
    pub date_source: &'static str,
    pub split: Split,
    pub nonce_seed: &'static str,
    pub selectors: Vec<SelectorReport>,
}

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error(transparent)]
    Fixture(#[from] FixtureError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub fn measure(fx: &Fixture, split: Split) -> Report {
    let mut events: Vec<_> = fx
        .events
        .iter()
        .filter(|ev| match split {
            Split::Dev => fx.split.dev.contains(&ev.id),
            Split::Test => fx.split.test.contains(&ev.id),
            Split::All => true,
        })
        .collect();
    events.sort_by_key(|ev| (&ev.session_id, ev.event_seq, &ev.id));
    let mut selectors = Vec::new();
    for selector in [
        &B0 as &dyn Selector,
        &B1 as &dyn Selector,
        &E1 as &dyn Selector,
    ] {
        let mut sessions: BTreeMap<&str, SessionState> = BTreeMap::new();
        let mut metrics = Vec::new();
        for ev in &events {
            let state = sessions.entry(&ev.session_id).or_default();
            state.begin_event(ev);
            let selection = selector.select(fx, ev, state);
            metrics.push(evaluate(fx, ev, &selection, state));
            state.record(&selection);
        }
        selectors.push(SelectorReport {
            selector: selector.name().into(),
            summary: summarize(&metrics),
            events: metrics,
        });
    }
    let commit = Command::new("git")
        .args(["-C", env!("CARGO_MANIFEST_DIR"), "rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned());
    let date = events
        .iter()
        .filter_map(|ev| chrono::DateTime::parse_from_rfc3339(&ev.now).ok())
        .max()
        .map(|date| date.to_rfc3339());
    Report {
        commit,
        date,
        date_source: "max(event.now)",
        split,
        nonce_seed: "454c4f5f42454e43",
        selectors,
    }
}

impl Report {
    pub fn table(&self) -> String {
        let mut output = String::from("| Selector | Forbidden | Scope | Precision | Full recall | Reachable recall | Session recall | Laws | Leaks/event | Bytes p50 | Bytes p95 | Abstention |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
        let value = |v: Option<f64>| v.map_or_else(|| "N/A".into(), |n| format!("{n:.6}"));
        for report in &self.selectors {
            let s = &report.summary;
            let rate = |name: &str| value(s.rates.get(name).and_then(|m| m.mean));
            output.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                report.selector,
                rate("forbidden_rate"),
                rate("scope_violation_rate"),
                rate("relevance_precision"),
                rate("required_full_recall"),
                rate("required_reachable_recall"),
                rate("required_session_recall"),
                rate("law_full_coverage"),
                value(s.supersession_leaks.mean),
                s.bytes_p50.map_or_else(|| "N/A".into(), |n| n.to_string()),
                s.bytes_p95.map_or_else(|| "N/A".into(), |n| n.to_string()),
                rate("abstention_accuracy")
            ));
        }
        output
    }
}

pub fn run(fixture: &Path, split: Split, json: Option<&Path>) -> Result<String, BenchError> {
    let report = measure(&load_fixture(fixture)?, split);
    if let Some(path) = json {
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&report)?),
        )?;
    }
    Ok(report.table())
}
