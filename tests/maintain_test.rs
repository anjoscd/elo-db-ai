use std::process::Command;

use elo_ctx::maintain::{apply, plan};
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
    let store = Store::open(dir.path()).unwrap();
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
    let mut store = Store::open(dir.path()).unwrap();
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
