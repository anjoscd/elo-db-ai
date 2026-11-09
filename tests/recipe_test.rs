use std::process::Command;

use tempfile::TempDir;

fn elo(dir: &std::path::Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn recipe_add_list_and_delivery_by_keyword() {
    let dir = TempDir::new().unwrap();
    elo(dir.path(), &["init"]);
    let (ok, out) = elo(
        dir.path(),
        &[
            "recipe",
            "add",
            "Copilot com ferramentas liberadas",
            "--passo",
            "usar --allow-tool 'shell(cmd:*)'",
            "--passo",
            "negar git push",
            "--palavras",
            "copilot,allow-tool",
            "--as-user",
        ],
    );
    assert!(ok, "{out}");
    assert!(
        out.contains("recipe saved: receita-copilot-com-ferramentas-liberadas"),
        "{out}"
    );
    let (_, list) = elo(dir.path(), &["recipe", "list"]);
    assert!(
        list.contains("receita-copilot-com-ferramentas-liberadas"),
        "{list}"
    );
    let (_, ctx) = elo(
        dir.path(),
        &[
            "context",
            "global",
            "--ids",
            "receita-copilot-com-ferramentas-liberadas",
        ],
    );
    assert!(
        ctx.contains("1. usar --allow-tool") && ctx.contains("2. negar git push"),
        "{ctx}"
    );
    // One step is not a recipe.
    let (ok, _) = elo(dir.path(), &["recipe", "add", "x", "--passo", "só um"]);
    assert!(!ok);
}

#[test]
fn recipe_is_delivered_by_its_keyword_and_titles_are_handled() {
    let dir = TempDir::new().unwrap();
    elo(dir.path(), &["init"]);
    elo(
        dir.path(),
        &[
            "recipe",
            "add",
            "Copilot com ferramentas",
            "--passo",
            "a",
            "--passo",
            "b",
            "--palavras",
            "copilot",
            "--as-user",
        ],
    );
    // Delivered by the keyword, through the prompt hook.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(["hook", "user-prompt-submit", "--live"])
        .current_dir(dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    let body =
        serde_json::json!({"session_id":"s","cwd":dir.path(),"prompt":"rodar o copilot agora"});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.to_string().as_bytes())
        .unwrap();
    let out = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned();
    assert!(out.contains("receita-copilot-com-ferramentas"), "{out}");
    // Same title again: clear refusal. Symbols-only title: still gets an id.
    let (ok, _) = elo(
        dir.path(),
        &[
            "recipe",
            "add",
            "Copilot com ferramentas",
            "--passo",
            "a",
            "--passo",
            "b",
        ],
    );
    assert!(!ok);
    let (ok, out) = elo(
        dir.path(),
        &["recipe", "add", "🚀🚀", "--passo", "a", "--passo", "b"],
    );
    assert!(ok && out.contains("recipe saved: receita-"), "{out}");
}

#[test]
fn recipe_accepts_english_step_and_keywords_flags() {
    let dir = TempDir::new().unwrap();
    elo(dir.path(), &["init"]);
    let (ok, out) = elo(
        dir.path(),
        &[
            "recipe",
            "add",
            "English flags recipe",
            "--step",
            "first step",
            "--step",
            "second step",
            "--keywords",
            "english,flags",
            "--as-user",
        ],
    );
    assert!(ok, "{out}");
    assert!(
        out.contains("recipe saved: receita-english-flags-recipe"),
        "{out}"
    );
}
