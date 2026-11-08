use std::process::Command;
#[test]
fn setup_prints_valid_snippets_without_writing() {
    for tool in [
        "claude",
        "codex",
        "gemini",
        "cursor",
        "copilot",
        "vscode",
        "windsurf",
        "zed",
        "jetbrains",
    ] {
        for project in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_elo"));
            cmd.current_dir(dir.path()).args(["setup", tool]);
            if project {
                cmd.arg("--projeto");
            }
            let out = cmd.output().unwrap();
            assert!(out.status.success());
            let text = String::from_utf8(out.stdout).unwrap();
            assert!(text.starts_with("# "));
            assert!(text.contains("mcp"));
            for snippet in text.split("# ").skip(1) {
                let (_, body) = snippet.split_once('\n').unwrap();
                if body.starts_with('{') {
                    let _: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
                } else {
                    let _: toml::Value = toml::from_str(body.trim()).unwrap();
                }
            }
            if matches!(tool, "claude" | "codex" | "gemini" | "cursor" | "copilot") {
                assert!(text.contains(&format!("--formato {tool}")));
            }
            if tool == "gemini" {
                assert!(text.contains("10000"));
            }
            if tool == "copilot" {
                assert!(text.contains(if project {
                    "# .github/hooks/elo.json"
                } else {
                    "# ~/.copilot/hooks/elo.json"
                }));
            }
            if tool == "windsurf" {
                assert!(text.contains("~/.config/devin/mcp_config.json"));
                assert!(text.contains("~/.codeium/windsurf/mcp_config.json"));
                assert!(text.contains(".devin/rules"));
                assert!(text.contains(".windsurf/rules"));
            }
            if project && tool != "windsurf" {
                assert!(!text.contains("~/"));
            }
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
    }
}
