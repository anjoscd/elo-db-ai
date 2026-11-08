use std::path::Path;

use elo_ctx::bench::load_fixture;
use elo_ctx::bench::run::{measure, Split};

fn summary(split: Split, name: &str) -> elo_ctx::bench::metrics::Summary {
    let fx = load_fixture(Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/fixture/v0"))
        .expect("fixture");
    let r = measure(&fx, split);
    r.selectors
        .into_iter()
        .find(|s| s.selector == name)
        .expect("selector")
        .summary
}

fn mean(s: &elo_ctx::bench::metrics::Summary, name: &str) -> f64 {
    s.rates[name].mean.expect("defined")
}

#[test]
fn e1_never_crosses_scope_or_emits_forbidden_items() {
    let e1 = summary(Split::Test, "E1");
    assert_eq!(mean(&e1, "forbidden_rate"), 0.0);
    assert_eq!(mean(&e1, "scope_violation_rate"), 0.0);
    assert_eq!(e1.supersession_leaks_total, 0);
}

#[test]
fn e1_delivers_every_law_and_beats_b1_on_precision_and_bytes() {
    let e1 = summary(Split::Test, "E1");
    let b1 = summary(Split::Test, "B1");
    assert_eq!(mean(&e1, "law_full_coverage"), 1.0);
    assert!(mean(&e1, "required_session_recall") >= mean(&b1, "required_session_recall"));
    let gate = (mean(&b1, "required_reachable_recall") + 0.20).min(0.95);
    assert!(mean(&e1, "required_reachable_recall") >= gate);
    assert!(mean(&e1, "relevance_precision") >= mean(&b1, "relevance_precision") + 0.20);
    assert!(e1.bytes_p95 <= b1.bytes_p95);
}
