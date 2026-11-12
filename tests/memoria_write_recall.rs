use std::fs;

use elo_ctx::hook::prepare;
use elo_ctx::recall::{recall, Query};
use elo_ctx::store::Store;
use elo_ctx::write::{add_at, forget_at, promote_at, supersede_at, Link, NewEntry, WriteError};
use tempfile::TempDir;

const NOW: i64 = 1_800_000_000;

fn entry(kind: &str, scope: &str, title: &str, body: &str) -> NewEntry {
    NewEntry {
        kind: kind.into(),
        scope: scope.into(),
        title: title.into(),
        body: body.into(),
        triggers: vec!["database".into()],
        importance: 0.5,
        supersedes: None,
        expires_at: None,
        id: None,
        as_user: false,
    }
}

fn store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("tmp");
    let store = Store::create(dir.path()).expect("open");
    (dir, store)
}

fn refused(result: Result<String, WriteError>, needle: &str) {
    match result {
        Err(e) => assert!(e.to_string().contains(needle), "{e} lacks {needle}"),
        Ok(id) => panic!("accepted {id}, expected refusal with {needle}"),
    }
}

fn status(store: &Store, id: &str) -> String {
    store
        .conn
        .query_row("SELECT status FROM items WHERE id = ?1", [id], |r| r.get(0))
        .expect("status")
}

#[test]
fn add_defaults_to_agent_and_generates_readable_id() {
    let (_d, mut s) = store();
    let id = add_at(
        &mut s,
        &entry("decision", "global", "Usar Postgres é ótimo", "x"),
        NOW,
    )
    .expect("add");
    assert_eq!(id, "decision-20270115-usar-postgres-e-otimo");
    let again = add_at(
        &mut s,
        &entry("decision", "global", "Usar Postgres é ótimo", "y"),
        NOW,
    )
    .expect("add");
    assert_eq!(again, format!("{id}-2"));
    let origin: String = s
        .conn
        .query_row("SELECT origin FROM items WHERE id = ?1", [&id], |r| {
            r.get(0)
        })
        .expect("origin");
    assert_eq!(origin, "agent");
}

#[test]
fn law_rules_and_refusals() {
    let (_d, mut s) = store();
    refused(
        add_at(&mut s, &entry("law", "global", "t", "b"), NOW),
        "--as-user",
    );
    let mut law = entry("law", "global", "t", &"x".repeat(401));
    law.as_user = true;
    refused(add_at(&mut s, &law, NOW), "400");
    law.body = "x".repeat(400);
    law.id = Some("L1".into());
    assert_eq!(add_at(&mut s, &law, NOW).expect("law"), "L1");
    let origin: String = s
        .conn
        .query_row("SELECT origin FROM items WHERE id='L1'", [], |r| r.get(0))
        .expect("origin");
    assert_eq!(origin, "user");
    for kind in ["law", "rule"] {
        let mut e = entry(kind, "global", "t", "b");
        e.as_user = true;
        e.expires_at = Some(NOW + 10);
        refused(add_at(&mut s, &e, NOW), "--expires");
    }
    let mut past = entry("note", "global", "t", "b");
    past.expires_at = Some(NOW - 1);
    refused(add_at(&mut s, &past, NOW), "future");
    let mut dup = entry("note", "global", "t", "b");
    dup.id = Some("L1".into());
    refused(add_at(&mut s, &dup, NOW), "already exists");
    refused(
        add_at(&mut s, &entry("note", "oops", "t", "b"), NOW),
        "scope",
    );
}

#[test]
fn supersede_invariants() {
    let (_d, mut s) = store();
    let mut mk = |kind: &str, scope: &str, id: &str| {
        let mut e = entry(kind, scope, id, "corpo");
        e.id = Some(id.into());
        add_at(&mut s, &e, NOW).expect("add")
    };
    mk("decision", "project:api", "d1");
    mk("decision", "project:api", "d2");
    mk("decision", "project:api", "d3");
    mk("note", "project:api", "n1");
    mk("decision", "global", "dg");
    mk("decision", "project:web", "dw");
    mk("rule", "project:api", "r1");

    supersede_at(&mut s, "d1", "d2", NOW).expect("supersede");
    assert_eq!(status(&s, "d1"), "superseded");
    // second supersession is refused and names the current successor
    match supersede_at(&mut s, "d1", "d3", NOW) {
        Err(WriteError::AlreadySuperseded { successor, .. }) => assert_eq!(successor, "d2"),
        other => panic!("{other:?}"),
    }
    assert!(
        supersede_at(&mut s, "d2", "d1", NOW).is_err(),
        "successor must be active"
    );
    assert!(supersede_at(&mut s, "d3", "d3", NOW).is_err());
    assert!(
        supersede_at(&mut s, "d3", "n1", NOW).is_err(),
        "kinds differ"
    );
    assert!(supersede_at(&mut s, "d3", "missing", NOW).is_err());
    assert!(supersede_at(&mut s, "r1", "d3", NOW).is_err());
    assert!(
        supersede_at(&mut s, "dg", "dw", NOW).is_err(),
        "narrower scope"
    );
    assert!(
        supersede_at(&mut s, "dw", "d3", NOW).is_err(),
        "different project"
    );
    supersede_at(&mut s, "d3", "dg", NOW).expect("wider scope is allowed");
}

#[test]
fn repeated_supersede_with_same_successor_is_ok_but_other_is_error() {
    let (_d, mut s) = store();
    for id in ["d1", "d2", "d3"] {
        let mut e = entry("decision", "global", id, "corpo");
        e.id = Some(id.into());
        add_at(&mut s, &e, NOW).expect("add");
    }
    assert_eq!(
        supersede_at(&mut s, "d1", "d2", NOW).expect("first"),
        Link::Apply
    );
    assert_eq!(
        supersede_at(&mut s, "d1", "d2", NOW).expect("again"),
        Link::AlreadyLinked
    );
    assert!(matches!(
        supersede_at(&mut s, "d1", "d3", NOW),
        Err(WriteError::AlreadySuperseded { .. })
    ));
}

#[test]
fn supersede_reports_a_real_cycle() {
    let (_d, mut s) = store();
    for id in ["d1", "d2", "d3"] {
        let mut e = entry("decision", "global", id, "corpo");
        e.id = Some(id.into());
        add_at(&mut s, &e, NOW).expect("add");
    }
    s.conn
        .execute(
            "UPDATE items SET status='superseded', superseded_by='d3' WHERE id='d2'",
            [],
        )
        .expect("edit");
    s.conn
        .execute(
            "UPDATE items SET status='superseded', superseded_by='d1' WHERE id='d3'",
            [],
        )
        .expect("edit");
    s.conn
        .execute(
            "UPDATE items SET status='active', superseded_by=NULL WHERE id='d1'",
            [],
        )
        .expect("edit");
    let err = supersede_at(&mut s, "d1", "d2", NOW).expect_err("cycle");
    assert!(matches!(err, WriteError::Refused(_)), "{err:?}");
    assert!(err.to_string().contains("cycle"), "{err}");
}

#[test]
fn promote_and_forget_respect_supersession_chains() {
    let (_d, mut s) = store();
    for id in ["old", "new"] {
        let mut e = entry("decision", "global", id, "corpo");
        e.id = Some(id.into());
        e.as_user = true;
        add_at(&mut s, &e, NOW).expect("add");
    }
    supersede_at(&mut s, "old", "new", NOW).expect("supersede");
    let err = promote_at(&mut s, "new", NOW).expect_err("successor");
    assert!(err.to_string().contains("supersession chain"), "{err}");
    let err = promote_at(&mut s, "old", NOW).expect_err("replaced");
    assert!(err.to_string().contains("supersession chain"), "{err}");
    let err = forget_at(&mut s, "new", NOW).expect_err("successor");
    assert!(err.to_string().contains("successor of old"), "{err}");
    assert_eq!(status(&s, "new"), "active");
}

#[test]
fn law_limit_is_measured_on_the_trimmed_body() {
    let (_d, mut s) = store();
    let mut e = entry("law", "global", "l", &format!("  {}  \n", "x".repeat(400)));
    e.as_user = true;
    add_at(&mut s, &e, NOW).expect("400 trimmed bytes fit");
    e.body = "x".repeat(401);
    refused(add_at(&mut s, &e, NOW), "limit");
}

#[test]
fn sweep_leaves_updated_at_alone_and_all_opens_have_busy_timeout() {
    let (d, mut s) = store();
    let mut e = entry("note", "global", "n", "corpo");
    e.id = Some("n".into());
    e.expires_at = Some(NOW + 10);
    add_at(&mut s, &e, NOW).expect("add");
    let mut other = entry("note", "global", "o", "corpo");
    other.id = Some("o".into());
    add_at(&mut s, &other, NOW + 20).expect("sweeps n");
    assert_eq!(status(&s, "n"), "expired");
    let updated: i64 = s
        .conn
        .query_row("SELECT updated_at FROM items WHERE id = 'n'", [], |r| {
            r.get(0)
        })
        .expect("updated");
    assert_eq!(updated, NOW);

    let timeout = |store: &Store| -> i64 {
        store
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .expect("pragma")
    };
    assert!(timeout(&s) > 0);
    drop(s);
    let ro = Store::open_read_only(d.path()).expect("ro");
    assert!(timeout(&ro) > 0);
}

fn hook_out(dir: &TempDir, session: &str, prompt: &str) -> String {
    let input = serde_json::json!({
        "session_id": session,
        "cwd": dir.path().join("api"),
        "prompt": prompt
    })
    .to_string();
    prepare("user-prompt-submit", &input)
        .expect("hook")
        .unwrap_or_default()
}

#[test]
fn hook_delivers_future_expiry_and_never_re_emits_filtered_ids() {
    let dir = TempDir::new().expect("tmp");
    fs::create_dir_all(dir.path().join("api")).expect("mkdir");
    fs::write(dir.path().join("api/package.json"), "{}").expect("manifest");
    let mut s = Store::create(dir.path()).expect("open");
    let now = chrono::Utc::now().timestamp();
    let mut e = entry("note", "global", "futuro", "zebrafutura always relevant");
    e.id = Some("futuro".into());
    e.triggers = vec!["zebrafutura".into()];
    e.importance = 1.0;
    e.expires_at = Some(now + 3600);
    add_at(&mut s, &e, now).expect("add");
    drop(s);

    let first = hook_out(&dir, "s2", "zebrafutura");
    assert!(first.contains("futuro"), "{first}");

    let mut s = Store::open(dir.path()).expect("reopen");
    forget_at(&mut s, "futuro", now).expect("forget");
    drop(s);
    let second = hook_out(&dir, "s2", "zebrafutura");
    assert!(!second.contains("futuro"), "{second}");
}

#[test]
fn add_with_supersedes_is_atomic() {
    let (_d, mut s) = store();
    let mut old = entry("decision", "project:api", "old", "velho");
    old.id = Some("old".into());
    add_at(&mut s, &old, NOW).expect("old");
    let mut bad = entry("note", "project:api", "new", "novo");
    bad.supersedes = Some("old".into());
    refused(add_at(&mut s, &bad, NOW), "kinds must match");
    let count: i64 = s
        .conn
        .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
        .expect("count");
    assert_eq!(count, 1, "failed write leaves nothing behind");
    let mut good = entry("decision", "project:api", "new", "novo");
    good.supersedes = Some("old".into());
    good.id = Some("new".into());
    add_at(&mut s, &good, NOW).expect("good");
    assert_eq!(status(&s, "old"), "superseded");
    let mut law = entry("law", "global", "l", "b");
    law.as_user = true;
    law.supersedes = Some("new".into());
    refused(add_at(&mut s, &law, NOW), "cannot supersede");
}

#[test]
fn promote_and_forget() {
    let (_d, mut s) = store();
    let mut user = entry("rule", "global", "r", "regra curta");
    user.id = Some("u".into());
    user.as_user = true;
    add_at(&mut s, &user, NOW).expect("user");
    let mut agent = entry("rule", "global", "r", "regra do agente");
    agent.id = Some("a".into());
    add_at(&mut s, &agent, NOW).expect("agent");
    assert!(
        promote_at(&mut s, "a", NOW).is_err(),
        "agent origin is not promotable"
    );
    promote_at(&mut s, "u", NOW).expect("promote");
    let (kind, evidence): (String, String) = s
        .conn
        .query_row("SELECT kind, evidence FROM items WHERE id='u'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("row");
    assert_eq!(kind, "law");
    assert!(evidence.contains("promoted to law by user"));
    assert!(promote_at(&mut s, "u", NOW).is_err(), "already a law");

    forget_at(&mut s, "a", NOW).expect("forget");
    assert_eq!(status(&s, "a"), "archived");
    assert!(forget_at(&mut s, "a", NOW).is_err());
    assert!(forget_at(&mut s, "nope", NOW).is_err());
}

#[test]
fn recall_ranks_filters_and_folds_accents() {
    let (_d, mut s) = store();
    let mut a = entry(
        "decision",
        "project:api",
        "Decisão sobre cache",
        "usar redis para cache de sessão",
    );
    a.id = Some("d-old".into());
    add_at(&mut s, &a, NOW).expect("a");
    let mut b = entry(
        "decision",
        "project:api",
        "Decisão sobre cache",
        "usar memcached para cache",
    );
    b.id = Some("d-new".into());
    b.supersedes = Some("d-old".into());
    add_at(&mut s, &b, NOW).expect("b");
    let mut c = entry("note", "project:web", "Outra", "cache do navegador");
    c.id = Some("n-web".into());
    c.expires_at = Some(NOW + 100);
    add_at(&mut s, &c, NOW).expect("c");

    let q = |text: &str| Query {
        text: text.into(),
        limit: 10,
        ..Query::default()
    };
    let ids = |hits: Vec<elo_ctx::recall::Hit>| hits.into_iter().map(|h| h.id).collect::<Vec<_>>();
    let active = recall(&s, &q("decisao"), NOW).expect("recall");
    assert_eq!(ids(active), ["d-new"], "accent folded, superseded hidden");
    let history = recall(
        &s,
        &Query {
            history: true,
            ..q("decisao")
        },
        NOW,
    )
    .expect("history");
    assert_eq!(history.len(), 2);
    assert!(history.iter().any(|h| h.status == "superseded"));
    // effective expiry on read, before any write sweeps it
    assert_eq!(ids(recall(&s, &q("navegador"), NOW).expect("r")), ["n-web"]);
    assert!(recall(&s, &q("navegador"), NOW + 200)
        .expect("r")
        .is_empty());
    let expired = recall(
        &s,
        &Query {
            history: true,
            ..q("navegador")
        },
        NOW + 200,
    )
    .expect("r");
    assert_eq!(expired[0].status, "expired");
    let scoped = Query {
        scope: Some("project:web".into()),
        ..q("cache")
    };
    assert_eq!(ids(recall(&s, &scoped, NOW).expect("r")), ["n-web"]);
    let kind = Query {
        kind: Some("decision".into()),
        ..q("cache")
    };
    assert_eq!(ids(recall(&s, &kind, NOW).expect("r")), ["d-new"]);
    assert!(recall(&s, &q("\"*"), NOW).is_err(), "no searchable words");
    assert!(
        recall(&s, &q("AND OR NOT \"x"), NOW).is_ok(),
        "operators are quoted"
    );
}

#[test]
fn hook_never_delivers_superseded_expired_or_archived() {
    let dir = TempDir::new().expect("tmp");
    fs::create_dir_all(dir.path().join("api")).expect("mkdir");
    fs::write(dir.path().join("api/package.json"), "{}").expect("manifest");
    let mut s = Store::create(dir.path()).expect("open");
    let now = chrono::Utc::now().timestamp();
    for (id, word) in [
        ("keep", "zebraalfa"),
        ("sup", "zebrabeta"),
        ("exp", "zebragama"),
        ("arq", "zebradelta"),
    ] {
        let mut e = entry("note", "global", id, &format!("{word} always relevant"));
        e.id = Some(id.into());
        e.triggers = vec![word.into()];
        e.importance = 1.0;
        add_at(&mut s, &e, now).expect("add");
    }
    let mut next = entry("note", "global", "next", "substituta zebrabeta");
    next.id = Some("next".into());
    next.triggers = vec!["zebrabeta".into()];
    next.supersedes = Some("sup".into());
    add_at(&mut s, &next, now).expect("next");
    forget_at(&mut s, "arq", now).expect("forget");
    // expiry in the past, still stored as active: the hook must apply it on read
    s.conn
        .execute(
            "UPDATE items SET expires_at = ?1 WHERE id = 'exp'",
            [now - 5],
        )
        .expect("expire");
    drop(s);

    let input = serde_json::json!({
        "session_id": "s1",
        "cwd": dir.path().join("api"),
        "prompt": "zebraalfa zebrabeta zebragama zebradelta"
    })
    .to_string();
    let out = prepare("user-prompt-submit", &input)
        .expect("hook")
        .unwrap_or_default();
    assert!(out.contains("keep"), "{out}");
    assert!(out.contains("next"), "{out}");
    for gone in ["sup", "exp", "arq"] {
        assert!(
            !out.contains(&format!("{gone}:")) && !out.contains(&format!(" {gone} ")),
            "{out}"
        );
    }
    assert!(
        !out.contains("zebragama") && !out.contains("zebradelta"),
        "{out}"
    );
}
