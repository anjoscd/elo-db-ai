use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use elo_ctx::work::{session_start_block, Work, BLOCK_MAX_BYTES};

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    elo_ctx::store::Store::create(dir.path()).unwrap();
    std::fs::create_dir_all(dir.path().join("app/src")).unwrap();
    std::fs::create_dir_all(dir.path().join("outro")).unwrap();
    dir
}

fn elo(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn elo_fails(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(!out.status.success(), "{args:?} should fail");
    String::from_utf8(out.stderr).unwrap()
}

fn age(dir: &Path, table: &str, id: i64, days: i64) {
    let conn = rusqlite::Connection::open(dir.join(".elo/work.db")).unwrap();
    conn.execute(
        &format!("UPDATE {table} SET updated_at = updated_at - ?1 WHERE n = ?2"),
        rusqlite::params![days * 86_400, id],
    )
    .unwrap();
}

fn setup(dir: &Path) {
    elo(
        dir,
        &["project", "add", "app", "--path", "app", "--name", "App"],
    );
}

#[test]
fn projeto_tarefa_marco_e_ids_curtos() {
    let dir = workspace();
    setup(dir.path());
    assert!(elo(dir.path(), &["project", "list"]).starts_with("app 0 open "));
    elo(
        dir.path(),
        &[
            "milestone",
            "add",
            "--project",
            "app",
            "v1",
            "Primeira versão",
        ],
    );
    let t = elo(
        dir.path(),
        &[
            "task",
            "add",
            "--project",
            "app",
            "fazer login",
            "--milestone",
            "v1",
        ],
    );
    assert_eq!(t.trim(), "t1 todo [app] fazer login (milestone v1)");
    elo(
        dir.path(),
        &["task", "update", "t1", "--status", "in_progress"],
    );
    assert!(elo(dir.path(), &["task", "list"]).contains("t1 in_progress"));
    elo(dir.path(), &["task", "done", "1"]);
    assert_eq!(elo(dir.path(), &["task", "list"]).trim(), "no tasks");
    assert!(elo(dir.path(), &["task", "list", "--all"]).contains("t1 completed"));
    assert!(elo(dir.path(), &["milestone", "list"]).starts_with("v1 [app] 1/1 "));
    let json = elo(dir.path(), &["task", "list", "--all", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v[0]["id"], "t1");
    assert!(elo_fails(dir.path(), &["task", "update", "t1", "--status", "xyz"]).contains("status"));
    assert!(
        elo_fails(dir.path(), &["task", "add", "--project", "nada", "x"])
            .contains("does not exist")
    );
    assert!(elo_fails(
        dir.path(),
        &["task", "add", "--project", "app", "x", "--milestone", "zz"]
    )
    .contains("milestone"));
}

#[test]
fn project_add_resolve_path_a_partir_da_raiz() {
    let dir = workspace();
    let p = dir.path();
    let out = elo(
        &p.join("outro"),
        &["project", "add", "app", "--path", "app", "--name", "App"],
    );
    let expected = std::fs::canonicalize(p.join("app")).unwrap();
    assert!(out.contains(expected.to_str().unwrap()), "{out}");
    assert!(!p.join("outro/.elo").exists());
}

#[test]
fn reabrir_tarefa_rebloqueia_dependentes_e_recusa_status_com_bloqueador() {
    let dir = workspace();
    let p = dir.path();
    setup(p);
    elo(p, &["task", "add", "--project", "app", "base"]);
    elo(
        p,
        &[
            "task",
            "add",
            "--project",
            "app",
            "filha",
            "--blocked-by",
            "t1",
        ],
    );
    elo(p, &["task", "done", "t1"]);
    assert!(elo(p, &["task", "list"]).contains("t2 todo"));
    elo(p, &["task", "update", "t1", "--status", "in_progress"]);
    assert!(elo(p, &["task", "list"]).contains("t2 blocked"));
    let err = elo_fails(p, &["task", "update", "t2", "--status", "todo"]);
    assert!(err.contains("t1"), "{err}");
    elo_fails(p, &["task", "update", "t2", "--status", "in_progress"]);
    elo(p, &["task", "update", "t2", "--status", "blocked"]);
    elo(p, &["task", "done", "t1"]);
    assert!(elo(p, &["task", "list"]).contains("t2 todo"));
}

#[test]
fn concluir_lista_o_que_foi_desbloqueado() {
    let dir = workspace();
    setup(dir.path());
    let p = dir.path();
    elo(p, &["task", "add", "--project", "app", "base"]);
    elo(p, &["task", "add", "--project", "app", "outra"]);
    let b = elo(
        p,
        &[
            "task",
            "add",
            "--project",
            "app",
            "depende de duas",
            "--blocked-by",
            "t1",
            "--blocked-by",
            "t2",
        ],
    );
    assert!(b.contains("t3 blocked") && b.contains("(waiting on t1,t2)"));
    elo(
        p,
        &[
            "task",
            "add",
            "--project",
            "app",
            "depende de t1",
            "--blocked-by",
            "t1",
        ],
    );
    let first = elo(p, &["task", "done", "t1"]);
    assert!(first.contains("unblocked t4 depende de t1"));
    assert!(!first.contains("unblocked t3"), "t3 ainda espera t2");
    let second = elo(p, &["task", "done", "t2"]);
    assert!(second.contains("unblocked t3 depende de duas"));
    assert!(elo(p, &["task", "list"]).contains("t3 todo"));
}

#[test]
fn passagem_de_bastao_ciclo_completo() {
    let dir = workspace();
    setup(dir.path());
    let p = dir.path();
    let n = elo(
        p,
        &[
            "handoff",
            "new",
            "--project",
            "app",
            "--title",
            "Continuar o login",
            "--body",
            "falta o refresh token\nver auth.rs",
        ],
    );
    assert!(n.starts_with("h1 [app] Continuar o login"));
    assert!(elo(p, &["handoff", "list"]).contains("h1"));
    let show = elo(p, &["handoff", "show", "REFRESH"]);
    assert!(show.contains("Continuar o login") && show.contains("ver auth.rs"));
    assert!(elo(p, &["handoff", "show", "h1"]).contains("refresh token"));
    assert!(elo(p, &["handoff", "close", "h1"]).contains("(closed)"));
    assert_eq!(elo(p, &["handoff", "list"]).trim(), "no handoffs");
    assert!(elo_fails(p, &["handoff", "show", "inexistente"]).contains("no handoff matching"));
}

#[test]
fn bloco_so_do_projeto_ativo_e_curto() {
    let dir = workspace();
    let p = dir.path();
    assert!(
        session_start_block(p, &p.join("app")).is_none(),
        "sem work.db"
    );
    assert!(!p.join(".elo/work.db").exists(), "leitura não cria o banco");
    setup(p);
    elo(p, &["project", "add", "outro", "--path", "outro"]);
    assert!(
        session_start_block(p, &p.join("app")).is_none(),
        "nada aberto"
    );
    elo(p, &["task", "add", "--project", "app", "no backlog"]);
    assert!(
        session_start_block(p, &p.join("app")).is_none(),
        "todo não aparece"
    );
    elo(p, &["task", "add", "--project", "app", "fazendo agora"]);
    elo(p, &["task", "update", "t2", "--status", "in_progress"]);
    elo(
        p,
        &[
            "task",
            "add",
            "--project",
            "app",
            "esperando",
            "--blocked-by",
            "t2",
        ],
    );
    elo(
        p,
        &[
            "handoff",
            "new",
            "--project",
            "app",
            "--title",
            "Passagem A",
            "--body",
            "corpo longo que não entra",
        ],
    );
    elo(
        p,
        &["task", "add", "--project", "outro", "segredo do outro"],
    );
    elo(p, &["task", "update", "t4", "--status", "in_progress"]);

    let block = session_start_block(p, &p.join("app/src")).unwrap();
    assert!(block.starts_with("ELO work [app]"));
    assert!(block.contains("handoff h1: Passagem A"));
    assert!(block.contains("doing t2: fazendo agora"));
    assert!(block.contains("blocked t3: esperando (waiting on t2)"));
    assert!(!block.contains("corpo longo") && !block.contains("segredo do outro"));
    assert!(
        session_start_block(p, p).is_none(),
        "fora de qualquer projeto"
    );
    let outro = session_start_block(p, &p.join("outro")).unwrap();
    assert!(outro.contains("segredo do outro") && !outro.contains("fazendo agora"));
}

#[test]
fn bloco_respeita_600_bytes_e_mantem_a_contagem_de_adiados() {
    let dir = workspace();
    let p = dir.path();
    setup(p);
    let mut work = Work::open(p).unwrap();
    for i in 0..30 {
        let long = format!(
            "tarefa número {i} com texto bem comprido ççç ãããã {}",
            "x".repeat(100)
        );
        work.add_task("app", &long, None, &[]).unwrap();
        work.set_status(&format!("t{}", i + 1), "in_progress")
            .unwrap();
    }
    work.new_handoff("app", &"título ".repeat(40), "b").unwrap();
    drop(work);
    age(p, "tasks", 30, 20);
    let block = session_start_block(p, &p.join("app")).unwrap();
    assert!(block.len() < BLOCK_MAX_BYTES, "{} bytes", block.len());
    assert!(block.starts_with("ELO work [app]"));
    assert!(block.ends_with("1 deferred: elo task list --deferred"));
}

#[test]
fn adiamento_por_dias_sem_mexer() {
    let dir = workspace();
    let p = dir.path();
    setup(p);
    elo(p, &["task", "add", "--project", "app", "velha"]);
    elo(p, &["task", "update", "t1", "--status", "in_progress"]);
    elo(p, &["task", "add", "--project", "app", "nova"]);
    elo(
        p,
        &[
            "handoff",
            "new",
            "--project",
            "app",
            "--title",
            "velha passagem",
            "--body",
            "b",
        ],
    );
    age(p, "tasks", 1, 11);
    age(p, "handoffs", 1, 11);
    assert!(
        elo(p, &["task", "list"]).contains("t2 todo") && !elo(p, &["task", "list"]).contains("t1")
    );
    let late = elo(p, &["task", "list", "--deferred"]);
    assert!(late.contains("t1 deferred") && late.contains("(idle 11d)"));
    let block = session_start_block(p, &p.join("app")).unwrap();
    assert!(block.contains("1 deferred: elo task list --deferred"));
    assert!(block.contains("1 deferred handoff:"));
    assert!(!block.contains("fazendo t1") && !block.contains("h1:"));
    // mexer na tarefa tira do adiamento
    elo(p, &["task", "update", "t1", "--status", "in_progress"]);
    assert!(elo(p, &["task", "list"]).contains("t1 in_progress"));
}

#[test]
fn stale_days_vem_do_elo_toml() {
    let dir = workspace();
    let p = dir.path();
    std::fs::write(p.join("elo.toml"), "[work]\nstale_days = 3\n").unwrap();
    setup(p);
    elo(p, &["task", "add", "--project", "app", "x"]);
    age(p, "tasks", 1, 4);
    assert!(elo(p, &["task", "list", "--deferred"]).contains("t1 deferred"));
    std::fs::write(p.join("elo.toml"), "[work]\nstale_days = 0\n").unwrap();
    assert!(elo(p, &["task", "list"]).contains("t1 todo"));
}

#[test]
fn hook_session_start_imprime_o_bloco() {
    let dir = workspace();
    let p = dir.path();
    setup(p);
    elo(
        p,
        &[
            "handoff",
            "new",
            "--project",
            "app",
            "--title",
            "Retomar",
            "--body",
            "b",
        ],
    );
    let run = |event: &str, cwd: &Path| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_elo"))
            .args(["hook", event, "--live"])
            .current_dir(p)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = serde_json::json!({"session_id": "s1", "cwd": cwd}).to_string();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
    };
    let out = run("session-start", &p.join("app"));
    assert!(out.contains("handoff h1: Retomar"), "{out}");
    assert!(run("session-start", p).is_empty());
    assert!(run("post-tool-use", &p.join("app")).is_empty());
}

#[test]
fn bloco_custa_menos_de_10ms() {
    let dir = workspace();
    let p = dir.path();
    setup(p);
    let mut work = Work::open(p).unwrap();
    for i in 0..200 {
        work.add_task("app", &format!("tarefa {i}"), None, &[])
            .unwrap();
    }
    work.set_status("t1", "in_progress").unwrap();
    drop(work);
    let cwd = p.join("app");
    session_start_block(p, &cwd).unwrap();
    let start = std::time::Instant::now();
    for _ in 0..20 {
        session_start_block(p, &cwd).unwrap();
    }
    let each = start.elapsed() / 20;
    println!("session_start_block: {each:?}");
    let budget_ms = if cfg!(debug_assertions) { 50 } else { 10 };
    assert!(
        each < std::time::Duration::from_millis(budget_ms),
        "{each:?}"
    );
}
