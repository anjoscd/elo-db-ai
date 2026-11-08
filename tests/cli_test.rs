use std::fs;
use std::path::Path;
use std::process::Command;

fn elo(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_elo"));
    command.current_dir(dir);
    command
}

#[test]
fn init_creates_the_workspace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let status = elo(dir.path()).arg("init").status().expect("run elo init");
    assert!(status.success());
    assert!(dir.path().join(".elo").is_dir());
    assert!(dir.path().join("elo.toml").is_file());
}

#[test]
fn init_keeps_an_existing_configuration() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("elo.toml");
    fs::write(&config, "# custom\n").expect("write configuration");

    let status = elo(dir.path()).arg("init").status().expect("run elo init");
    assert!(status.success());
    assert_eq!(
        fs::read_to_string(&config).expect("read configuration"),
        "# custom\n"
    );
}

#[test]
fn doctor_succeeds_on_a_fresh_workspace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let status = elo(dir.path()).arg("init").status().expect("run elo init");
    assert!(status.success());

    let output = elo(dir.path())
        .arg("doctor")
        .output()
        .expect("run elo doctor");
    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn doctor_rejects_missing_schema_objects() {
    for (kind, name) in [
        ("TRIGGER", "items_after_insert"),
        ("TRIGGER", "items_after_update"),
        ("TRIGGER", "items_after_delete"),
        ("TABLE", "items"),
        ("TABLE", "items_fts"),
        ("TABLE", "meta"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        elo_ctx::init::init(dir.path()).unwrap();
        let conn = rusqlite::Connection::open(dir.path().join(".elo/elo.db")).unwrap();
        conn.execute_batch(&format!("DROP {kind} {name}")).unwrap();
        let output = elo(dir.path()).arg("doctor").output().unwrap();
        assert_eq!(output.status.code(), Some(1), "missing {name}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("fail store:"));
    }
}

#[cfg(unix)]
#[test]
fn closed_stdout_exits_successfully() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    for command in ["init", "doctor"] {
        let (writer, reader) = UnixStream::pair().unwrap();
        drop(reader);
        let fd: OwnedFd = writer.into();
        let output = elo(dir.path())
            .arg(command)
            .stdout(Stdio::from(fd))
            .output()
            .unwrap();
        assert!(output.status.success(), "{command}: {:?}", output);
        assert!(output.stderr.is_empty());
    }
}
