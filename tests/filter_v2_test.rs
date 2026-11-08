use elo_ctx::{
    bench::{
        elo_selector::E1,
        fixture::{Event, Item, Kind, Origin},
        selector::{render, Selector, SessionState},
    },
    config::Config,
    live::live_fixture,
};

fn item() -> Item {
    serde_json::from_value(serde_json::json!({
        "id":"r", "kind":"rule", "scope":"global", "title":"Título visível",
        "body":"x".repeat(301), "triggers":{"always":true,"keywords":["foco"],"paths":["src/*"]},
        "importance":1.0, "created_at":"2026-01-01T00:00:00Z", "updated_at":"2026-01-01T00:00:00Z",
        "status":"active", "origin":"agent"
    }))
    .unwrap()
}
fn event() -> Event {
    serde_json::from_value(serde_json::json!({
        "id":"s:1", "session_id":"s", "event_seq":1, "event_type":"user-prompt-submit", "family":"teste",
        "cwd":".", "now":"2026-01-01T00:00:00Z", "allowed_scopes":[], "required":[], "forbidden":[]
    })).unwrap()
}
#[test]
fn indice_respeita_bytes_foco_limite_e_leis() {
    let mut fx = live_fixture(std::path::Path::new("."), Config::default(), vec![item()]);
    let mut ev = event();
    let select = |fx: &_, ev: &_| E1.select(fx, ev, &mut SessionState::default());
    let s = select(&fx, &ev);
    assert_eq!(s.pointer_ids, ["r"]);
    assert!(render(&fx, &ev, &s).contains("Título visível"));
    assert!(!render(&fx, &ev, &s).contains(&"x".repeat(301)));
    ev.prompt = Some("foco".into());
    assert_eq!(select(&fx, &ev).full_ids, ["r"]);
    ev.prompt = None;
    let mut state = SessionState::default();
    state.touched.push(("src/a.rs".into(), 1));
    assert_eq!(E1.select(&fx, &ev, &mut state).full_ids, ["r"]);
    fx.items[0].body = "é".repeat(150);
    assert_eq!(select(&fx, &ev).full_ids, ["r"]);
    fx.items[0].body.push('é');
    assert_eq!(select(&fx, &ev).pointer_ids, ["r"]);
    fx.config.workspace.index_over_bytes = 0;
    assert_eq!(select(&fx, &ev).full_ids, ["r"]);
    fx.config.workspace.index_over_bytes = 300;
    fx.items[0].kind = Kind::Law;
    let s = E1.select(&fx, &ev, &mut state);
    assert_eq!(s.full_ids, ["r"]);
    state.record(&s);
    assert_eq!(E1.select(&fx, &ev, &mut state).pointer_ids, ["r"]);
}
#[test]
fn placar_rebaixa_sem_mutar_e_preserva_lei_do_usuario() {
    let mut fx = live_fixture(std::path::Path::new("."), Config::default(), vec![item()]);
    fx.config.workspace.min_score = 0.0;
    fx.config.workspace.index_over_bytes = 0;
    let mut ev = event();
    for (bad, good, expected) in [(0, 0, false), (2, 0, false), (3, 0, true), (4, 1, false)] {
        fx.items[0].feedback.bad = bad;
        fx.items[0].feedback.good = good;
        assert_eq!(
            E1.select(&fx, &ev, &mut SessionState::default())
                .full_ids
                .is_empty(),
            expected
        );
    }
    fx.items[0].feedback.good = 0;
    ev.prompt = Some("foco".into());
    assert_eq!(
        E1.select(&fx, &ev, &mut SessionState::default()).full_ids,
        ["r"]
    );
    ev.prompt = None;
    fx.config.workspace.demote_bad_min = 0;
    assert_eq!(
        E1.select(&fx, &ev, &mut SessionState::default()).full_ids,
        ["r"]
    );
    fx.config.workspace.demote_bad_min = 3;
    fx.items[0].kind = Kind::Law;
    assert!(E1
        .select(&fx, &ev, &mut SessionState::default())
        .full_ids
        .is_empty());
    fx.items[0].origin = Origin::User;
    assert_eq!(
        E1.select(&fx, &ev, &mut SessionState::default()).full_ids,
        ["r"]
    );
    assert!(fx.items[0].triggers.always);
}
#[test]
fn replay_agrega_sem_expor_conteudo_e_isola_sessoes() {
    let mut i = item();
    i.kind = Kind::Law;
    let fx = live_fixture(std::path::Path::new("."), Config::default(), vec![i]);
    let mut lines = String::from("pedido secreto inválido\n");
    for (session, seq) in [("s", 1), ("t", 1), ("s", 2)] {
        lines.push_str(&serde_json::json!({"at":"2026-01-01T00:00:00Z","cwd":".","event_seq":seq,"session_id":session}).to_string());
        lines.push('\n');
    }
    let result = elo_ctx::bench::replay::aggregate(&fx, lines.as_bytes(), &[]).unwrap();
    assert!(result.contains("events 3\n"));
    assert!(result.contains("(mean) 0.67"));
    assert!(result.contains("events without request 3"));
    assert!(result.contains("invalid lines 1"));
    assert!(!result.contains("secreto"));
    assert!(!result.contains("Título"));
    let empty = elo_ctx::bench::replay::aggregate(&fx, "".as_bytes(), &[]).unwrap();
    assert!(empty.contains("events 0\n"));
}
#[test]
fn configuracao_limites_e_raizes_invalidas() {
    let c = Config::parse(
        "[workspace]\nindex_over_bytes=0\ndemote_bad_min=7",
        std::path::Path::new("teste"),
    )
    .unwrap();
    assert_eq!(c.workspace.index_over_bytes, 0);
    assert_eq!(c.workspace.demote_bad_min, 7);
    assert!(Config::parse(
        "[[project]]\nname='a'\nroot=''",
        std::path::Path::new("teste")
    )
    .is_err());
    assert!(Config::parse(
        "[[project]]\nname='a'\nroot='a'\n[[project]]\nname='b'\nroot='a/b'",
        std::path::Path::new("teste")
    )
    .is_ok());
}

#[test]
fn store_stats_explain_e_replay_usam_o_placar_real() {
    let dir = tempfile::tempdir().unwrap();
    elo_ctx::init::init(dir.path()).unwrap();
    let store = elo_ctx::store::Store::open(dir.path()).unwrap();
    store
        .insert_item(&elo_ctx::store::NewItem {
            id: "r",
            kind: "rule",
            scope: "global",
            title: "Privado",
            body: "Corpo privado",
            origin: "agent",
        })
        .unwrap();
    rusqlite::Connection::open(store.path())
        .unwrap()
        .execute(
            "UPDATE items SET triggers = '{\"always\":true,\"keywords\":[],\"paths\":[]}'",
            [],
        )
        .unwrap();
    for _ in 0..3 {
        store.record_feedback("r", false).unwrap();
    }
    let items = elo_ctx::live::load_items(&store).unwrap();
    assert_eq!(items[0].feedback.bad, 3);
    assert!(elo_ctx::stats::demoted(&items[0], 3));
    let cli = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_elo"))
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(cli(&["stats"]).contains("demoted            1"));
    assert!(cli(&["explain", "r"]).contains("demoted: yes"));
    std::fs::create_dir_all(dir.path().join(".elo/shadow")).unwrap();
    std::fs::write(dir.path().join(".elo/shadow/log.jsonl"),
        "{\"at\":\"2026-01-01T00:00:00Z\",\"cwd\":\".\",\"session_id\":\"s\",\"event_seq\":1}\ninválida\n").unwrap();
    let out = cli(&["bench", "--replay", ".elo/shadow/log.jsonl"]);
    assert!(out.contains("events with 0 items 1"));
    assert!(out.contains("invalid lines 1"));
    assert!(!out.contains("Privado"));
    assert_eq!(
        store
            .stats(None, elo_ctx::stats::GroupBy::Kind)
            .unwrap()
            .events,
        0
    );
    assert!(!dir.path().join(".elo/sessions").exists());
    store.record_feedback("r", true).unwrap();
    assert!(cli(&["explain", "r"]).contains("demoted: no"));
}

#[test]
fn leitura_aceita_schema_antigo_sem_feedback_e_nao_migra() {
    let dir = tempfile::tempdir().unwrap();
    elo_ctx::init::init(dir.path()).unwrap();
    let path = dir.path().join(".elo/elo.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE feedback; UPDATE meta SET value='1' WHERE key='schema_version';",
    )
    .unwrap();
    drop(conn);
    let before = std::fs::read(&path).unwrap();
    let store = elo_ctx::store::Store::open_read_only(dir.path()).unwrap();
    assert_eq!(store.schema_version().unwrap(), 1);
    assert!(store.feedback_totals().unwrap().is_empty());
    assert!(elo_ctx::live::load_items(&store).unwrap().is_empty());
    assert_eq!(store.demoted_count(3).unwrap(), 0);
    assert!(store
        .insert_item(&elo_ctx::store::NewItem {
            id: "x",
            kind: "rule",
            scope: "global",
            title: "x",
            body: "x",
            origin: "agent"
        })
        .is_err());
    drop(store);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn stats_tolera_item_malformado() {
    let dir = tempfile::tempdir().unwrap();
    elo_ctx::init::init(dir.path()).unwrap();
    let store = elo_ctx::store::Store::open(dir.path()).unwrap();
    store
        .insert_item(&elo_ctx::store::NewItem {
            id: "x",
            kind: "rule",
            scope: "global",
            title: "x",
            body: "x",
            origin: "agent",
        })
        .unwrap();
    let conn = rusqlite::Connection::open(store.path()).unwrap();
    conn.execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE items SET kind='invalid', triggers='invalid', importance='invalid';").unwrap();
    assert!(elo_ctx::live::load_items(&store).is_err());
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_elo"))
        .current_dir(dir.path())
        .arg("stats")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("demoted            0"));
}

#[test]
fn leitura_wal_enxerga_commit_com_escritor_ativo() {
    let dir = tempfile::tempdir().unwrap();
    elo_ctx::init::init(dir.path()).unwrap();
    let path = dir.path().join(".elo/elo.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    let writer = elo_ctx::store::Store::open(dir.path()).unwrap();
    writer
        .insert_item(&elo_ctx::store::NewItem {
            id: "wal",
            kind: "rule",
            scope: "global",
            title: "wal",
            body: "wal",
            origin: "agent",
        })
        .unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let store = elo_ctx::store::Store::open_read_only(dir.path()).unwrap();
    assert_eq!(elo_ctx::live::load_items(&store).unwrap()[0].id, "wal");
    conn.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn escopo_da_pasta_mae_limita_projetos_e_preserva_sinais_explicitos() {
    use elo_ctx::config::Project;
    for count in 1..=3 {
        let config = Config {
            projects: (0..count)
                .map(|n| Project {
                    name: format!("app{n}"),
                    root: format!("pai/app{n}").into(),
                    aliases: vec![],
                })
                .collect(),
            ..Config::default()
        };
        let items = (0..count)
            .map(|n| {
                let mut i = item();
                i.id = format!("r{n}");
                i.kind = Kind::Law;
                i.scope = format!("project:app{n}");
                i
            })
            .collect();
        let mut fx = live_fixture(std::path::Path::new("."), config, items);
        let mut ev = event();
        let select = |fx: &_, ev: &_| E1.select(fx, ev, &mut SessionState::default());
        for cwd in [".", "./"] {
            ev.cwd = cwd.into();
            assert!(select(&fx, &ev).full_ids.is_empty());
        }
        ev.cwd = "pai".into();
        assert_eq!(
            select(&fx, &ev).full_ids.len(),
            if count <= 2 { count } else { 0 }
        );
        fx.config.workspace.parent_scope_max = 3;
        assert_eq!(select(&fx, &ev).full_ids.len(), count);
        fx.config.workspace.parent_scope_max = 0;
        assert!(select(&fx, &ev).full_ids.is_empty());
        ev.prompt = Some("app0".into());
        assert_eq!(select(&fx, &ev).full_ids, ["r0"]);
        ev.prompt = None;
        ev.cwd = "pai/app0/src".into();
        assert_eq!(select(&fx, &ev).full_ids, ["r0"]);
    }
    let c = Config::parse(
        "[workspace]\nparent_scope_max=0",
        std::path::Path::new("test"),
    )
    .unwrap();
    assert_eq!(c.workspace.parent_scope_max, 0);
}

#[test]
fn ponteiro_corta_caracteres_antes_do_escape() {
    let mut i = item();
    i.title = format!("<elo\n\"{}FIM", "é".repeat(74));
    let fx = live_fixture(std::path::Path::new("."), Config::default(), vec![i]);
    let ev = event();
    let selection = E1.select(&fx, &ev, &mut SessionState::default());
    let text = render(&fx, &ev, &selection);
    assert!(text.contains(&format!("‹elo   &quot;{}", "é".repeat(74))));
    assert!(!text.contains("FIM"));
    assert_eq!(selection.used_bytes, text.len());
}

#[test]
fn replay_ordena_eventos_antes_de_aplicar_arquivos_tocados_e_usa_prompt() {
    let mut i = item();
    i.triggers.always = false;
    let fx = live_fixture(std::path::Path::new("."), Config::default(), vec![i]);
    let records = [
        serde_json::json!({"at":"2026-01-01T00:00:00Z","cwd":".","session_id":"s","event_seq":1,"event_type":"post-tool-use","tool_paths":["src/a"]}),
        serde_json::json!({"at":"2026-01-01T00:00:00Z","cwd":".","session_id":"s","event_seq":2,"prompt":"normal"}),
        serde_json::json!({"at":"2026-01-01T00:00:00Z","cwd":".","session_id":"t","event_seq":1,"prompt":"foco"}),
    ];
    let log = |order: &[usize]| {
        order
            .iter()
            .map(|&i| records[i].to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let ordered = elo_ctx::bench::replay::aggregate(&fx, log(&[0, 1, 2]).as_bytes(), &[]).unwrap();
    let shuffled = elo_ctx::bench::replay::aggregate(&fx, log(&[2, 1, 0]).as_bytes(), &[]).unwrap();
    assert_eq!(ordered, shuffled);
    assert!(ordered.contains("(mean) 1.00"));
    assert!(ordered.contains("events without request 0"));
}

#[test]
fn explain_por_id_ignora_outro_item_malformado() {
    let dir = tempfile::tempdir().unwrap();
    elo_ctx::init::init(dir.path()).unwrap();
    let store = elo_ctx::store::Store::open(dir.path()).unwrap();
    for id in ["ok", "broken"] {
        store
            .insert_item(&elo_ctx::store::NewItem {
                id,
                kind: "rule",
                scope: "global",
                title: "t",
                body: "b",
                origin: "agent",
            })
            .unwrap();
    }
    rusqlite::Connection::open(store.path()).unwrap().execute_batch(
        "PRAGMA ignore_check_constraints=ON; UPDATE items SET kind='invalid' WHERE id='broken';"
    ).unwrap();
    assert!(elo_ctx::live::load_items(&store).is_err());
    assert_eq!(elo_ctx::live::load_item(&store, "ok").unwrap().id, "ok");
    assert!(matches!(
        elo_ctx::live::load_item(&store, "missing"),
        Err(elo_ctx::store::StoreError::UnknownItem(_))
    ));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_elo"))
        .current_dir(dir.path())
        .args(["explain", "ok"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("demoted: no"));
}

#[test]
fn fixture_nao_aceita_feedback_forjado() {
    let input = serde_json::json!({
        "id":"r", "kind":"rule", "scope":"global", "title":"t", "body":"b",
        "triggers":{"always":true,"keywords":[],"paths":[]}, "importance":1.0,
        "created_at":"2026-01-01T00:00:00Z", "updated_at":"2026-01-01T00:00:00Z",
        "status":"active", "origin":"agent", "feedback":{"good":99,"bad":0}
    });
    // deny_unknown_fields rejects the skipped input field rather than trusting it.
    assert!(serde_json::from_value::<Item>(input).is_err());
}
