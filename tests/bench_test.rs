use std::collections::BTreeMap;
use std::path::Path;

use elo_ctx::bench::baselines::keyword_matches;
use elo_ctx::bench::load_fixture;
use elo_ctx::bench::metrics::{summarize, EventMetrics};
use elo_ctx::bench::run::{measure, Report, Split};

fn fixture_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/fixture/v0")
}

fn report(split: Split) -> Report {
    let fx = load_fixture(fixture_dir()).expect("fixture v0 loads");
    measure(&fx, split)
}

fn selector<'a>(report: &'a Report, name: &str) -> &'a elo_ctx::bench::run::SelectorReport {
    report
        .selectors
        .iter()
        .find(|s| s.selector == name)
        .expect("selector present")
}

#[test]
fn b1_matches_whole_words_only() {
    assert!(!keyword_matches("Explain the database layer", "data"));
    assert!(keyword_matches("check the data, then ship", "data"));
    assert!(!keyword_matches("metadata_store", "data"));
}

#[test]
fn b1_ignores_case_and_accents() {
    assert!(keyword_matches("Revisar a AÇÃO de estorno", "acao"));
    assert!(keyword_matches("revisar a acao", "Ação"));
    assert!(keyword_matches("PAYMENT failed", "payment"));
}

#[test]
fn b0_injects_nothing() {
    let r = report(Split::All);
    let b0 = selector(&r, "B0");
    assert!(b0.events.iter().all(|e| e.used_bytes == 0));
    let forbidden = &b0.summary.rates["forbidden_rate"];
    assert_eq!(forbidden.mean, Some(0.0));
}

#[test]
fn budget_is_never_exceeded() {
    let fx = load_fixture(fixture_dir()).expect("fixture v0 loads");
    let budget = usize::try_from(fx.config.workspace.budget_bytes).expect("budget fits usize");
    let r = measure(&fx, Split::All);
    for s in &r.selectors {
        for e in &s.events {
            assert!(
                e.used_bytes <= budget,
                "{} used {} > {budget} on {}",
                s.selector,
                e.used_bytes,
                e.event_id
            );
        }
    }
}

#[test]
fn empty_denominators_are_excluded_from_the_mean() {
    let mut with_value = BTreeMap::new();
    with_value.insert("relevance_precision".to_string(), Some(1.0));
    let mut without_value = BTreeMap::new();
    without_value.insert("relevance_precision".to_string(), None);
    let events = vec![
        EventMetrics {
            event_id: "a".into(),
            rates: with_value,
            supersession_leaks: 0,
            used_bytes: 10,
        },
        EventMetrics {
            event_id: "b".into(),
            rates: without_value,
            supersession_leaks: 0,
            used_bytes: 30,
        },
    ];
    let summary = summarize(&events);
    let precision = &summary.rates["relevance_precision"];
    assert_eq!(precision.mean, Some(1.0));
    assert_eq!(precision.included_events, 1);
    assert_eq!(precision.excluded_events, 1);
}

#[test]
fn b0_precision_has_no_denominator() {
    let r = report(Split::Test);
    let precision = &selector(&r, "B0").summary.rates["relevance_precision"];
    assert_eq!(precision.mean, None);
    assert_eq!(precision.included_events, 0);
}

#[test]
fn measurement_is_deterministic() {
    let a = report(Split::Test);
    let b = report(Split::Test);
    assert_eq!(a.table(), b.table());
    assert_eq!(
        serde_json::to_string(&a.selectors).expect("serializes"),
        serde_json::to_string(&b.selectors).expect("serializes")
    );
}

#[test]
fn benchmark_mode_outputs_never_contain_more_line() {
    use elo_ctx::bench::elo_selector::E1;
    use elo_ctx::bench::selector::{render, Selector, SessionState};
    let fx = load_fixture(fixture_dir()).expect("fixture v0 loads");
    let mut sessions: std::collections::BTreeMap<&str, SessionState> =
        std::collections::BTreeMap::new();
    for ev in &fx.events {
        if ev.allowed_scopes.is_empty() {
            continue;
        }
        let state = sessions.entry(ev.session_id.as_str()).or_default();
        state.begin_event(ev);
        let sel = E1.select(&fx, ev, state);
        let out = render(&fx, ev, &sel);
        assert!(
            !out.contains("more → elo context"),
            "benchmark output contains '+N more → elo context' for event {}",
            ev.id
        );
        state.record(ev, &sel);
    }
}
