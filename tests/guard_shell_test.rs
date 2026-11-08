use elo_ctx::{config::Config, guard::Guard};
use serde_json::json;

const GUARDS: &str = include_str!("fixtures/guards-exemplo.toml");

fn guards() -> Vec<Guard> {
    toml::from_str::<Config>(GUARDS).unwrap().guards
}

#[test]
fn real_shell_shapes() {
    let guards = guards();
    let cases = [
        ("push-so-com-ordem", "git commit -m \"$(cat <<'EOF'\ngit push origin main\nEOF\n)\"", false),
        ("push-so-com-ordem", "cd x && git push", true),
        ("publicar-so-com-ordem", "sed -n 1,5p scripts/deploy-functions.sh", false),
        ("publicar-so-com-ordem", "bash ~/x/deploy-functions.sh", true),
        ("revisor-sem-hook-rs", "timeout 560 env MODELO=claude-opus-5 ~/s/revisor-externo.sh r p.md src/main.rs src/hook.rs", true),
        ("revisor-sem-hook-rs", "timeout 560s env MODELO=x ~/s/revisor-externo.sh r src/main.rs", false),
        ("copilot-sem-allow-all", "nohup copilot -p \"$(cat p)\" --allow-tool write --allow-all-tools > log &", true),
        ("copilot-sem-allow-all", "copilot --allow-tool 'shell(cargo:*)'", false),
        ("sem-linha-de-processo", "ps aux | grep x", true),
        ("sem-linha-de-processo", "pkill -f x", false),
        ("site-sem-preview", "npx vite preview", true),
        ("push-so-com-ordem", "echo 'git push'", false),
    ];
    for (id, command, expected) in cases {
        let guard = guards.iter().find(|g| g.id == id).unwrap();
        assert_eq!(
            guard.matches("Bash", &json!({"command": command})),
            expected,
            "{id}: {command}"
        );
    }
    let command = format!("cat > guards.toml <<'EOF'\n{}\nEOF", GUARDS);
    for guard in guards {
        assert!(
            !guard.matches("Bash", &json!({"command": command})),
            "{}",
            guard.id
        );
    }
}

fn matches(command: &str, all: &[&str], any: &[&str], none: &[&str]) -> bool {
    Guard {
        tool: "Bash".into(),
        field: "command".into(),
        all: all.iter().map(|s| s.to_string()).collect(),
        any: any.iter().map(|s| s.to_string()).collect(),
        none: none.iter().map(|s| s.to_string()).collect(),
        ..Guard::default()
    }
    .matches("Bash", &json!({"command": command}))
}

#[test]
fn heredocs_quotes_and_escapes_are_opaque() {
    let hostile_body = "git commit -m \"$(cat <<'EOF'\n\")\"; git push\nEOF\n)\"";
    assert!(!matches(hostile_body, &["git push"], &[], &[]));
    assert!(matches(
        &format!("{hostile_body}; git push"),
        &["git push"],
        &[],
        &[]
    ));

    for command in [
        "cat <<EOF\ngit push\nEOF",
        "cat <<'EOF'\ngit push\nEOF",
        "cat <<-\"X\"\n\tgit push\n\tX",
        "cat <<A <<'B'\ngit push\nA\ngit push\nB",
        "cat <<E\\OF\ngit push\nEOF",
        "cat <<EOF\ngit push",
        "echo \"$(echo \"git push\")\"",
        "echo \"escaped \\\" ; git push\"",
        "echo 'a; git push'",
        "echo git\\ push",
        "echo x \\; git push",
        "# git push\necho ok",
        "echo \"\" git push",
        "'echo' git push",
    ] {
        assert!(!matches(command, &["git push"], &[], &[]), "{command}");
    }
    assert!(matches(
        "cat <<EOF\ngit push\nEOF\ngit push",
        &["git push"],
        &[],
        &[]
    ));
    assert!(matches("git \\\n push", &["git push"], &[], &[]));
}

#[test]
fn segments_do_not_share_conditions() {
    assert!(matches("cat <<< 'text'\ngit push", &["git push"], &[], &[]));
    for separator in [";", "&&", "||", "|", "&", "\n", "(", ")", "$("] {
        let command = format!("echo x {separator} git push origin");
        assert!(matches(&command, &["git push"], &[], &[]), "{command}");
        let command = format!("copilot {separator} echo --allow-all-tools");
        assert!(!matches(
            &command,
            &["copilot", "--allow-all-tools"],
            &[],
            &[]
        ));
        assert!(!matches(
            &command,
            &["copilot"],
            &["--allow-all-tools"],
            &[]
        ));
    }
    assert!(matches(
        "echo --help; copilot --yolo",
        &["copilot"],
        &["--yolo"],
        &["--help"]
    ));
    assert!(!matches(
        "copilot --yolo --help",
        &["copilot"],
        &["--yolo"],
        &["--help"]
    ));
    assert!(!matches("echo git push", &[], &["git push"], &[]));
    assert!(matches("/usr/bin/git push", &[], &["git push"], &[]));
}

#[test]
fn wrappers_and_assignments() {
    for prefix in [
        "A=b",
        "A='a b' B=\"x\"",
        "bash",
        "sh",
        "zsh",
        "env A=b",
        "nohup",
        "exec",
        "command",
        "time",
        "npx",
        "sudo",
        "timeout 2",
        "timeout 1.5m",
        "xargs",
        "sudo -u root env A=b timeout -s TERM 2h bash --",
        "xargs -n 1",
        "env -u SECRET",
        "npx --yes",
    ] {
        let command = format!("{prefix} ~/scripts/deploy.sh x");
        assert!(matches(&command, &["deploy.sh"], &[], &[]), "{command}");
    }
    for command in [
        "env",
        "timeout",
        "sudo -u",
        "A=b",
        "",
        "timeout nope deploy.sh",
    ] {
        assert!(!matches(command, &["deploy.sh"], &[], &[]));
    }
}

#[test]
fn shell_command_arguments_have_bounded_recursion() {
    for command in [
        "bash -c \"git push\"",
        "sh -c 'git push'",
        "zsh -c 'git push'",
        "bash -lc 'git push'",
        "env A=b bash -c 'echo ok; git push'",
        "bash -c \"sh -c 'git push'\"",
        "bash -c 'git push' ignored",
        "bash -c \"sh -c \\\"git push\\\"\"",
    ] {
        assert!(matches(command, &["git push"], &[], &[]), "{command}");
    }
    for command in [
        "echo bash -c 'git push'",
        "bash -c 'echo git push'",
        "bash -c 'echo ok' 'git push'",
        "bash -c \"sh -c \\\"zsh -c 'git push'\\\"\"",
    ] {
        assert!(!matches(command, &["git push"], &[], &[]), "{command}");
    }
}

#[test]
fn backticks_and_shell_keywords() {
    for command in [
        "echo `git push`",
        "if git push; then echo ok; fi",
        "then git push",
        "else git push",
        "elif git push",
        "fi git push",
        "do git push",
        "done git push",
        "while git push",
        "until git push",
        "for git push",
        "! git push",
        "{ git push; }",
        "} git push",
        "case x in x) git push;; esac",
    ] {
        assert!(matches(command, &["git push"], &[], &[]), "{command}");
    }
    for command in [
        "echo '`git push`'",
        "echo \\`git push\\`",
        "cat <<EOF\n`git push`\nEOF",
    ] {
        assert!(!matches(command, &["git push"], &[], &[]), "{command}");
    }
    assert!(!matches(
        "copilot `echo --allow-all-tools`",
        &["copilot", "--allow-all-tools"],
        &[],
        &[]
    ));
}

#[test]
fn standalone_wrappers_and_process_fixture() {
    let guards = guards();
    let guard = guards
        .iter()
        .find(|g| g.id == "sem-linha-de-processo")
        .unwrap();
    for command in [
        "env",
        "env A=b",
        "env -u SECRET",
        "pgrep -a",
        "pgrep -fl x",
        "pgrep -lf x",
        "ps ax",
        "printenv",
    ] {
        assert!(
            guard.matches("Bash", &json!({"command":command})),
            "{command}"
        );
    }
    for command in [
        // `pgrep -f` alone prints only process ids, not command lines.
        "pgrep -f x",
        "env echo ok",
        "env A=b ls",
        "echo env",
        "environment",
        "env bash -c 'echo ok'",
    ] {
        assert!(
            !guard.matches("Bash", &json!({"command":command})),
            "{command}"
        );
    }
    for command in ["timeout", "timeout 2", "timeout -s TERM 2"] {
        assert!(matches(command, &["timeout"], &[], &[]), "{command}");
    }
}

#[test]
fn command_limit_respects_utf8_boundaries() {
    let command = format!("{}é; git push", " ".repeat(256 * 1024 - 1));
    assert!(!matches(&command, &["git push"], &[], &[]));
    let command = format!("git push; {}", "x".repeat(300 * 1024));
    assert!(matches(&command, &["git push"], &[], &[]));
}

#[test]
fn opaque_words_and_case_patterns_do_not_become_conditions() {
    assert!(!matches("copilot 'anything'", &["copilot", "0"], &[], &[]));
    assert!(!matches(
        "case x in git push) echo ok;; esac",
        &["git push"],
        &[],
        &[]
    ));
    assert!(!matches("env --help", &["env"], &[], &["--help"]));
}

#[test]
fn bash_options_with_operands_before_dash_c() {
    let guards = guards();
    let push = guards.iter().find(|g| g.id == "push-so-com-ordem").unwrap();
    for command in [
        "bash -o pipefail -c 'git push origin main'",
        "bash -O extglob -c \"git push\"",
        "bash --rcfile /dev/null -c 'git push'",
        "sh -e -c 'git push'",
    ] {
        assert!(
            push.matches("Bash", &json!({"command": command})),
            "{command}"
        );
    }
    assert!(!push.matches(
        "Bash",
        &json!({"command": "bash -o pipefail -c 'git status'"})
    ));
}
