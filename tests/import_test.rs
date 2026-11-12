use std::fs;

use elo_ctx::bench::fixture::Kind;
use elo_ctx::import::import;
use elo_ctx::live::load_items;
use elo_ctx::store::Store;
use tempfile::TempDir;

fn write(dir: &TempDir, lines: &[&str]) -> std::path::PathBuf {
    let p = dir.path().join("in.jsonl");
    fs::write(&p, lines.join("\n")).expect("write");
    p
}

const LAW: &str = r#"{"id":"L1","kind":"law","scope":"global","title":"No prod migrations","body":"Never run migrations against production.","keywords":["migration"]}"#;
const LONG_LAW: &str = r#"{"id":"L2","kind":"law","scope":"global","title":"Long","body":"XXXX"}"#;
const RULE: &str = r#"{"id":"R1","kind":"rule","scope":"project:api","title":"API rule","body":"Use the client.","keywords":["client"],"importance":0.9}"#;

#[test]
fn laws_need_as_user_and_nothing_changes_silently() {
    let dir = TempDir::new().expect("tmp");
    let mut store = Store::create(dir.path()).expect("open");
    let file = write(&dir, &[LAW, RULE]);
    let r = import(&mut store, &file, false, false).expect("import");
    assert_eq!(r.imported, 2);
    assert_eq!(r.downgraded.len(), 1);
    let items = load_items(&store).expect("load");
    let l1 = items.iter().find(|i| i.id == "L1").expect("L1");
    assert_eq!(l1.kind, Kind::Rule);

    let r = import(&mut store, &file, true, false).expect("reimport as user");
    assert!(r.downgraded.is_empty());
    let items = load_items(&store).expect("load");
    assert_eq!(items.len(), 2, "upsert, no duplicates");
    assert_eq!(
        items.iter().find(|i| i.id == "L1").expect("L1").kind,
        Kind::Law
    );
    assert_eq!(
        items
            .iter()
            .find(|i| i.id == "R1")
            .expect("R1")
            .triggers
            .keywords,
        ["client"]
    );
}

#[test]
fn oversized_laws_are_reported_and_kept_as_rules() {
    let dir = TempDir::new().expect("tmp");
    let mut store = Store::create(dir.path()).expect("open");
    let long = LONG_LAW.replace("XXXX", &"a".repeat(401));
    let file = write(&dir, &[&long]);
    let r = import(&mut store, &file, true, false).expect("import");
    assert_eq!(r.downgraded.len(), 1);
    assert!(r.downgraded[0].1.contains("401 bytes"));
}

#[test]
fn bad_lines_are_rejected_with_line_numbers_and_dry_run_writes_nothing() {
    let dir = TempDir::new().expect("tmp");
    let mut store = Store::create(dir.path()).expect("open");
    let file = write(
        &dir,
        &[
            RULE,
            "not json",
            r#"{"id":"X","kind":"weird","scope":"global","title":"t","body":"b"}"#,
            r#"{"id":"Y","kind":"note","scope":"elsewhere","title":"t","body":"b"}"#,
        ],
    );
    let r = import(&mut store, &file, false, true).expect("dry run");
    assert_eq!(r.imported, 1);
    let lines: Vec<_> = r.rejected.iter().map(|(n, _, _)| *n).collect();
    assert_eq!(lines, [2, 3, 4]);
    assert!(
        load_items(&store).expect("load").is_empty(),
        "dry run wrote nothing"
    );
}

#[test]
fn search_index_follows_upserts() {
    let dir = TempDir::new().expect("tmp");
    let mut store = Store::create(dir.path()).expect("open");
    import(&mut store, &write(&dir, &[RULE]), false, false).expect("first");
    let changed = RULE.replace("Use the client.", "Prefer the gateway.");
    import(&mut store, &write(&dir, &[&changed]), false, false).expect("second");
    assert_eq!(store.search("gateway").expect("search"), ["R1"]);
    assert!(store.search("client").expect("search").is_empty());
}

#[test]
fn untrusted_imports_cannot_touch_user_items_or_set_always() {
    let dir = TempDir::new().expect("tmp");
    let mut store = Store::create(dir.path()).expect("open");
    import(&mut store, &write(&dir, &[LAW]), true, false).expect("as user");
    let hostile = LAW.replace("Never run migrations against production.", "Anything goes.");
    let always =
        r#"{"id":"A1","kind":"rule","scope":"global","title":"t","body":"b","always":true}"#;
    let dup = RULE;
    let r = import(
        &mut store,
        &write(&dir, &[&hostile, always, RULE, dup]),
        false,
        false,
    )
    .expect("untrusted");
    assert_eq!(r.protected, ["L1"]);
    assert!(r
        .downgraded
        .iter()
        .any(|(id, why)| id == "A1" && why.contains("always")));
    assert_eq!(r.rejected.len(), 1, "duplicate id rejected");
    let items = load_items(&store).expect("load");
    let l1 = items.iter().find(|i| i.id == "L1").expect("L1");
    assert_eq!(l1.kind, Kind::Law);
    assert!(l1.body.contains("Never run migrations"));
    assert!(
        !items
            .iter()
            .find(|i| i.id == "A1")
            .expect("A1")
            .triggers
            .always
    );
}

fn decision(id: &str, scope: &str, extra: &str) -> String {
    format!(
        r#"{{"id":"{id}","kind":"decision","scope":"{scope}","title":"t {id}","body":"corpo {id}"{extra}}}"#
    )
}

fn status_of(store: &Store, id: &str) -> String {
    store
        .conn
        .query_row("SELECT status FROM items WHERE id = ?1", [id], |r| r.get(0))
        .expect("status")
}

#[test]
fn import_with_supersedes_links_valid_and_rejects_unknown_targets() {
    let dir = TempDir::new().expect("tmp");
    let mut store = Store::create(dir.path()).expect("open");
    let old = decision("D1", "project:a", "");
    let new = decision("D2", "project:a", r#","supersedes":"D1""#);
    let r = import(&mut store, &write(&dir, &[&old, &new]), false, false).expect("ok");
    assert_eq!(r.imported, 2);
    assert!(r.rejected.is_empty());
    assert_eq!(status_of(&store, "D1"), "superseded");

    let ghost = decision("D3", "project:a", r#","supersedes":"nope""#);
    let r = import(&mut store, &write(&dir, &[&ghost]), false, false).expect("ghost");
    assert_eq!(r.imported, 0);
    assert_eq!(r.rejected.len(), 1);
    assert!(r.rejected[0].2.contains("supersedes"), "{:?}", r.rejected);
    let stored: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM items WHERE id = 'D3'", [], |r| {
            r.get(0)
        })
        .expect("count");
    assert_eq!(stored, 0, "nothing written for the rejected line");
}

#[test]
fn upsert_cannot_move_a_chained_item_to_another_scope() {
    let dir = TempDir::new().expect("tmp");
    let mut store = Store::create(dir.path()).expect("open");
    let old = decision("D1", "project:a", "");
    let new = decision("D2", "project:a", r#","supersedes":"D1""#);
    import(&mut store, &write(&dir, &[&old, &new]), false, false).expect("ok");
    let moved = decision("D2", "project:b", "");
    let r = import(&mut store, &write(&dir, &[&moved]), false, false).expect("moved");
    assert_eq!(r.imported, 0);
    assert!(
        r.rejected[0].2.contains("supersession chain"),
        "{:?}",
        r.rejected
    );
    let scope: String = store
        .conn
        .query_row("SELECT scope FROM items WHERE id = 'D2'", [], |r| r.get(0))
        .expect("scope");
    assert_eq!(scope, "project:a");
}

#[test]
fn import_marks_overdue_items_expired() {
    let dir = TempDir::new().expect("tmp");
    let mut store = Store::create(dir.path()).expect("open");
    import(
        &mut store,
        &write(&dir, &[&decision("N1", "global", "")]),
        false,
        false,
    )
    .expect("first");
    store
        .conn
        .execute(
            "UPDATE items SET expires_at = 1000, updated_at = 5 WHERE id = 'N1'",
            [],
        )
        .expect("expire");
    import(
        &mut store,
        &write(&dir, &[&decision("N2", "global", "")]),
        false,
        false,
    )
    .expect("second");
    assert_eq!(status_of(&store, "N1"), "expired");
    let updated: i64 = store
        .conn
        .query_row("SELECT updated_at FROM items WHERE id = 'N1'", [], |r| {
            r.get(0)
        })
        .expect("updated");
    assert_eq!(updated, 5, "sweeping does not touch updated_at");
}
