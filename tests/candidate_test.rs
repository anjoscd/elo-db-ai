//! An item written by an agent stops going in full once the owner rates it `bad`.
use elo_ctx::{
    hook,
    store::{NewItem, Store},
};
use serde_json::json;

fn prompt_output(dir: &std::path::Path) -> String {
    hook::prepare(
        "user-prompt-submit",
        &json!({"cwd": dir, "session_id": "s", "prompt": "como publicar o process_item"})
            .to_string(),
    )
    .unwrap()
    .unwrap_or_default()
}

#[test]
fn an_agent_item_the_owner_rated_bad_becomes_a_pointer() {
    for origin in ["agent", "user"] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("elo.toml"),
            "[workspace]\nfull_min_score = 0\n",
        )
        .unwrap();
        store
            .insert_item(&NewItem {
                id: "R",
                kind: "rule",
                scope: "global",
                title: "Publicar",
                body: "Corpo que so aparece completo zxqw.",
                origin,
            })
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE items SET triggers=?1",
                [
                    json!({"paths":[],"keywords":["process_item", "publicar"],"always":false})
                        .to_string(),
                ],
            )
            .unwrap();
        assert!(prompt_output(dir.path()).contains("zxqw"), "{origin}");
        store.record_feedback("R", false).unwrap();
        // A new session, so the item is not skipped as already served.
        let out = hook::prepare(
            "user-prompt-submit",
            &json!({"cwd": dir.path(), "session_id": "s2", "prompt": "como publicar o process_item"})
                .to_string(),
        )
        .unwrap()
        .unwrap_or_default();
        assert_eq!(out.contains("zxqw"), origin == "user", "{origin}: {out}");
        if origin == "agent" {
            assert!(out.contains("R"), "pointer expected: {out}");
        }
    }
}
