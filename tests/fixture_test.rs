use std::fs;
use std::path::Path;

use elo_ctx::bench::fixture::{load_fixture, FixtureError, Kind, Status};
use tempfile::TempDir;

const ITEM: &str = r#"{"id":"I1","kind":"law","scope":"global","title":"Synthetic","body":"Safe","triggers":{"keywords":[],"paths":[],"always":false},"importance":0.5,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","status":"active","origin":"user"}"#;
const EVENT: &str = r#"{"id":"E1","event_type":"local","session_id":"s1","family":"f1","prompt":"Review this change","event_seq":1,"cwd":".","now":"2026-03-01T00:00:00Z","allowed_scopes":["global"],"required":["I1"],"forbidden":[]}"#;
const SPLIT: &str = r#"{"dev":["E1"],"test":[]}"#;

fn fixture(items: &str, events: &str, split: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in [
        ("elo.toml", ""),
        ("items.jsonl", items),
        ("events.jsonl", events),
        ("split.json", split),
    ] {
        fs::write(dir.path().join(name), text).unwrap();
    }
    dir
}
fn error(items: &str, events: &str, split: &str) -> FixtureError {
    load_fixture(fixture(items, events, split).path()).unwrap_err()
}

#[test]
fn public_v0_is_valid_and_stratified_by_whole_session() {
    let f = load_fixture(Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/fixture/v0")).unwrap();
    assert_eq!((f.items.len(), f.events.len()), (60, 120));
    assert_eq!((f.config.projects.len(), f.config.links.len()), (4, 2));
    assert_eq!((f.split.dev.len(), f.split.test.len()), (72, 48));
    assert_eq!(f.items.iter().filter(|i| i.kind == Kind::Law).count(), 6);
    assert_eq!(
        f.items
            .iter()
            .filter(|i| i.status == Status::Expired)
            .count(),
        2
    );
    let mut counts = std::collections::BTreeMap::new();
    let mut sessions = std::collections::BTreeMap::new();
    for e in &f.events {
        let dev = f.split.dev.contains(&e.id);
        let count = counts.entry(&e.event_type).or_insert((0, 0));
        if dev {
            count.0 += 1;
        } else {
            count.1 += 1;
        }
        if let Some(previous) = sessions.insert(&e.session_id, dev) {
            assert_eq!(previous, dev);
        }
        for id in &e.required {
            let item = f.items.iter().find(|i| &i.id == id).unwrap();
            assert!(e.allowed_scopes.contains(&item.scope));
            assert_eq!(item.status, Status::Active);
        }
    }
    assert_eq!(counts.len(), 6);
    assert!(counts.values().all(|&count| count == (12, 8)));
}

#[test]
fn duplicate_item_and_event_ids() {
    assert!(
        matches!(error(&format!("{ITEM}\n{ITEM}"), EVENT, SPLIT), FixtureError::DuplicateId { entity: "item", id } if id == "I1")
    );
    assert!(
        matches!(error(ITEM, &format!("{EVENT}\n{EVENT}"), SPLIT), FixtureError::DuplicateId { entity: "event", id } if id == "E1")
    );
}

#[test]
fn missing_references_identify_the_field() {
    for field in ["required", "forbidden"] {
        let event = if field == "required" {
            EVENT.replace("[\"I1\"]", "[\"missing\"]")
        } else {
            EVENT.replace("\"forbidden\":[]", "\"forbidden\":[\"missing\"]")
        };
        assert!(
            matches!(error(ITEM, &event, SPLIT), FixtureError::UnknownItem { field: actual, id, .. } if actual == field && id == "missing")
        );
    }
    let item = ITEM.replace("\"origin\":", "\"superseded_by\":\"missing\",\"origin\":");
    assert!(
        matches!(error(&item, EVENT, SPLIT), FixtureError::UnknownItem { field: "superseded_by", id, .. } if id == "missing")
    );
}

#[test]
fn contradictory_labels() {
    let event = EVENT.replace("\"forbidden\":[]", "\"forbidden\":[\"I1\"]");
    assert!(
        matches!(error(ITEM, &event, SPLIT), FixtureError::LabelOverlap { event, id } if event == "E1" && id == "I1")
    );
}

#[test]
fn unknown_item_and_event_scopes() {
    for scope in ["project:missing", "link:missing", "typo"] {
        assert!(
            matches!(error(&ITEM.replace("global", scope), EVENT, SPLIT), FixtureError::UnknownScope { owner, .. } if owner == "I1")
        );
        assert!(
            matches!(error(ITEM, &EVENT.replace("global", scope), SPLIT), FixtureError::UnknownScope { owner, .. } if owner == "E1")
        );
    }
}

#[test]
fn split_must_be_an_exact_partition() {
    assert!(
        matches!(error(ITEM, EVENT, r#"{"dev":["E1","E1"],"test":[]}"#), FixtureError::DuplicateSplitEvent(id) if id == "E1")
    );
    assert!(
        matches!(error(ITEM, EVENT, r#"{"dev":["E1"],"test":["E1"]}"#), FixtureError::DuplicateSplitEvent(id) if id == "E1")
    );
    assert!(
        matches!(error(ITEM, EVENT, r#"{"dev":[],"test":[]}"#), FixtureError::MissingSplitEvent(id) if id == "E1")
    );
    assert!(
        matches!(error(ITEM, EVENT, r#"{"dev":["missing"],"test":[]}"#), FixtureError::UnknownSplitEvent(id) if id == "missing")
    );
}

#[test]
fn laws_are_limited_by_utf8_bytes() {
    let valid = ITEM.replace("Safe", &"é".repeat(200));
    assert!(load_fixture(fixture(&valid, EVENT, SPLIT).path()).is_ok());
    let invalid = ITEM.replace("Safe", &"é".repeat(201));
    assert!(
        matches!(error(&invalid, EVENT, SPLIT), FixtureError::LawTooLong { id, bytes: 402 } if id == "I1")
    );
    assert!(
        load_fixture(fixture(&invalid.replace("\"law\"", "\"note\""), EVENT, SPLIT).path()).is_ok()
    );
}

#[test]
fn rejects_self_cycles_and_longer_cycles_but_accepts_chains() {
    let first = ITEM
        .replace("active", "superseded")
        .replace("\"origin\":", "\"superseded_by\":\"I2\",\"origin\":");
    let second = ITEM.replace("I1", "I2");
    assert!(load_fixture(
        fixture(
            &format!("{first}\n{second}"),
            &EVENT.replace("I1", "I2"),
            SPLIT
        )
        .path()
    )
    .is_ok());
    let cyclic = second
        .replace("active", "superseded")
        .replace("\"origin\":", "\"superseded_by\":\"I1\",\"origin\":");
    assert!(matches!(
        error(&format!("{first}\n{cyclic}"), EVENT, SPLIT),
        FixtureError::SupersessionCycle(_)
    ));
    assert!(
        matches!(error(&first.replace("I2", "I1"), EVENT, SPLIT), FixtureError::SupersessionCycle(id) if id == "I1")
    );
}

#[test]
fn json_strings_nulls_and_numeric_forms() {
    let item = ITEM
        .replace("Safe", r#"Caf\u00e9 \ud83d\ude80\n\"quoted\" \\ /\t"#)
        .replace("0.5", "5e-1")
        .replace(
            "\"origin\":",
            "\"expires_at\":null,\"superseded_by\":null,\"origin\":",
        );
    let f = load_fixture(fixture(&item, EVENT, SPLIT).path()).unwrap();
    assert_eq!(f.items[0].body, "Café 🚀\n\"quoted\" \\ /\t");
    assert_eq!(f.items[0].importance, 0.5);
    assert!(f.items[0].expires_at.is_none());
}

#[test]
fn malformed_json_reports_file_and_line() {
    for invalid in [
        "{",
        "null",
        "{} trailing",
        "{\"x\":01}",
        "{\"x\":1.}",
        "{\"x\":1e}",
        "{\"x\":true,}",
        r#"{"x":"\ud800"}"#,
        r#"{"x":"\q"}"#,
        r#"{"x":1,"x":2}"#,
    ] {
        assert!(
            matches!(error(&format!("{ITEM}\n{invalid}"), EVENT, SPLIT), FixtureError::Parse { path, line: 2, .. } if path.ends_with("items.jsonl"))
        );
    }
    assert!(
        matches!(error(ITEM, "{}", SPLIT), FixtureError::Parse { path, line: 1, .. } if path.ends_with("events.jsonl"))
    );
    assert!(
        matches!(error(ITEM, EVENT, "[]"), FixtureError::Parse { path, .. } if path.ends_with("split.json"))
    );
}

#[test]
fn missing_files_and_invalid_config_are_reported() {
    let dir = fixture(ITEM, EVENT, SPLIT);
    fs::remove_file(dir.path().join("items.jsonl")).unwrap();
    assert!(
        matches!(load_fixture(dir.path()), Err(FixtureError::Read { path, .. }) if path.ends_with("items.jsonl"))
    );
    fs::write(dir.path().join("elo.toml"), "invalid = [").unwrap();
    assert!(matches!(
        load_fixture(dir.path()),
        Err(FixtureError::Config(_))
    ));
}

#[test]
fn families_and_sessions_cannot_cross_the_split() {
    let split = r#"{"dev":["E1"],"test":["E2"]}"#;
    let second = EVENT
        .replace("E1", "E2")
        .replace("Review this change", "Inspect that patch");
    for (event, expected) in [
        (second.replace("s1", "s2"), "family"),
        (second.replace("f1", "f2"), "session"),
    ] {
        assert!(matches!(
            error(ITEM, &format!("{EVENT}\n{event}"), split),
            FixtureError::SplitLeak { entity, .. } if entity == expected
        ));
    }
    // Repeated families across independent sessions are legal on the same side.
    let events = format!("{EVENT}\n{}", second.replace("s1", "s2"));
    assert!(
        load_fixture(fixture(ITEM, &events, r#"{"dev":["E1","E2"],"test":[]}"#).path()).is_ok()
    );
    assert!(matches!(
        error(ITEM, &EVENT.replace("f1", " "), SPLIT),
        FixtureError::EmptyFamily(_)
    ));
}

#[test]
fn normalized_prompts_must_be_unique() {
    for prompt in [
        "Review this change",
        "REVIEW   this\\tchange",
        " review\\nthis change ",
    ] {
        let second = EVENT
            .replace("E1", "E2")
            .replace("Review this change", prompt);
        assert!(matches!(
            error(ITEM, &format!("{EVENT}\n{second}"), r#"{"dev":["E1","E2"],"test":[]}"#),
            FixtureError::DuplicatePrompt { first, second } if first == "E1" && second == "E2"
        ));
    }
    let unicode = EVENT.replace("Review this change", "AÇÃO com espaços");
    let second = unicode
        .replace("E1", "E2")
        .replace("AÇÃO com espaços", "ação\u{a0}com espaços");
    assert!(matches!(
        error(ITEM, &format!("{unicode}\n{second}"), SPLIT),
        FixtureError::DuplicatePrompt { .. }
    ));
}

#[test]
fn invalid_jsonl_event_reports_physical_line() {
    for invalid in ["{", "", r#"{"prompt":"broken\q"}"#] {
        let events = format!("{EVENT}\n{invalid}\n");
        let err = error(ITEM, &events, SPLIT);
        assert!(err.to_string().contains("line 2"));
        assert!(
            matches!(err, FixtureError::Parse { path, line: 2, .. } if path.ends_with("events.jsonl"))
        );
    }
}

#[test]
fn unknown_and_duplicate_json_fields_are_rejected() {
    for item in [
        ITEM.replace("\"id\":", "\"extra\":true,\"id\":"),
        ITEM.replace("\"id\":", "\"id\":\"other\",\"id\":"),
        ITEM.replace("\"always\":", "\"extra\":true,\"always\":"),
    ] {
        assert!(matches!(
            error(&item, EVENT, SPLIT),
            FixtureError::Parse { .. }
        ));
    }
    assert!(matches!(
        error(
            ITEM,
            &EVENT.replace("\"id\":", "\"extra\":true,\"id\":"),
            SPLIT
        ),
        FixtureError::Parse { .. }
    ));
    assert!(matches!(
        error(
            ITEM,
            EVENT,
            &SPLIT.replace("\"dev\":", "\"extra\":true,\"dev\":")
        ),
        FixtureError::Parse { .. }
    ));
}

fn words(prompt: &str) -> std::collections::BTreeSet<String> {
    prompt
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn public_v0_has_no_near_duplicates_across_split() {
    let f = load_fixture(Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/fixture/v0")).unwrap();
    let mut maximum = (0.0_f64, "", "");
    for dev in f.events.iter().filter(|e| f.split.dev.contains(&e.id)) {
        let left = words(dev.prompt.as_deref().unwrap());
        for test in f.events.iter().filter(|e| f.split.test.contains(&e.id)) {
            let right = words(test.prompt.as_deref().unwrap());
            let union = left.union(&right).count();
            let score = if union == 0 {
                1.0
            } else {
                left.intersection(&right).count() as f64 / union as f64
            };
            if score > maximum.0 {
                maximum = (score, &dev.id, &test.id);
            }
            if score > 0.8 {
                eprintln!(
                    "WARNING: dev {} and test {} have word Jaccard {score:.6} > 0.8",
                    dev.id, test.id
                );
            }
        }
    }
    assert!(maximum.0 <= 0.8, "cross-split near duplicate: {maximum:?}");
    let distinct: std::collections::BTreeSet<_> = f
        .events
        .iter()
        .map(|e| e.prompt.as_ref().unwrap())
        .collect();
    assert_eq!(distinct.len(), 120);
    // Each authored card starts in Portuguese; the remaining turns are English.
    let portuguese = f.events.iter().filter(|e| e.event_seq == 1).count();
    assert_eq!(portuguese, 30);
    let code = f
        .events
        .iter()
        .filter(|e| e.prompt.as_ref().unwrap().contains('`'))
        .count();
    assert!(code > 0);
    let families = |ids: &[String]| -> std::collections::BTreeSet<_> {
        f.events
            .iter()
            .filter(|e| ids.contains(&e.id))
            .map(|e| &e.family)
            .collect()
    };
    let dev = families(&f.split.dev);
    let test = families(&f.split.test);
    assert!(dev.is_disjoint(&test));
    println!("distinct prompts: {}; Portuguese: {portuguese}/120 (25%); code: {code}; families dev/test: {}/{}; max Jaccard: {:.6} ({} / {})", distinct.len(), dev.len(), test.len(), maximum.0, maximum.1, maximum.2);
}

#[test]
fn code_only_triggers_are_negative_fixture_examples() {
    let f = load_fixture(Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/fixture/v0")).unwrap();
    let literal: Vec<_> = f
        .events
        .iter()
        .filter(|e| e.family == "literal-text")
        .collect();
    assert_eq!(literal.len(), 4);
    for event in &literal {
        assert_eq!(event.allowed_scopes, ["global"]);
        assert!(event.required.is_empty());
        assert!(event.tool_paths.as_ref().is_none_or(Vec::is_empty));
    }
    for (index, keyword, forbidden) in [
        (0, "checkout", "R16"),
        (1, "release-check", "L1"),
        (2, "invoice", "R17"),
        (3, "refund-policy", "D6"),
    ] {
        let event = literal[index];
        let prompt = event.prompt.as_ref().unwrap();
        let start = prompt.find('`').unwrap();
        let end = prompt.rfind('`').unwrap();
        assert!(prompt[start..=end].contains(keyword));
        assert!(!prompt[..start].contains(keyword));
        assert!(!prompt[end + 1..].contains(keyword));
        assert!(event.forbidden.iter().any(|id| id == forbidden));
    }
}

fn changed(text: &str, field: &str, value: serde_json::Value) -> String {
    let mut json: serde_json::Value = serde_json::from_str(text).unwrap();
    json[field] = value;
    json.to_string()
}

#[test]
fn required_scope_status_and_expiration() {
    use serde_json::json;
    assert!(matches!(
        error(ITEM, &changed(EVENT, "allowed_scopes", json!([])), SPLIT),
        FixtureError::RequiredScope { .. }
    ));
    for status in ["expired", "archived"] {
        assert!(matches!(
            error(&changed(ITEM, "status", json!(status)), EVENT, SPLIT),
            FixtureError::RequiredInactive { .. }
        ));
    }
    for date in [
        "2026-02-28T23:59:59Z",
        "2026-03-01T00:00:00Z",
        "2026-03-01T01:00:00+01:00",
    ] {
        assert!(matches!(
            error(&changed(ITEM, "expires_at", json!(date)), EVENT, SPLIT),
            FixtureError::RequiredExpired { .. }
        ));
    }
    for date in ["2026-03-01T00:00:00.001Z", "2026-02-28T23:00:01-01:00"] {
        assert!(load_fixture(
            fixture(&changed(ITEM, "expires_at", json!(date)), EVENT, SPLIT).path()
        )
        .is_ok());
    }
    for (item, event) in [
        (changed(ITEM, "expires_at", json!("invalid")), EVENT.into()),
        (
            changed(ITEM, "expires_at", json!("2027-01-01T00:00:00Z")),
            changed(EVENT, "now", json!("invalid")),
        ),
    ] {
        assert!(matches!(
            error(&item, &event, SPLIT),
            FixtureError::InvalidTimestamp { .. }
        ));
    }
}

#[test]
fn supersession_requires_status_kind_scope_and_active_end() {
    use serde_json::json;
    assert!(matches!(
        error(&changed(ITEM, "status", json!("superseded")), EVENT, SPLIT),
        FixtureError::SupersessionStatus(_)
    ));
    let first = changed(ITEM, "superseded_by", json!("I2"));
    let second = ITEM.replace("I1", "I2");
    assert!(matches!(
        error(&format!("{first}\n{second}"), EVENT, SPLIT),
        FixtureError::SupersessionStatus(_)
    ));
    let first = first.replace("active", "superseded");
    let event = EVENT.replace("I1", "I2");
    assert!(matches!(
        error(
            &format!("{first}\n{}", changed(&second, "kind", json!("note"))),
            &event,
            SPLIT
        ),
        FixtureError::SupersessionKind(_)
    ));
    let dir = fixture(
        &format!(
            "{first}\n{}",
            changed(&second, "scope", json!("project:api"))
        ),
        &event,
        SPLIT,
    );
    fs::write(
        dir.path().join("elo.toml"),
        "[[project]]\nname = 'api'\nroot = 'api'\n",
    )
    .unwrap();
    assert!(matches!(
        load_fixture(dir.path()),
        Err(FixtureError::SupersessionScope(_))
    ));
    for status in ["expired", "archived"] {
        assert!(matches!(
            error(
                &format!("{first}\n{}", changed(&second, "status", json!(status))),
                &event,
                SPLIT
            ),
            FixtureError::SupersessionInactiveEnd(_)
        ));
    }
    assert!(matches!(
        error(&format!("{first}\n{second}"), EVENT, SPLIT),
        FixtureError::RequiredInactive { .. }
    ));
    let middle = changed(&second, "superseded_by", json!("I3")).replace("active", "superseded");
    assert!(load_fixture(
        fixture(
            &format!("{first}\n{middle}\n{}", ITEM.replace("I1", "I3")),
            &EVENT.replace("I1", "I3"),
            SPLIT
        )
        .path()
    )
    .is_ok());
}

#[test]
fn law_origin_and_always_are_validated_without_keyword_requirement() {
    use serde_json::json;
    for origin in ["agent", "import"] {
        assert!(matches!(
            error(&changed(ITEM, "origin", json!(origin)), EVENT, SPLIT),
            FixtureError::LawOrigin(_)
        ));
    }
    assert!(matches!(
        error(&ITEM.replace("false", "true"), EVENT, SPLIT),
        FixtureError::LawAlways(_)
    ));
    assert!(load_fixture(fixture(ITEM, EVENT, SPLIT).path()).is_ok());
}

#[test]
fn split_reports_real_line_and_column() {
    let split = "{\n  \"dev\": [\"E1\"],\n  \"test\": @\n}";
    let expected = serde_json::from_str::<serde_json::Value>(split).unwrap_err();
    assert!(
        matches!(error(ITEM, EVENT, split), FixtureError::Parse { path, line, column, .. } if path.ends_with("split.json") && line == expected.line() && column == expected.column() && line == 3)
    );
}

#[test]
fn prompt_or_tool_paths_are_required() {
    use serde_json::json;
    for prompt in ["", " \t\n"] {
        let event = changed(EVENT, "prompt", json!(prompt));
        assert!(matches!(
            error(ITEM, &event, SPLIT),
            FixtureError::EmptyPrompt(_)
        ));
        assert!(matches!(
            error(
                ITEM,
                &changed(&event, "tool_paths", json!(["src/main.rs"])),
                SPLIT
            ),
            FixtureError::EmptyPrompt(_)
        ));
    }
    let event = changed(EVENT, "prompt", json!(null));
    for paths in [json!(null), json!([])] {
        assert!(matches!(
            error(ITEM, &changed(&event, "tool_paths", paths), SPLIT),
            FixtureError::MissingToolPaths(_)
        ));
    }
    let event = changed(&event, "tool_paths", json!(["src/main.rs"]));
    assert!(load_fixture(fixture(ITEM, &event, SPLIT).path()).is_ok());
    let mut absent: serde_json::Value = serde_json::from_str(&event).unwrap();
    absent.as_object_mut().unwrap().remove("prompt");
    assert!(load_fixture(fixture(ITEM, &absent.to_string(), SPLIT).path()).is_ok());
    absent.as_object_mut().unwrap().remove("tool_paths");
    assert!(matches!(
        error(ITEM, &absent.to_string(), SPLIT),
        FixtureError::MissingToolPaths(_)
    ));
}

#[test]
fn identifiers_and_event_lists_are_strict() {
    use serde_json::json;
    for empty in ["", " \t"] {
        assert!(matches!(
            error(&changed(ITEM, "id", json!(empty)), EVENT, SPLIT),
            FixtureError::EmptyId { field: "id", .. }
        ));
        for field in ["id", "session_id"] {
            assert!(
                matches!(error(ITEM, &changed(EVENT, field, json!(empty)), SPLIT), FixtureError::EmptyId { field: actual, .. } if actual == field)
            );
        }
        assert!(matches!(
            error(ITEM, &changed(EVENT, "family", json!(empty)), SPLIT),
            FixtureError::EmptyFamily(_)
        ));
    }
    for (field, value) in [
        ("required", "I1"),
        ("forbidden", "I1"),
        ("allowed_scopes", "global"),
    ] {
        assert!(
            matches!(error(ITEM, &changed(EVENT, field, json!([value, value])), SPLIT), FixtureError::DuplicateEntry { field: actual, .. } if actual == field)
        );
    }
}
