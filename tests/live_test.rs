use elo_ctx::bench::elo_selector::E1;
use elo_ctx::bench::fixture::{Event, Kind, Status};
use elo_ctx::bench::selector::{Selector, SessionState};
use elo_ctx::config::Config;
use elo_ctx::live::{live_fixture, load_items};
use elo_ctx::store::{NewItem, Store};
use rusqlite::Connection;
use tempfile::TempDir;

#[test]
fn items_round_trip_from_the_store_and_feed_the_selector() {
    let dir = TempDir::new().expect("tmp");
    let store = Store::create(dir.path()).expect("open");
    store
        .insert_item(&NewItem {
            id: "R1",
            kind: "rule",
            scope: "global",
            title: "Deploy rule",
            body: "Always run the release checklist.",
            origin: "user",
        })
        .expect("insert");
    // A second, unrelated rule: notes must not affect rule IDF, and with
    // a one-rule corpus every keyword has idf 0.
    store
        .insert_item(&NewItem {
            id: "R2",
            kind: "rule",
            scope: "global",
            title: "Unrelated",
            body: "Something else.",
            origin: "user",
        })
        .expect("insert");
    drop(store);
    let db = dir.path().join(".elo/elo.db");
    Connection::open(&db)
        .expect("raw")
        .execute(
            "UPDATE items SET triggers = '{\"keywords\":[\"release\"],\"paths\":[],\"always\":false}' WHERE id = 'R1'",
            [],
        )
        .expect("set triggers");

    let store = Store::open(dir.path()).expect("reopen");
    let items = load_items(&store).expect("load");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].kind, Kind::Rule);
    assert_eq!(items[0].status, Status::Active);
    assert_eq!(items[0].triggers.keywords, ["release"]);

    let config = Config::parse("[workspace]\nbudget_bytes = 9000\n", dir.path()).expect("cfg");
    let fx = live_fixture(dir.path(), config, items);
    let ev: Event = serde_json::from_value(serde_json::json!({
        "id": "e1", "event_type": "prompt", "session_id": "s", "family": "f", "event_seq": 1,
        "cwd": ".", "prompt": "prepare the release", "now": "2026-11-05T12:00:00Z",
        "allowed_scopes": [], "required": [], "forbidden": []
    }))
    .expect("event");
    let mut state = SessionState::default();
    state.begin_event(&ev);
    let selection = E1.select(&fx, &ev, &mut state);
    assert_eq!(selection.full_ids, ["R1"]);
}
