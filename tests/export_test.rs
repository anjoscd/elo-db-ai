mod support;
use std::fs;
use support::*;
#[test]
fn export_preserves_user_text_is_idempotent_and_cleans_only_owned_files() {
    let dir = workspace();
    fs::write(
        dir.path().join("AGENTS.md"),
        "User prefix\n<!-- elo:begin -->\nold\n<!-- elo:end -->\nUser suffix\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join(".cursor/rules")).unwrap();
    fs::write(dir.path().join(".cursor/rules/elo-obsolete.mdc"), "old").unwrap();
    fs::write(dir.path().join(".cursor/rules/user.mdc"), "user rule").unwrap();
    let args = ["export", "--formato", "todos"];
    ok(dir.path(), &args);
    let agents = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
    assert!(agents.starts_with("User prefix\n"));
    assert!(agents.ends_with("\nUser suffix\n"));
    assert!(agents.contains("Preserve user data."));
    assert!(agents.contains("Check the schema."));
    assert!(ok(dir.path(), &args).is_empty());
    assert_eq!(
        fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        agents
    );
    assert!(!dir.path().join(".cursor/rules/elo-obsolete.mdc").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join(".cursor/rules/user.mdc")).unwrap(),
        "user rule"
    );
    for path in [
        ".cursor/rules/elo-paths.mdc",
        ".github/instructions/elo-paths.instructions.md",
        ".devin/rules/elo-paths.md",
        ".clinerules/elo-paths.md",
    ] {
        let text = fs::read_to_string(dir.path().join(path)).unwrap();
        assert!(text.contains("db/migrations*"));
        assert!(text.contains("db/migrations*/**"));
    }
    assert!(fs::read_to_string(dir.path().join(".cursor/rules/elo.mdc"))
        .unwrap()
        .contains("alwaysApply: true"));
    assert!(dir.path().join("GEMINI.md").exists());
}
#[test]
fn dry_run_output_scope_and_safe_names() {
    let dir = workspace();
    fs::write(dir.path().join("extra.jsonl"),r#"{"id":"../../UNSAFE","kind":"law","scope":"project:demo","title":"Scoped","body":"Scoped law.","paths":["src*"]}"#).unwrap();
    ok(dir.path(), &["import", "extra.jsonl", "--as-user"]);
    let args = [
        "export",
        "--formato",
        "todos",
        "--saida",
        "out",
        "--escopo",
        "project:demo",
    ];
    let mut dry = args.to_vec();
    dry.push("--dry-run");
    assert!(ok(dir.path(), &dry).contains("would write"));
    assert!(!dir.path().join("out").exists());
    ok(dir.path(), &args);
    let files: Vec<_> = fs::read_dir(dir.path().join("out/.clinerules"))
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    assert_eq!(files.len(), 1);
    let name = files[0].file_name().to_string_lossy().into_owned();
    assert!(name
        .strip_suffix(".md")
        .unwrap()
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    assert!(!fs::read_to_string(dir.path().join("out/AGENTS.md"))
        .unwrap()
        .contains("Preserve user data."));
}
#[test]
fn malformed_markers_and_symlinks_are_preserved() {
    let dir = workspace();
    fs::write(dir.path().join("AGENTS.md"), "user\n<!-- elo:begin -->\n").unwrap();
    assert!(!command(dir.path(), &["export"]).status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        "user\n<!-- elo:begin -->\n"
    );
    #[cfg(unix)]
    {
        fs::remove_file(dir.path().join("AGENTS.md")).unwrap();
        fs::write(dir.path().join("user.md"), "keep").unwrap();
        std::os::unix::fs::symlink("user.md", dir.path().join("AGENTS.md")).unwrap();
        assert!(!command(dir.path(), &["export"]).status.success());
        assert_eq!(
            fs::read_to_string(dir.path().join("user.md")).unwrap(),
            "keep"
        );
    }
}

#[test]
fn every_shared_file_preserves_content_and_archived_laws_disappear() {
    let dir = workspace();
    for name in [
        "AGENTS.md",
        "GEMINI.md",
        ".github/copilot-instructions.md",
        ".cursor/rules/elo.mdc",
        ".clinerules",
    ] {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            "prefix\n<!-- elo:begin -->\nold\n<!-- elo:end -->\nsuffix",
        )
        .unwrap();
    }
    ok(dir.path(), &["export", "--formato", "todos"]);
    for name in [
        "AGENTS.md",
        "GEMINI.md",
        ".github/copilot-instructions.md",
        ".cursor/rules/elo.mdc",
        ".clinerules",
    ] {
        let text = fs::read_to_string(dir.path().join(name)).unwrap();
        assert!(text.starts_with("prefix\n"));
        assert!(text.ends_with("\nsuffix"));
        assert!(text.contains("Preserve user data."));
    }
    rusqlite::Connection::open(dir.path().join(".elo/elo.db"))
        .unwrap()
        .execute("UPDATE items SET status='archived' WHERE id='paths'", [])
        .unwrap();
    let before = fs::read_to_string(dir.path().join(".cursor/rules/elo-paths.mdc")).unwrap();
    assert!(
        ok(dir.path(), &["export", "--formato", "todos", "--dry-run"]).contains("would remove")
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(".cursor/rules/elo-paths.mdc")).unwrap(),
        before
    );
    ok(dir.path(), &["export", "--formato", "todos"]);
    assert!(!dir.path().join(".cursor/rules/elo-paths.mdc").exists());
}

#[test]
fn scoped_export_preserves_other_active_native_rules() {
    let dir = workspace();
    fs::write(dir.path().join("extra.jsonl"),r#"{"id":"scoped","kind":"law","scope":"project:demo","title":"Scoped","body":"Scoped law.","paths":["src*"]}"#).unwrap();
    ok(dir.path(), &["import", "extra.jsonl", "--as-user"]);
    ok(dir.path(), &["export", "--formato", "cursor"]);
    let global = fs::read_to_string(dir.path().join(".cursor/rules/elo-paths.mdc")).unwrap();
    ok(
        dir.path(),
        &["export", "--formato", "cursor", "--escopo", "project:demo"],
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(".cursor/rules/elo-paths.mdc")).unwrap(),
        global
    );
}

#[test]
fn windsurf_current_and_legacy_are_explicit_and_separate() {
    for (format, expected, absent) in [
        ("windsurf", ".devin/rules", ".windsurf/rules"),
        ("windsurf-legado", ".windsurf/rules", ".devin/rules"),
    ] {
        let dir = workspace();
        ok(dir.path(), &["export", "--formato", format]);
        assert!(dir.path().join(expected).join("elo-always.md").is_file());
        assert!(!dir.path().join(absent).exists());
        assert!(ok(dir.path(), &["export", "--formato", format]).is_empty());
    }
}

#[test]
fn absolute_temporary_output_allows_symlink_ancestors_but_not_inner_links() {
    let dir = workspace();
    let temp = tempfile::tempdir().unwrap();
    ok(
        dir.path(),
        &["export", "--saida", temp.path().to_str().unwrap()],
    );
    assert!(temp.path().join("AGENTS.md").is_file());
    #[cfg(unix)]
    {
        let alias = dir.path().join("temp-alias");
        std::os::unix::fs::symlink(temp.path(), &alias).unwrap();
        let output = alias.join("output");
        ok(dir.path(), &["export", "--saida", output.to_str().unwrap()]);
        assert!(output.join("AGENTS.md").is_file());
        std::os::unix::fs::symlink(temp.path(), output.join(".cursor")).unwrap();
        assert!(!command(
            dir.path(),
            &[
                "export",
                "--formato",
                "cursor",
                "--saida",
                output.to_str().unwrap()
            ]
        )
        .status
        .success());
    }
}
