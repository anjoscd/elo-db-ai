use std::process::Command;

use elo_ctx::maintain::{
    apply, apply_proposals, list_proposals, maybe_invite, plan, propose, ProposalOp,
};
use elo_ctx::store::{now_seconds, NewItem, Store};
use tempfile::TempDir;

fn add(store: &Store, id: &str, kind: &str, body: &str, updated: i64) {
    store
        .insert_item(&NewItem {
            id,
            kind,
            scope: "global",
            title: id,
            body,
            origin: "user",
        })
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE items SET updated_at=?1 WHERE id=?2",
            rusqlite::params![updated, id],
        )
        .unwrap();
}

fn status(store: &Store, id: &str) -> (String, Option<String>) {
    store
        .conn
        .query_row(
            "SELECT status, superseded_by FROM items WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn workspace() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::create(dir.path()).unwrap();
    add(
        &store,
        "D-velha",
        "decision",
        "Usar o  Revisor para revisar",
        100,
    );
    add(
        &store,
        "D-nova",
        "decision",
        "usar o revisor para revisar",
        200,
    );
    add(&store, "L1", "law", "Nunca publicar sem ordem", 100);
    add(&store, "L2", "law", "Nunca publicar sem ordem", 200);
    add(&store, "N-vence", "note", "nota com prazo", 100);
    add(&store, "N-unica", "note", "nota única", 100);
    store
        .conn
        .execute(
            "UPDATE items SET expires_at=?1 WHERE id='N-vence'",
            [now_seconds() - 10],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO auto_usage(session_id,event_seq,item_id,kind,scope,at) VALUES ('s',1,'sumiu','note','global',?1)",
            [now_seconds()],
        )
        .unwrap();
    (dir, store)
}

#[test]
fn plan_proposes_without_changing_anything() {
    let (dir, store) = workspace();
    let p = plan(&store, now_seconds()).unwrap();
    assert_eq!(
        p.duplicates,
        vec![("D-velha".to_string(), "D-nova".to_string())]
    );
    assert_eq!(
        p.duplicated_laws,
        vec![vec!["L2".to_string(), "L1".to_string()]]
    );
    assert_eq!(p.expired, vec!["N-vence".to_string()]);
    assert_eq!(p.orphan_usage, 1);
    drop(store);
    // The CLI without --aplicar leaves the store byte for byte.
    let db = dir.path().join(".elo/elo.db");
    let before = std::fs::read(&db).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(["maintain"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("D-velha → superseded by D-nova"), "{text}");
    assert!(text.contains("duplicated laws (decide by hand"), "{text}");
    assert!(text.contains("nothing was changed"), "{text}");
    assert_eq!(std::fs::read(&db).unwrap(), before);
}

#[test]
fn maintain_apply_english_flag_and_old_alias_work() {
    for flag in ["--apply", "--aplicar"] {
        let (dir, _store) = workspace();
        let out = Command::new(env!("CARGO_BIN_EXE_elo"))
            .args(["maintain", flag])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{flag}: {text}");
        assert!(text.contains("APPLIED:"), "{flag}: {text}");
    }
}

#[test]
fn apply_supersedes_expires_cleans_and_backs_up_but_never_touches_laws() {
    let (_dir, mut store) = workspace();
    let now = now_seconds();
    let p = plan(&store, now).unwrap();
    let applied = apply(&mut store, &p, now).unwrap();
    assert!(applied.backup.is_file(), "backup {:?}", applied.backup);
    assert_eq!(applied.superseded, 1);
    assert_eq!(
        status(&store, "D-velha"),
        ("superseded".into(), Some("D-nova".into()))
    );
    assert_eq!(status(&store, "D-nova").0, "active");
    assert_eq!(status(&store, "L1").0, "active", "laws are only reported");
    assert_eq!(status(&store, "L2").0, "active");
    assert_eq!(status(&store, "N-vence").0, "expired");
    assert_eq!(status(&store, "N-unica").0, "active");
    let orphans: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM auto_usage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(orphans, 0);
    // A second pass has nothing left to do.
    let again = plan(&store, now).unwrap();
    assert!(again.duplicates.is_empty() && again.expired.is_empty() && again.orphan_usage == 0);
}

#[test]
fn similar_notes_are_suggested_but_never_applied() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::create(dir.path()).unwrap();
    add(
        &store,
        "N-v2",
        "note",
        "filtro v2 no ar: main ea8ccb0 push github binario instalado local",
        100,
    );
    add(
        &store,
        "N-v4",
        "note",
        "filtro v4 no ar: main 406ea01 push github binario instalado local",
        200,
    );
    add(
        &store,
        "N-outra",
        "note",
        "assunto completamente diferente sobre banco e migracao",
        150,
    );
    let now = now_seconds();
    let p = plan(&store, now).unwrap();
    assert_eq!(p.similar.len(), 1, "{:?}", p.similar);
    assert_eq!(
        (p.similar[0].0.as_str(), p.similar[0].1.as_str()),
        ("N-v2", "N-v4")
    );
    apply(&mut store, &p, now).unwrap();
    assert_eq!(
        status(&store, "N-v2").0,
        "active",
        "suggestions are never applied"
    );
}

fn propose_workspace() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.path().join("src/a.ts"), "export const a = 1;\n").unwrap();
    std::fs::create_dir(dir.path().join("api")).unwrap();
    std::fs::write(dir.path().join("api/package.json"), "{}\n").unwrap();
    let store = Store::create(dir.path()).unwrap();
    for (id, kind, body) in [
        ("r1", "rule", "first rule"),
        ("r2", "rule", "second rule"),
        ("r3", "rule", "third rule"),
        ("r4", "rule", "fourth rule"),
        ("n1", "note", "a note"),
        ("n2", "note", "second note"),
        ("l1", "law", "law one"),
        ("l2", "law", "law two"),
    ] {
        add(&store, id, kind, body, 100);
    }
    store
        .conn
        .execute(
            "UPDATE items SET triggers=?1 WHERE id IN ('r1','l1')",
            [serde_json::json!({"always":true}).to_string()],
        )
        .unwrap();
    (dir, store)
}

fn triggers(store: &Store, id: &str) -> serde_json::Value {
    serde_json::from_str(
        &store
            .conn
            .query_row("SELECT triggers FROM items WHERE id=?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn proposals_are_stored_not_applied_and_laws_need_an_explicit_id() {
    let (dir, mut store) = propose_workspace();
    let ops = vec![
        ProposalOp::SetPaths {
            id: "r1".into(),
            globs: vec!["../src/a.rs".into()],
        },
        ProposalOp::SetPaths {
            id: "r2".into(),
            globs: vec!["/src/a.rs".into()],
        },
        ProposalOp::SetPaths {
            id: "r3".into(),
            globs: vec!["nothing/*".into()],
        },
        ProposalOp::SetPaths {
            id: "r1".into(),
            globs: vec!["src/a.*".into()],
        },
        ProposalOp::Merge {
            from: "l1".into(),
            into: "l2".into(),
        },
        ProposalOp::SetScope {
            id: "r4".into(),
            scope: "project:web".into(),
        },
        ProposalOp::SetScope {
            id: "r4".into(),
            scope: "project:api".into(),
        },
        ProposalOp::DropAlways { id: "r1".into() },
        ProposalOp::DropAlways { id: "l1".into() },
        ProposalOp::Merge {
            from: "r3".into(),
            into: "n2".into(),
        },
        ProposalOp::Merge {
            from: "n1".into(),
            into: "n2".into(),
        },
    ];
    let (accepted, rejected) = propose(dir.path(), &store, ops).unwrap();
    assert_eq!(accepted, vec!["r1", "r4", "r1", "l1", "n1"]);
    assert_eq!(rejected.len(), 6);
    let rejections = rejected
        .iter()
        .map(|(id, reason)| (id.as_str(), reason.as_str()))
        .collect::<Vec<_>>();
    for (id, expected) in [
        ("r1", "cannot contain '..'"),
        ("r2", "relative"),
        ("r3", "did not match any existing file"),
        ("r3", "merge requires the same kind"),
        ("l1", "laws cannot be merged automatically"),
        ("r4", "not used by any item or discovered project"),
    ] {
        assert!(
            rejections
                .iter()
                .any(|(item, reason)| *item == id && reason.contains(expected)),
            "missing rejection ({id}, {expected}): {rejections:?}"
        );
    }
    // Nothing is applied before the review: triggers, scope and status are untouched.
    assert_eq!(triggers(&store, "r1"), serde_json::json!({"always":true}));
    let scope: String = store
        .conn
        .query_row("SELECT scope FROM items WHERE id='r4'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(scope, "global");
    assert_eq!(status(&store, "r2"), ("active".into(), None));
    assert_eq!(status(&store, "n1"), ("active".into(), None));
    let pending = list_proposals(&store).unwrap();
    assert_eq!(pending.len(), 5);
    assert!(pending.iter().all(|p| p.id >= 1));

    let now = now_seconds();
    let law_id = pending
        .iter()
        .find(|p| p.item_id.as_deref() == Some("l1"))
        .unwrap()
        .id;
    let applied = apply_proposals(&mut store, &[], true, now).unwrap();
    assert_eq!(applied.applied, vec![1, 2, 3, 5]);
    assert!(applied.skipped.is_empty());
    // Non-law proposals are in effect.
    let t = triggers(&store, "r1");
    assert_eq!(t["paths"], serde_json::json!(["src/a.*"]));
    assert_eq!(t["always"], serde_json::json!(false));
    let evidence: String = store
        .conn
        .query_row("SELECT evidence FROM items WHERE id='r1'", [], |r| r.get(0))
        .unwrap();
    assert!(
        evidence.contains("set_paths applied from agent proposal"),
        "{evidence}"
    );
    assert!(
        evidence.contains("drop_always applied from agent proposal"),
        "{evidence}"
    );
    let scope: String = store
        .conn
        .query_row("SELECT scope FROM items WHERE id='r4'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(scope, "project:api");
    assert_eq!(
        status(&store, "n1"),
        ("superseded".into(), Some("n2".into()))
    );
    assert_eq!(status(&store, "n2").0, "active");
    let note_evidence: String = store
        .conn
        .query_row("SELECT evidence FROM items WHERE id='n1'", [], |r| r.get(0))
        .unwrap();
    assert!(
        note_evidence.contains("merge applied from agent proposal"),
        "{note_evidence}"
    );
    // The law stayed where it was until somebody named its proposal.
    assert_eq!(triggers(&store, "l1"), serde_json::json!({"always":true}));
    let pending = list_proposals(&store).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, law_id);
    let applied = apply_proposals(&mut store, &[law_id], false, now).unwrap();
    assert_eq!(applied.applied, vec![law_id]);
    assert_eq!(triggers(&store, "l1")["always"], serde_json::json!(false));
    let law_evidence: String = store
        .conn
        .query_row("SELECT evidence FROM items WHERE id='l1'", [], |r| r.get(0))
        .unwrap();
    assert!(
        law_evidence.contains("applied from agent proposal"),
        "{law_evidence}"
    );
    assert!(list_proposals(&store).unwrap().is_empty());
}

#[test]
fn maintain_review_cli_lists_and_applies() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/x.rs"), "fn x() {}\n").unwrap();
    {
        let store = Store::create(dir.path()).unwrap();
        add(&store, "k1", "rule", "keep me", 100);
        add(&store, "m1", "law", "muted law", 100);
        store
            .conn
            .execute(
                "UPDATE items SET triggers=?1 WHERE id IN ('k1','m1')",
                [serde_json::json!({"always":true}).to_string()],
            )
            .unwrap();
        let (accepted, rejected) = propose(
            dir.path(),
            &store,
            vec![
                ProposalOp::SetPaths {
                    id: "k1".into(),
                    globs: vec!["src/x.rs".into()],
                },
                ProposalOp::DropAlways { id: "k1".into() },
                ProposalOp::DropAlways { id: "m1".into() },
            ],
        )
        .unwrap();
        assert_eq!(accepted, vec!["k1", "k1", "m1"]);
        assert!(rejected.is_empty());
        drop(store);
    }
    let review = |args: &[&str]| -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_elo"))
            .arg("maintain")
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(out.status.success(), "{text}");
        text
    };
    let text = review(&["review"]);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines[0].starts_with("1 set_paths k1 "), "{}", lines[0]);
    assert!(lines[0].contains("src/x.rs"));
    assert_eq!(lines[1].trim_end(), "2 drop_always k1 {}");
    assert_eq!(lines[2].trim_end(), "3 drop_always m1 {}");
    let text = review(&["review", "--apply-nonlaw"]);
    assert!(text.contains("applied: 2"), "{text}");
    assert_eq!(review(&["review"]).trim_end(), "3 drop_always m1 {}");
    let text = review(&["review", "--apply", "3"]);
    assert!(text.contains("applied: 1"), "{text}");
    assert_eq!(review(&["review"]).trim_end(), "no pending proposals");

    // An unknown or already applied id is reported, never dropped in silence.
    let text = review(&["review", "--apply", "999"]);
    assert!(text.contains("applied: 0"), "{text}");
    assert!(text.contains("999: not pending"), "{text}");
    let text = review(&["review", "--apply", "3"]);
    assert!(text.contains("3: not pending"), "{text}");
    // A typo is an error: nothing is applied and the exit status says so.
    let out = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(["maintain", "review", "--apply", "x2"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid proposal id"));
}

#[test]
fn idle_cycle_invites_only_after_five_sessions_and_a_day() {
    let dir = TempDir::new().unwrap();
    let store = Store::create(dir.path()).unwrap();
    let t0 = 1_700_000_000i64;
    for i in 0..5 {
        assert_eq!(
            maybe_invite(&store, t0 + i).unwrap(),
            None,
            "call {} must not invite yet",
            i + 1
        );
    }
    let invite = maybe_invite(&store, t0 + 5)
        .unwrap()
        .expect("sixth session start after an idle day");
    assert!(invite.contains("elo_maintain_catalog"), "{}", invite);
    assert!(invite.contains("elo_maintain_propose"), "{}", invite);
    // The counters are persisted and reset.
    assert_eq!(
        store
            .meta_get("maintain_sessions_since")
            .unwrap()
            .as_deref(),
        Some("1")
    );
    let last_cycle = store
        .meta_get("maintain_last_cycle_at")
        .unwrap()
        .expect("last cycle recorded");
    assert_eq!(last_cycle, (t0 + 5).to_string());
    // A day has not passed since the cycle: no invite while the counter climbs.
    assert_eq!(maybe_invite(&store, t0 + 6).unwrap(), None);
    let base = t0 + 90_000;
    for i in 0..3 {
        assert_eq!(
            maybe_invite(&store, base + i).unwrap(),
            None,
            "call {}",
            i + 2
        );
    }
    assert!(maybe_invite(&store, base + 3).unwrap().is_some());
    // The meta state survives a reopen.
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store
            .meta_get("maintain_sessions_since")
            .unwrap()
            .as_deref(),
        Some("1")
    );
    assert_eq!(
        maybe_invite(&store, base + 4).unwrap(),
        None,
        "day not over yet"
    );
    assert_eq!(
        store
            .meta_get("maintain_sessions_since")
            .unwrap()
            .as_deref(),
        Some("2")
    );
    // The two gates stand alone: five sessions are useless without the day,
    // the day is useless without five sessions.
    let dir2 = TempDir::new().unwrap();
    let store2 = Store::create(dir2.path()).unwrap();
    store2.meta_set("maintain_sessions_since", "9").unwrap();
    store2
        .meta_set("maintain_last_cycle_at", &(base - 86_401).to_string())
        .unwrap();
    assert!(maybe_invite(&store2, base).unwrap().is_some());
    store2
        .meta_set("maintain_last_cycle_at", &(base - 10).to_string())
        .unwrap();
    assert_eq!(maybe_invite(&store2, base).unwrap(), None);
}

#[cfg(unix)]
#[test]
fn glob_rejects_symlinks_escaping_root_and_bare_directories() {
    let (dir, store) = propose_workspace();
    let outside = TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.rs"), "fn s() {}\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("escape_dir")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.rs"),
        dir.path().join("src/escape_file.rs"),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("empty_dir")).unwrap();

    let ops = vec![
        ProposalOp::SetPaths {
            id: "r1".into(),
            globs: vec!["escape_dir/*".into()],
        },
        ProposalOp::SetPaths {
            id: "r2".into(),
            globs: vec!["src/escape_file.rs".into()],
        },
        ProposalOp::SetPaths {
            id: "r3".into(),
            globs: vec!["src".into()],
        },
        ProposalOp::SetPaths {
            id: "r4".into(),
            globs: vec!["empty_dir".into()],
        },
    ];
    let (accepted, rejected) = propose(dir.path(), &store, ops).unwrap();
    assert!(accepted.is_empty(), "{accepted:?}");
    assert_eq!(rejected.len(), 4, "{rejected:?}");
}

#[cfg(unix)]
#[test]
fn glob_validation_follows_the_delivery_prefix_semantics() {
    let (dir, mut store) = propose_workspace();
    // `pkg` holds only subdirectories; the real files live deeper inside.
    std::fs::create_dir_all(dir.path().join("pkg/deep")).unwrap();
    std::fs::write(dir.path().join("pkg/deep/file.rs"), "fn f() {}\n").unwrap();
    std::fs::create_dir(dir.path().join("pkg/empty_sub")).unwrap();
    // A symlinked directory whose only files live outside the root.
    let outside = TempDir::new().unwrap();
    std::fs::write(outside.path().join("x.rs"), "fn x() {}\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("pkg/linked")).unwrap();
    // Build output is pruned from the prefix search.
    std::fs::create_dir_all(dir.path().join("target/build")).unwrap();
    std::fs::write(dir.path().join("target/build/out.rs"), "fn o() {}\n").unwrap();
    std::fs::create_dir(dir.path().join("lonely")).unwrap();

    let ops = vec![
        ProposalOp::SetPaths {
            id: "r1".into(),
            globs: vec!["pkg/*".into()],
        },
        ProposalOp::SetPaths {
            id: "r1".into(),
            globs: vec!["pkg/deep/file.rs".into()],
        },
        ProposalOp::SetPaths {
            id: "r1".into(),
            globs: vec!["pkg/**".into()],
        },
        ProposalOp::SetPaths {
            id: "r1".into(),
            globs: vec!["pkg/deep/**".into()],
        },
        ProposalOp::SetPaths {
            id: "r1".into(),
            globs: vec!["pkg/*/x".into()],
        },
        ProposalOp::SetPaths {
            id: "r1".into(),
            globs: vec!["lonely/*".into()],
        },
        ProposalOp::SetPaths {
            id: "r1".into(),
            globs: vec!["pkg/linked/*".into()],
        },
        ProposalOp::SetPaths {
            id: "r1".into(),
            globs: vec!["target/*".into()],
        },
    ];
    let (accepted, rejected) = propose(dir.path(), &store, ops).unwrap();
    // `pkg/*` counts files inside subdirectories; the exact path works too.
    assert_eq!(accepted, vec!["r1", "r1"], "{accepted:?}");
    assert_eq!(rejected.len(), 6, "{rejected:?}");
    for (_, reason) in &rejected[..3] {
        assert!(
            reason == "only a trailing * (prefix) is supported, e.g. dir/*",
            "{rejected:?}"
        );
    }
    for (_, reason) in &rejected[3..] {
        assert!(
            reason.contains("did not match any existing file"),
            "{rejected:?}"
        );
    }

    // Review-time revalidation applies the same rule: once the only matching
    // file goes away, the pending set_paths proposals are skipped, not applied.
    std::fs::remove_file(dir.path().join("pkg/deep/file.rs")).unwrap();
    let applied = apply_proposals(&mut store, &[], true, now_seconds()).unwrap();
    assert!(applied.applied.is_empty(), "{applied:?}");
    assert_eq!(applied.skipped.len(), 2, "{applied:?}");
    for (_, reason) in &applied.skipped {
        assert!(
            reason.contains("did not match any existing file"),
            "{applied:?}"
        );
    }
}

#[test]
fn catalog_truncates_utf8_titles_by_char_and_aggregates_counts() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::create(dir.path()).unwrap();
    let long_euro = "€".repeat(100);
    let long_emoji = "🦀✨".repeat(50);
    for (id, title) in [("e1", long_euro.as_str()), ("e2", long_emoji.as_str())] {
        store
            .insert_item(&NewItem {
                id,
                kind: "rule",
                scope: "global",
                title,
                body: "body",
                origin: "user",
            })
            .unwrap();
    }
    store
        .record_injection(
            "s1",
            1,
            20,
            &[
                ("e1", elo_ctx::stats::Mode::Full),
                ("e2", elo_ctx::stats::Mode::Pointer),
            ],
        )
        .unwrap();
    store
        .record_injection("s1", 2, 20, &[("e1", elo_ctx::stats::Mode::Full)])
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO auto_usage (session_id, event_seq, item_id, kind, scope, at) \
             VALUES ('s1', 1, 'e1', 'rule', 'global', 10)",
            [],
        )
        .unwrap();
    let json_str = elo_ctx::maintain::catalog(&store, 0).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let items = parsed["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    for item in items {
        let title = item["title"].as_str().unwrap();
        assert_eq!(title.chars().count(), 80);
        if item["id"] == "e1" {
            assert_eq!(item["delivered"], 2);
            assert_eq!(item["used"], 1);
        } else {
            assert_eq!(item["delivered"], 1);
            assert_eq!(item["used"], 0);
        }
    }
}

#[test]
fn merge_into_self_is_rejected() {
    let (dir, store) = propose_workspace();
    let (accepted, rejected) = propose(
        dir.path(),
        &store,
        vec![ProposalOp::Merge {
            from: "n1".into(),
            into: "n1".into(),
        }],
    )
    .unwrap();
    assert!(accepted.is_empty());
    assert_eq!(rejected.len(), 1);
    assert!(
        rejected[0].1.contains("itself"),
        "unexpected reason: {:?}",
        rejected[0]
    );
}

#[test]
fn apply_one_is_atomic_when_proposal_update_fails() {
    let (dir, mut store) = propose_workspace();
    let (accepted, rejected) = propose(
        dir.path(),
        &store,
        vec![ProposalOp::DropAlways { id: "r1".into() }],
    )
    .unwrap();
    assert_eq!(accepted, vec!["r1"]);
    assert!(rejected.is_empty());

    // Force the final UPDATE on proposals to abort after item/evidence updates.
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER fail_proposals_update BEFORE UPDATE ON proposals \
             BEGIN SELECT RAISE(ABORT, 'forced proposal failure'); END;",
        )
        .unwrap();

    let res = apply_proposals(&mut store, &[], true, now_seconds()).unwrap();
    assert!(res.applied.is_empty());
    assert_eq!(res.skipped.len(), 1);
    // Both triggers and evidence on r1 must have been rolled back.
    assert_eq!(triggers(&store, "r1"), serde_json::json!({"always":true}));
    let evidence: Option<String> = store
        .conn
        .query_row("SELECT evidence FROM items WHERE id='r1'", [], |r| r.get(0))
        .unwrap();
    assert!(evidence.is_none(), "{evidence:?}");
    assert_eq!(list_proposals(&store).unwrap().len(), 1);
}

#[test]
fn review_revalidates_proposals_against_current_files_scopes_and_status() {
    let (dir, mut store) = propose_workspace();
    let (accepted, rejected) = propose(
        dir.path(),
        &store,
        vec![
            ProposalOp::SetPaths {
                id: "r1".into(),
                globs: vec!["src/a.rs".into()],
            },
            ProposalOp::SetScope {
                id: "r2".into(),
                scope: "project:api".into(),
            },
            ProposalOp::DropAlways { id: "r3".into() },
            ProposalOp::Merge {
                from: "n1".into(),
                into: "n2".into(),
            },
        ],
    )
    .unwrap();
    assert_eq!(accepted.len(), 4);
    assert!(rejected.is_empty());

    // Invalidate all four conditions before review:
    // 1. Remove the matched file src/a.rs
    std::fs::remove_file(dir.path().join("src/a.rs")).unwrap();
    // 2. Remove the discovered project directory api/
    std::fs::remove_dir_all(dir.path().join("api")).unwrap();
    // 3. Expire r3
    let now = now_seconds();
    store
        .conn
        .execute(
            "UPDATE items SET expires_at = ?1 WHERE id = 'r3'",
            [now - 5],
        )
        .unwrap();
    // 4. Mark n2 as superseded/inactive
    store
        .conn
        .execute("UPDATE items SET status = 'expired' WHERE id = 'n2'", [])
        .unwrap();

    let out = apply_proposals(&mut store, &[], true, now).unwrap();
    assert!(out.applied.is_empty(), "{:?}", out.applied);
    assert_eq!(out.skipped.len(), 4, "{:?}", out.skipped);
    assert_eq!(triggers(&store, "r1"), serde_json::json!({"always":true}));
    let scope_r2: String = store
        .conn
        .query_row("SELECT scope FROM items WHERE id='r2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(scope_r2, "global");
    assert_eq!(status(&store, "n1"), ("active".into(), None));
}
