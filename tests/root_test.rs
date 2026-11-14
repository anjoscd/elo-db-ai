use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

fn run(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_elo"))
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

fn add(cwd: &Path) -> Output {
    run(
        cwd,
        &[
            "add",
            "law",
            "--scope",
            "global",
            "--title",
            "Preserve data",
            "--body",
            "Always preserve user data.",
            "--triggers",
            "preserve",
            "--as-user",
            "--id",
            "root-law",
        ],
    )
}

fn hook(cwd: &Path) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_elo"))
        .current_dir(cwd)
        .args(["hook", "user-prompt-submit", "--live"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::json!({"cwd": cwd, "session_id": "root-test", "prompt": "Preserve data"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn add_from_subfolder_uses_root_store() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(dir.path(), &["init"]).status.success());
    let sub = dir.path().join("project/src");
    fs::create_dir_all(&sub).unwrap();
    let output = add(&sub);
    assert!(output.status.success(), "{output:?}");
    assert!(!sub.join(".elo").exists());
    assert!(!dir.path().join("project/.elo").exists());
    let output = run(dir.path(), &["recall", "Preserve", "--json"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("root-law"));
}

#[test]
fn add_without_root_fails_without_creating_files() {
    let dir = tempfile::tempdir().unwrap();
    let output = add(dir.path());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("No ELO root found"));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn subfolder_hook_receives_root_laws() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(dir.path(), &["init"]).status.success());
    assert!(add(dir.path()).status.success());
    let sub = dir.path().join("project");
    fs::create_dir(&sub).unwrap();
    let output = hook(&sub);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Always preserve user data."));
    assert!(!sub.join(".elo").exists());
}

#[test]
fn hook_skips_directory_without_database_and_delivers_parent_law() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(dir.path(), &["init"]).status.success());
    assert!(add(dir.path()).status.success());
    let sub = dir.path().join("project");
    fs::create_dir_all(sub.join(".elo/sessions")).unwrap();
    fs::write(sub.join(".elo/discovery.json"), "{}").unwrap();
    let output = hook(&sub);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Always preserve user data."));
    assert!(!sub.join(".elo/elo.db").exists());
    assert_eq!(fs::read_dir(sub.join(".elo")).unwrap().count(), 2);
    assert_eq!(fs::read_dir(sub.join(".elo/sessions")).unwrap().count(), 0);
    let output = run(&sub, &["recall", "Preserve", "--json"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("root-law"));
    let output = run(&sub, &["doctor"]);
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("warn nested"));
}

#[test]
fn doctor_warns_only_within_three_levels_and_skips_generated_directories() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(dir.path(), &["init"]).status.success());
    for path in [
        "one/.elo",
        "one/two/.elo",
        "one/two/three/.elo",
        "one/two/three/four/.elo",
        "node_modules/pkg/.elo",
        "target/build/.elo",
        ".git/objects/.elo",
        ".venv/pkg/.elo",
        "venv/pkg/.elo",
        "build/pkg/.elo",
        "dist/pkg/.elo",
        ".next/pkg/.elo",
        ".cache/pkg/.elo",
        "vendor/pkg/.elo",
        ".dart_tool/pkg/.elo",
        "Pods/pkg/.elo",
    ] {
        fs::create_dir_all(dir.path().join(path)).unwrap();
        fs::write(dir.path().join(path).join("elo.db"), "").unwrap();
    }
    let output = run(dir.path(), &["doctor"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.matches("warn nested ELO root:").count(), 3);
    assert!(text.contains("hides the workspace root laws"));
    assert!(text.contains("move its items to the workspace root and rename"));
    assert!(!text.contains("four/.elo"));
    assert!(!text.contains("pkg/.elo"));
    assert!(!text.contains("build/.elo"));
    assert!(!text.contains("objects/.elo"));
    assert!(dir.path().join("one/.elo/elo.db").is_file());
}

#[test]
fn init_creates_in_current_directory_even_below_a_root() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(dir.path(), &["init"]).status.success());
    let sub = dir.path().join("project");
    fs::create_dir(&sub).unwrap();
    assert!(run(&sub, &["init"]).status.success());
    assert!(sub.join(".elo/elo.db").is_file());
}

#[test]
fn opening_missing_store_never_creates_directories_or_database() {
    let dir = tempfile::tempdir().unwrap();
    assert!(elo_ctx::store::Store::open(dir.path()).is_err());
    assert!(!dir.path().join(".elo").exists());
    fs::create_dir(dir.path().join(".elo")).unwrap();
    assert!(elo_ctx::store::Store::open(dir.path()).is_err());
    assert!(!dir.path().join(".elo/elo.db").exists());
}

#[test]
fn import_from_subfolder_keeps_input_relative_to_cwd_and_store_at_root() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(dir.path(), &["init"]).status.success());
    let sub = dir.path().join("project");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("items.jsonl"),
        r#"{"id":"imported-rule","kind":"rule","scope":"global","title":"Imported","body":"Preserve data."}"#,
    ).unwrap();
    let output = run(&sub, &["import", "items.jsonl"]);
    assert!(output.status.success(), "{output:?}");
    assert!(!sub.join(".elo").exists());
    let output = run(dir.path(), &["recall", "Imported", "--json"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("imported-rule"));
}

#[cfg(unix)]
#[test]
fn doctor_skips_unreadable_directories_and_symlinks() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    assert!(run(dir.path(), &["init"]).status.success());
    let blocked = dir.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(outside.path().join(".elo")).unwrap();
    fs::write(outside.path().join(".elo/elo.db"), "").unwrap();
    symlink(outside.path(), dir.path().join("linked")).unwrap();
    symlink(dir.path().join("missing"), dir.path().join("broken")).unwrap();
    fs::create_dir_all(dir.path().join("visible/.elo")).unwrap();
    fs::write(dir.path().join("visible/.elo/elo.db"), "").unwrap();
    let output = run(dir.path(), &["doctor"]);
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.matches("warn nested ELO root:").count(), 1);
    assert!(text.contains("visible/.elo"));
}

#[test]
fn rootless_commands_do_not_require_or_create_a_root() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["update", "--version", "invalid!"],
        vec!["bench", "--fixture", "missing-fixture"],
    ] {
        let output = run(dir.path(), &args);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("No ELO root found"));
    }
    assert!(hook(dir.path()).status.success());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn doctor_warns_about_database_inside_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(dir.path(), &["init"]).status.success());
    let nested = dir.path().join(".elo/.elo");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("elo.db"), "").unwrap();
    let output = run(dir.path(), &["doctor"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("warn nested ELO database:"));
    assert!(text.contains(".elo/.elo/elo.db"));
    assert!(nested.join("elo.db").is_file());
}
