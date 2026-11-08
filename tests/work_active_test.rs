use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use elo_ctx::work::{session_start_block, Work, BLOCK_MAX_BYTES};

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".elo")).unwrap();
    dir
}

/// Registers `slug` at `path` with one task in progress.
fn project(p: &Path, slug: &str, path: &Path) {
    let mut work = Work::open(p).unwrap();
    work.add_project(slug, path, None).unwrap();
    work.add_task(slug, &format!("trabalho {slug}"), None, &[])
        .unwrap();
    let id = format!("t{}", work.tasks(None, true, false).unwrap().len());
    work.set_status(&id, "in_progress").unwrap();
}

fn block(p: &Path, cwd: &Path) -> Option<String> {
    session_start_block(p, cwd)
}

fn hook(p: &Path, cwd: &Path, home: Option<&Path>) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_elo"));
    cmd.args(["hook", "session-start", "--live"])
        .current_dir(p)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(h) = home {
        cmd.env("HOME", h);
    }
    let mut child = cmd.spawn().unwrap();
    let input = serde_json::json!({"session_id": "s1", "cwd": cwd}).to_string();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn elo(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

#[test]
fn projeto_registrado_antes_de_a_pasta_existir() {
    let dir = workspace();
    let p = dir.path();
    project(p, "novo", &p.join("futuro/app"));
    std::fs::create_dir_all(p.join("futuro/app/src")).unwrap();
    assert!(block(p, &p.join("futuro/app/src"))
        .unwrap()
        .contains("[novo]"));
    // cwd that does not exist yet resolves the same way
    assert!(block(p, &p.join("futuro/app/ainda/nao")).is_some());
}

#[cfg(unix)]
#[test]
fn link_simbolico_dos_dois_lados() {
    let dir = workspace();
    let p = dir.path();
    std::fs::create_dir_all(p.join("real/src")).unwrap();
    std::os::unix::fs::symlink(p.join("real"), p.join("atalho")).unwrap();
    project(p, "app", &p.join("atalho"));
    assert!(block(p, &p.join("real/src")).is_some(), "gravado pelo link");
    assert!(block(p, &p.join("atalho/src")).is_some());

    let dir = workspace();
    let p = dir.path();
    std::fs::create_dir_all(p.join("real/src")).unwrap();
    std::os::unix::fs::symlink(p.join("real"), p.join("atalho")).unwrap();
    project(p, "app", &p.join("real"));
    assert!(block(p, &p.join("atalho/src")).is_some(), "cwd pelo link");
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn caixa_diferente_em_sistema_sem_distincao() {
    let dir = workspace();
    let p = dir.path();
    std::fs::create_dir_all(p.join("App/src")).unwrap();
    project(p, "app", &p.join("App"));
    assert!(block(p, &p.join("app/SRC")).is_some());
    project(p, "novo", &p.join("Futuro"));
    assert!(block(p, &p.join("FUTURO/x")).is_some());
}

#[test]
fn projeto_mais_especifico_vence_a_pasta_mae() {
    let dir = workspace();
    let p = dir.path();
    std::fs::create_dir_all(p.join("app/src/x")).unwrap();
    project(p, "pai", &p.join("app"));
    project(p, "filho", &p.join("app/src"));
    assert!(block(p, &p.join("app/src/x")).unwrap().contains("[filho]"));
    assert!(block(p, &p.join("app")).unwrap().contains("[pai]"));
}

#[cfg(unix)]
#[test]
fn projeto_na_raiz_nao_captura_tudo() {
    let dir = workspace();
    let p = dir.path();
    std::fs::create_dir_all(p.join("app")).unwrap();
    project(p, "raiz", Path::new("/"));
    assert!(block(p, &p.join("app")).is_none());
    project(p, "app", &p.join("app"));
    assert!(block(p, &p.join("app")).unwrap().contains("[app]"));
}

#[test]
fn projeto_no_home_so_vale_para_o_proprio_home() {
    let dir = workspace();
    let p = dir.path().canonicalize().unwrap();
    let home = p.join("home");
    std::fs::create_dir_all(home.join("app")).unwrap();
    project(&p, "casa", &home);
    elo(&p, &["handoff", "list"]);
    let run = |cwd: &Path| String::from_utf8(hook(&p, cwd, Some(&home)).stdout).unwrap();
    assert!(run(&home.join("app")).is_empty(), "subpasta do home");
    assert!(run(&home).contains("[casa]"));
    project(&p, "app", &home.join("app"));
    assert!(run(&home.join("app")).contains("[app]"));
}

#[test]
fn fallback_pelo_elo_toml() {
    let dir = workspace();
    let p = dir.path();
    std::fs::create_dir_all(p.join("web/src")).unwrap();
    std::fs::create_dir_all(p.join("elsewhere")).unwrap();
    std::fs::write(
        p.join("elo.toml"),
        "[[project]]\nname = \"web\"\nroot = \"web\"\n",
    )
    .unwrap();
    project(p, "web", &p.join("elsewhere"));
    assert!(block(p, &p.join("web/src")).unwrap().contains("[web]"));
    assert!(block(p, p).is_none());
}

fn snapshot(dir: &Path) -> Vec<(String, std::time::SystemTime, Vec<u8>)> {
    let mut out: Vec<_> = std::fs::read_dir(dir.join(".elo"))
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_name().to_string_lossy().starts_with("work"))
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                e.metadata().unwrap().modified().unwrap(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn hook_nao_altera_a_pasta_work() {
    let dir = workspace();
    let p = dir.path();
    std::fs::create_dir_all(p.join("app")).unwrap();
    project(p, "app", &p.join("app"));
    let before = snapshot(p);
    assert_eq!(before.len(), 1, "só work.db: {:?}", before.len());
    std::thread::sleep(std::time::Duration::from_millis(20));
    let out = hook(p, &p.join("app"), None);
    assert!(String::from_utf8_lossy(&out.stdout).contains("[app]"));
    assert!(snapshot(p) == before, ".elo/work* mudou");
}

#[test]
fn bloco_corta_so_detalhes_e_mantem_rodape() {
    let dir = workspace();
    let p = dir.path();
    std::fs::create_dir_all(p.join("app")).unwrap();
    let mut work = Work::open(p).unwrap();
    work.add_project("app", &p.join("app"), None).unwrap();
    for i in 0..20 {
        work.add_task("app", &format!("tarefa {i} {}", "y".repeat(80)), None, &[])
            .unwrap();
        work.set_status(&format!("t{}", i + 1), "in_progress")
            .unwrap();
    }
    drop(work);
    let b = block(p, &p.join("app")).unwrap();
    assert!(b.len() < BLOCK_MAX_BYTES);
    assert!(b.starts_with("ELO work [app]\n"));
    assert!(b.contains("… +"), "{b}");
    assert!(!b.ends_with('…'));
}

#[test]
fn validacoes_baixas() {
    let dir = workspace();
    let p = dir.path();
    std::fs::create_dir_all(p.join("app")).unwrap();
    let mut work = Work::open(p).unwrap();
    for bad in [".", "..", &"a".repeat(41)] {
        assert!(
            work.add_project(bad, &p.join("app"), None).is_err(),
            "{bad}"
        );
    }
    work.add_project(&"a".repeat(40), &p.join("app"), None)
        .unwrap();
    work.add_project("app", &p.join("app"), None).unwrap();
    let err = work
        .add_task("app", "x", None, &["t1".to_string()])
        .unwrap_err();
    assert!(err.to_string().contains("cannot block itself"), "{err}");
    assert!(work.tasks(None, true, false).unwrap().is_empty());
    drop(work);
    let both = elo(p, &["task", "list", "--all", "--deferred"]);
    assert!(!both.status.success());
    let both = elo(p, &["handoff", "list", "--all", "--deferred"]);
    assert!(!both.status.success());
}

#[test]
fn erro_de_banco_sai_com_codigo_diferente_do_de_uso() {
    let dir = workspace();
    let p = dir.path();
    std::fs::write(
        p.join(".elo/work.db"),
        b"isto nao e sqlite, mas e bem longo ".repeat(40),
    )
    .unwrap();
    let usage = elo(p, &["task", "add", "--project", "nada", "x"]);
    let internal_code = usage.status.code();
    std::fs::remove_file(p.join(".elo/work.db")).unwrap();
    let usage = elo(p, &["task", "add", "--project", "nada", "x"]);
    // Usage errors exit with 2, like every other usage error of the CLI.
    assert_eq!(usage.status.code(), Some(2));
    assert_eq!(internal_code, Some(70));
}

/// Costly: `cargo test --release --test work_active_test -- --ignored --nocapture`
#[test]
#[ignore]
fn hook_de_ponta_a_ponta_com_historico_realista_custa_menos_de_10ms() {
    let dir = workspace();
    let p = dir.path();
    std::fs::create_dir_all(p.join("app")).unwrap();
    let mut work = Work::open(p).unwrap();
    work.add_project("app", &p.join("app"), None).unwrap();
    for i in 0..2050 {
        work.add_task("app", &format!("tarefa {i}"), None, &[])
            .unwrap();
        let id = format!("t{}", i + 1);
        if i < 2000 {
            work.set_status(&id, "completed").unwrap();
        } else if i % 5 == 0 {
            work.set_status(&id, "in_progress").unwrap();
        }
    }
    drop(work);
    let p95 = |dir: &Path| {
        let cwd = dir.join("app");
        std::fs::create_dir_all(&cwd).unwrap();
        hook(dir, &cwd, None);
        let mut times: Vec<_> = (0..20)
            .map(|_| {
                let t = std::time::Instant::now();
                hook(dir, &cwd, None);
                t.elapsed()
            })
            .collect();
        times.sort();
        times[18]
    };
    let with = p95(p);
    let bare = workspace();
    let without = p95(bare.path());
    println!("com work.db: {with:?}, sem: {without:?}");
    assert!(
        with < without + std::time::Duration::from_millis(10),
        "{with:?} vs {without:?}"
    );
}
