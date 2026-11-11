use elo_ctx::{
    bench::{
        elo_selector::E1,
        fixture::{Event, Item, Kind},
        selector::{render, Selection, Selector, SessionState},
    },
    config::Config,
    live::live_fixture,
};
use std::path::Path;
fn note(n: usize) -> Item {
    serde_json::from_value(serde_json::json!({
        "id":format!("n{n:02}"), "kind":if n.is_multiple_of(2) {"note"} else {"decision"}, "scope":"global", "title":"Título é & \" <elo ".repeat(8),
        "body":"CORPO ".repeat(50), "triggers":{"always":true,"keywords":["revisor","deploy"],"paths":["src/*"]},
        "importance":1.0, "created_at":"2026-01-01T00:00:00Z", "updated_at":format!("2026-01-{:02}T00:00:00Z", n%30+1),
        "status":"active", "origin":"agent"
    })).unwrap()
}
fn event(prompt: &str) -> Event {
    serde_json::from_value(serde_json::json!({"id":"s:1","session_id":"s","event_seq":1,"event_type":"user-prompt-submit","family":"test","cwd":".","now":"2026-02-01T00:00:00Z","prompt":prompt,"allowed_scopes":[],"required":[],"forbidden":[]})).unwrap()
}
fn select(items: Vec<Item>, config: Config, prompt: &str) -> (Selection, String) {
    let fx = live_fixture(Path::new("."), config, items);
    let ev = event(prompt);
    let s = E1.select(&fx, &ev, &mut SessionState::default());
    let output = render(&fx, &ev, &s);
    assert_eq!(s.used_bytes, output.len());
    assert!(s.used_bytes <= fx.config.workspace.budget_bytes as usize);
    (s, output)
}
#[test]
fn thirty_generic_notes_fit_with_exact_omission_count_and_newest_first() {
    let (s, out) = select((0..30).map(note).collect(), Config::default(), "revisor");
    assert!(s.full_ids.is_empty());
    assert!(!s.pointer_ids.is_empty());
    assert_eq!(s.pointer_ids[0], "n29");
    assert!(s.pointer_ids.windows(2).all(|w| w[0] > w[1]));
    assert!(!out.contains("CORPO"));
    let payload = out
        .lines()
        .skip(1)
        .take_while(|l| !l.starts_with("</elo"))
        .map(|l| l.len() + 1)
        .sum::<usize>();
    assert!(payload <= 1500, "{payload}");
    let omitted = 30 - s.pointer_ids.len();
    assert_eq!(
        s.omitted_notes.iter().map(|(n, _)| n).sum::<usize>(),
        omitted
    );
    if omitted > 0 {
        assert!(out.contains(&format!("+{omitted} notes (elo recall revisor)")));
    } else {
        assert!(s.omitted_notes.is_empty());
    }
}
#[test]
fn two_distinct_keywords_or_path_and_global_maximum() {
    let mut config = Config::default();
    config.workspace.notes_budget_bytes = 0;
    let (s, _) = select((0..5).map(note).collect(), config.clone(), "revisor deploy");
    assert_eq!(s.full_ids, ["n04", "n03"]);
    let mut duplicate = note(0);
    duplicate.triggers.keywords = vec!["revisor".into(), "REVISOR".into()];
    assert!(select(vec![duplicate], config.clone(), "revisor revisor")
        .0
        .full_ids
        .is_empty());
    let fx = live_fixture(Path::new("."), config, vec![note(0)]);
    let mut state = SessionState::default();
    state.touched.push(("src/a.rs".into(), 1));
    assert_eq!(E1.select(&fx, &event(""), &mut state).full_ids, ["n00"]);
    assert!(select(vec![note(0)], Config::default(), "revisor `deploy`")
        .0
        .full_ids
        .is_empty());
}
#[test]
fn zero_disables_each_policy_independently() {
    let c = Config::parse(
        "[workspace]\nfull_notes_max=0\nnotes_budget_bytes=0",
        Path::new("test"),
    )
    .unwrap();
    assert_eq!(
        select((0..5).map(note).collect(), c.clone(), "revisor")
            .0
            .full_ids
            .len(),
        5
    );
    let mut c = c;
    c.workspace.full_notes_max = 2;
    let (s, _) = select((0..5).map(note).collect(), c, "revisor deploy");
    assert_eq!(s.full_ids.len(), 2);
    assert_eq!(s.pointer_ids.len(), 3);
    let mut c = Config::default();
    c.workspace.full_notes_max = 0;
    c.workspace.notes_budget_bytes = 500;
    let (s, _) = select((0..5).map(note).collect(), c, "revisor");
    assert!(s.full_ids.len() < 5);
    // The notice is optional when the selected notes exhaust the ceiling.
    assert!(s.full_ids.len() + s.pointer_ids.len() < 5);
    assert_eq!(Config::default().workspace.full_notes_max, 2);
    assert_eq!(Config::default().workspace.notes_budget_bytes, 1500);
}
#[test]
fn laws_rules_and_tiny_budgets() {
    let mut rule = note(0);
    rule.kind = Kind::Rule;
    let mut law = note(1);
    law.kind = Kind::Law;
    assert_eq!(
        select(vec![rule, law], Config::default(), "revisor")
            .0
            .full_ids
            .len(),
        2
    );
    for budget in [1, 120, 450, 900, 1500] {
        let mut c = Config::default();
        c.workspace.budget_bytes = budget;
        select((0..30).map(note).collect(), c, "revisor deploy");
    }
    let mut c = Config::default();
    c.workspace.notes_budget_bytes = 1;
    assert!(select(vec![note(0)], c, "revisor").1.is_empty());
}
#[test]
fn full_text_and_pointers_share_the_ceiling() {
    let (s, out) = select(
        (0..30).map(note).collect(),
        Config::default(),
        "revisor deploy",
    );
    assert_eq!(s.full_ids.len(), 2);
    assert!(!s.pointer_ids.is_empty());
    let payload = out
        .lines()
        .skip(1)
        .take_while(|l| !l.starts_with("</elo"))
        .map(|l| l.len() + 1)
        .sum::<usize>();
    assert!(payload <= 1500);
}

#[test]
fn note_budget_and_full_note_max_are_per_event() {
    let mut c = Config::parse("[[project]]\nname='a'\nroot='a'\n[[project]]\nname='b'\nroot='b'\n[[link]]\nname='ab'\nprojects=['a','b']", Path::new("test")).unwrap();
    let mut items = Vec::new();
    for (layer, scope) in ["project:a", "link:ab", "global"].into_iter().enumerate() {
        for n in 0..30 {
            let mut i = note(n);
            i.id = format!("l{layer}-{}", i.id);
            i.scope = scope.into();
            items.push(i);
        }
    }
    let (s, out) = select(items.clone(), c.clone(), "a b revisor");
    let payload: usize = out
        .lines()
        .skip(1)
        .take_while(|l| !l.starts_with("</elo"))
        .map(|l| l.len() + 1)
        .sum();
    assert!(payload <= 1500);
    assert_eq!(
        s.omitted_notes.iter().map(|(n, _)| n).sum::<usize>(),
        90 - s.pointer_ids.len() - s.full_ids.len()
    );
    c.workspace.notes_budget_bytes = 0;
    items.retain(|i| i.id.ends_with("n29"));
    let (s, _) = select(items, c, "a b revisor deploy");
    assert_eq!(s.full_ids.len(), 2);
    assert!(!s.full_ids.iter().any(|id| id.starts_with("l2-")));
}

#[test]
fn recency_uses_instants_and_served_items_are_not_repeated_in_real_mode() {
    let mut a = note(0);
    a.updated_at = "2026-01-02T01:00:00+02:00".into();
    let mut b = note(1);
    b.updated_at = "2026-01-02T00:00:00Z".into();
    assert_eq!(
        select(vec![a.clone(), b], Config::default(), "revisor")
            .0
            .pointer_ids,
        ["n01", "n00"]
    );
    let fx = live_fixture(Path::new("."), Config::default(), vec![a]);
    let mut state = SessionState::default();
    state.served.insert("n00".into());
    // Real mode: an item already served in full is not pointed again in the same session.
    assert!(E1
        .select(&fx, &event("revisor deploy"), &mut state)
        .pointer_ids
        .is_empty());
}

#[test]
fn omission_only_output_has_no_items_but_nonempty_output() {
    let mut c = Config::default();
    c.workspace.notes_budget_bytes = 40;
    let (s, out) = select(vec![note(0)], c.clone(), "revisor");
    assert!(s.full_ids.is_empty() && s.pointer_ids.is_empty());
    assert!(out.contains("+1 notes (elo recall revisor)"));
    let fx = live_fixture(Path::new("."), c, vec![note(0)]);
    let log = serde_json::json!({"at":"2026-02-01T00:00:00Z","cwd":".","event_seq":1,"session_id":"s","prompt":"revisor"}).to_string();
    let report = elo_ctx::bench::replay::aggregate(&fx, log.as_bytes(), &[]).unwrap();
    assert!(report.contains("events with 0 items 1\n"));
    assert!(report.contains("events with empty output 0\n"));
}

#[test]
fn final_event_cut_reports_every_omitted_note() {
    for budget in [450, 900, 1500] {
        let mut c = Config::default();
        c.workspace.budget_bytes = budget;
        let (s, out) = select((0..30).map(note).collect(), c, "revisor");
        let omitted = 30 - s.full_ids.len() - s.pointer_ids.len();
        if !s.omitted_notes.is_empty() {
            assert_eq!(
                s.omitted_notes.iter().map(|(n, _)| n).sum::<usize>(),
                omitted
            );
            assert!(out.contains(&format!("+{omitted} notes")));
        } else {
            let fx = live_fixture(
                Path::new("."),
                {
                    let mut c = Config::default();
                    c.workspace.budget_bytes = budget;
                    c
                },
                (0..30).map(note).collect(),
            );
            let mut with_notice = s.clone();
            assert!(omitted > 0, "sem aviso só quando não sobra espaço para ele");
            with_notice.omitted_notes.push((omitted, "revisor".into()));
            assert!(render(&fx, &event("revisor"), &with_notice).len() > budget as usize);
        }
    }
}

#[test]
fn quota_rejections_do_not_consume_full_note_slots() {
    let mut c = Config::parse("[[project]]\nname='a'\nroot='a'", Path::new("test")).unwrap();
    c.workspace.notes_budget_bytes = 0;
    c.quotas.project = 0.0;
    c.quotas.link = 0.0;
    c.quotas.global = 1.0;
    let mut items: Vec<_> = (0..4).map(note).collect();
    for item in &mut items[2..] {
        item.scope = "project:a".into();
    }
    let (s, _) = select(items, c, "a revisor deploy");
    assert_eq!(s.full_ids, ["n01", "n00"]);
    assert_eq!(s.pointer_ids.len(), 2);
}

#[test]
fn oversized_full_note_is_demoted_to_pointer() {
    let mut item = note(0);
    item.body = "x".repeat(3000);
    let (s, _) = select(vec![item], Config::default(), "revisor deploy");
    assert!(s.full_ids.is_empty());
    assert_eq!(s.pointer_ids, ["n00"]);
    assert!(s.omitted_notes.is_empty());
}

#[test]
fn invalid_dates_sort_last_and_equal_instants_use_id() {
    let mut items: Vec<_> = (0..4).map(note).collect();
    items[0].updated_at = "inválido".into();
    items[1].updated_at = "".into();
    items[2].updated_at = "2026-01-02T01:00:00+01:00".into();
    items[3].updated_at = "2026-01-02T00:00:00Z".into();
    let (s, _) = select(items, Config::default(), "revisor");
    assert_eq!(s.pointer_ids, ["n02", "n03", "n00", "n01"]);
}

#[test]
fn rule_pointer_cut_keeps_original_envelope_accounting() {
    for budget in 100..800 {
        let mut c = Config::default();
        c.workspace.budget_bytes = budget;
        let mut item = note(0);
        item.kind = Kind::Rule;
        item.body = "x".repeat(3000);
        let fx = live_fixture(Path::new("."), c, vec![item]);
        let ev = event("revisor");
        let candidate = Selection::new(&fx, &ev, vec![], vec!["n00".into()]);
        let selected = E1.select(&fx, &ev, &mut SessionState::default());
        assert_eq!(
            selected.pointer_ids == ["n00"],
            candidate.used_bytes <= budget as usize
        );
        assert!(selected.omitted_notes.is_empty());
    }
}

#[test]
fn tight_budgets_preserve_exact_law_rule_output_without_notes() {
    let mut law = note(40);
    law.kind = Kind::Law;
    let mut full_rule = note(41);
    full_rule.kind = Kind::Rule;
    full_rule.body = "regra curta".into();
    let mut pointer_rule = note(42);
    pointer_rule.kind = Kind::Rule;
    pointer_rule.body = "r".repeat(5000);
    let protected = vec![law, full_rule, pointer_rule];
    for served in [false, true] {
        for budget in 100..1600 {
            let mut c = Config::default();
            c.workspace.budget_bytes = budget;
            let plain = live_fixture(Path::new("."), c.clone(), protected.clone());
            let mut items = protected.clone();
            items.extend((0..6).map(note));
            let mixed = live_fixture(Path::new("."), c, items);
            let mut state = SessionState::default();
            if served {
                state.served.insert("n40".into());
            }
            let ev = event("revisor deploy");
            let baseline = E1.select(&plain, &ev, &mut state.clone());
            let mut actual = E1.select(&mixed, &ev, &mut state);
            assert!(actual.used_bytes <= budget as usize);
            actual
                .full_ids
                .retain(|id| protected.iter().any(|i| &i.id == id));
            actual
                .pointer_ids
                .retain(|id| protected.iter().any(|i| &i.id == id));
            actual.omitted_notes.clear();
            assert_eq!(
                render(&mixed, &ev, &actual),
                render(&plain, &ev, &baseline),
                "budget={budget}, served={served}"
            );
        }
    }
}

#[test]
fn note_recency_stays_within_layers_and_rule_and_note_pointers_are_scored() {
    let mut c = Config::parse("[[project]]\nname='a'\nroot='a'\n[[project]]\nname='b'\nroot='b'\n[[link]]\nname='ab'\nprojects=['a','b']", Path::new("test")).unwrap();
    c.workspace.notes_budget_bytes = 0;
    let mut items: Vec<_> = (0..6).map(note).collect();
    for (i, item) in items.iter_mut().enumerate() {
        item.scope = ["project:a", "link:ab", "global"][i / 2].into();
    }
    let mut rule = note(10);
    rule.kind = Kind::Rule;
    rule.body = "r".repeat(10000);
    items.push(rule);
    let (s, _) = select(items, c, "a b revisor");
    assert_eq!(
        s.pointer_ids,
        // Real mode: pointers are ranked by score (ties keep input order).
        // At most 5 note pointers (MAX_NOTE_POINTERS): the lowest scored note is cut.
        ["n01", "n00", "n03", "n02", "n10", "n05"]
    );
}

#[test]
fn notice_uses_omitted_keyword_or_falls_back_to_notas() {
    for keyword in [Some("omitida"), None] {
        let mut c = Config::default();
        c.workspace.notes_budget_bytes = 100;
        let mut included = note(29);
        included.title = "curta".into();
        included.triggers.keywords = vec!["incluida".into()];
        let mut omitted = note(0);
        omitted.triggers.keywords = keyword.into_iter().map(String::from).collect();
        let (s, out) = select(vec![included, omitted], c, "incluida omitida");
        assert_eq!(s.pointer_ids, ["n29"]);
        assert_eq!(
            s.omitted_notes,
            vec![(1, keyword.unwrap_or("notas").into())]
        );
        assert!(out.contains(&format!("elo recall {}", keyword.unwrap_or("notas"))));
    }
}

#[test]
fn demoted_full_note_frees_its_slot_for_the_next_eligible() {
    let mut items: Vec<_> = (0..3).map(note).collect();
    items[0].body = "x".repeat(3000);
    items[0].updated_at = "2026-01-29T00:00:00Z".into();
    let mut c = Config::default();
    c.workspace.full_notes_max = 2;
    let (s, _) = select(items, c, "revisor deploy");
    assert!(s.pointer_ids.contains(&"n00".to_string()));
    assert!(!s.full_ids.contains(&"n00".to_string()));
    assert_eq!(
        s.full_ids.len(),
        2,
        "a vaga da n00 passa para a próxima: {:?}",
        s.full_ids
    );
    assert!(s.full_ids.iter().all(|id| !s.pointer_ids.contains(id)));
}

#[test]
fn freed_slot_skips_ineligible_notes() {
    let mut items: Vec<_> = (0..4).map(note).collect();
    items[0].body = "x".repeat(3000);
    items[0].updated_at = "2026-01-29T00:00:00Z".into();
    // n03 casa só 1 palavra: fica indexada e não pode ganhar a vaga.
    items[3].triggers.keywords = vec!["revisor".into()];
    items[3].updated_at = "2026-01-28T00:00:00Z".into();
    let mut c = Config::default();
    c.workspace.full_notes_max = 2;
    let (s, _) = select(items, c, "revisor deploy");
    assert!(!s.full_ids.contains(&"n03".to_string()));
    assert!(!s.full_ids.contains(&"n00".to_string()));
    assert_eq!(s.full_ids.len(), 2, "{:?}", s.full_ids);
}
