//! Agent hooks record context under `.elo/shadow/` in both shadow and live modes.
//! Live mode also returns the rendered prompt context after all writes succeed.
//! The CLI keeps errors and panics from breaking the agent session.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde::Deserialize;

use crate::bench::elo_selector::E1;
use crate::bench::fixture::{Event, Item, Status};
use crate::bench::selector::{render, Selector, SessionState};
use crate::config::Config;
use crate::discover::{discover, to_toml};
use crate::live::{live_fixture, load_items};
use crate::stats::Mode;
use crate::store::{Store, DATA_DIR};

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    cwd: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    defer_prompt: bool,
    #[serde(default)]
    suppress_pre_context: bool,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Session {
    seq: u64,
    state: SessionState,
    #[serde(default)]
    shown_work_project: Option<String>,
    #[serde(default)]
    pending_context: Option<String>,
}

/// Local heuristic: checks lines and their concatenation before the cut.
/// Does not guarantee detecting secrets without a marker, short ones, or with inner separators.
fn has_secret(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    let assignment = [
        "token",
        "senha",
        "password",
        "passwd",
        "secret",
        "api_key",
        "apikey",
        "authorization",
    ]
    .iter()
    .any(|key| {
        lower.match_indices(key).any(|(start, _)| {
            let suffix = lower[start + key.len()..].trim_start();
            // Also accepts quoted keys in JSON/YAML.
            let suffix = suffix.trim_start_matches(['\"', '\'']).trim_start();
            suffix.starts_with([':', '='])
        })
    });
    let prefix = [
        "bearer ",
        "sk-",
        "sk_",
        "rk_",
        "pk_live",
        "akia",
        "asia",
        "xox",
        "ghp_",
        "gho_",
        "github_pat_",
        "eyj",
        "aiza",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    let private_key = lower
        .split("begin")
        .skip(1)
        .any(|tail| tail.contains("private key"));
    let postgres = lower.split("postgres://").skip(1).any(|tail| {
        let authority = tail.split(['/', '?', '#']).next().unwrap_or_default();
        authority.rsplit_once('@').is_some_and(|(userinfo, _)| {
            userinfo
                .split_once(':')
                .is_some_and(|(_, password)| !password.is_empty())
        })
    });
    let encoded = prompt
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_')))
        .any(|part| {
            let hex = part.bytes().all(|b| b.is_ascii_hexdigit());
            let uuid = part.len() == 36
                && part.bytes().enumerate().all(|(i, b)| {
                    if matches!(i, 8 | 13 | 18 | 23) {
                        b == b'-'
                    } else {
                        b.is_ascii_hexdigit()
                    }
                });
            part.len() >= 32
                && !hex
                && !uuid
                && part.bytes().any(|b| b.is_ascii_uppercase())
                && part.bytes().any(|b| b.is_ascii_lowercase())
                && part.bytes().any(|b| b.is_ascii_digit())
        });
    assignment || prefix || private_key || postgres || encoded
}

fn logged_prompt(prompt: &str) -> String {
    let joined: String = prompt.lines().map(str::trim).collect();
    if prompt.lines().any(has_secret) || has_secret(&joined) {
        return "[secret removed]".into();
    }
    let mut end = prompt.len().min(4000);
    while !prompt.is_char_boundary(end) {
        end -= 1;
    }
    prompt[..end].to_owned()
}

/// Only active, unexpired items that nobody replaced may be delivered (SPEC §6.1).
pub fn deliverable(item: &Item, now: chrono::DateTime<Utc>) -> bool {
    item.status == Status::Active
        && item.superseded_by.is_none()
        && item
            .expires_at
            .as_deref()
            .is_none_or(|exp| chrono::DateTime::parse_from_rfc3339(exp).is_ok_and(|exp| exp > now))
}

/// Nearest ancestor of `cwd` that holds `.elo/`.
pub fn find_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|a| a.join(DATA_DIR).is_dir())
        .map(Path::to_path_buf)
}

fn safe_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `elo.toml` when present, otherwise the projects discovered right now (zero config).
pub(crate) fn config_for(
    root: &Path,
    found: &crate::discover::Discovery,
) -> Result<Config, String> {
    if root.join("elo.toml").is_file() {
        return Config::load(root).map_err(|e| e.to_string());
    }
    let text = format!(
        "[workspace]\nbudget_bytes = 9000\n{}",
        to_toml(&found.projects)
    );
    Config::parse(&text, root).map_err(|e| e.to_string())
}

/// Lexical `.`/`..` resolution, for paths that do not exist yet.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Workspace-relative path (relative inputs are resolved against `base`), with worktree
/// copies mapped onto their main project. Paths outside the workspace give `.`.
pub(crate) fn relative(
    root: &Path,
    base: &Path,
    path: &Path,
    worktrees: &[(String, PathBuf)],
) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let canon_root = fs::canonicalize(root).unwrap_or_else(|_| lexical(root));
    let canon = fs::canonicalize(&absolute).unwrap_or_else(|_| lexical(&absolute));
    let rel = canon
        .strip_prefix(&canon_root)
        .map(Path::to_path_buf)
        .or_else(|_| {
            lexical(&absolute)
                .strip_prefix(lexical(root))
                .map(Path::to_path_buf)
        });
    let Ok(rel) = rel else {
        return ".".into();
    };
    let mut rel = rel.to_string_lossy().replace('\\', "/");
    for (wt, main) in worktrees {
        let wt = wt.trim_end_matches('/');
        if rel == wt || rel.starts_with(&format!("{wt}/")) {
            if let Ok(main_rel) = main.strip_prefix(&canon_root) {
                let main_rel = main_rel.to_string_lossy().replace('\\', "/");
                rel = format!("{main_rel}{}", &rel[wt.len()..]);
            }
            break;
        }
    }
    if rel.is_empty() {
        ".".into()
    } else {
        rel
    }
}

/// Project discovery cached in `.elo/discovery.json` for ten minutes, so a hook event
/// never walks the workspace more than once and usually not at all.
fn discovery(root: &Path) -> crate::discover::Discovery {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Cached {
        at: i64,
        projects: Vec<(String, String)>,
        worktrees: Vec<(String, PathBuf)>,
    }
    let file = root.join(DATA_DIR).join("discovery.json");
    let now = Utc::now().timestamp();
    if let Some(c) = fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str::<Cached>(&t).ok())
        .filter(|c| now - c.at < 600)
    {
        return crate::discover::Discovery {
            projects: c
                .projects
                .into_iter()
                .map(|(name, root)| crate::discover::Discovered { name, root })
                .collect(),
            worktrees: c.worktrees,
            ..Default::default()
        };
    }
    let found = discover(root);
    let cached = Cached {
        at: now,
        projects: found
            .projects
            .iter()
            .map(|p| (p.name.clone(), p.root.clone()))
            .collect(),
        worktrees: found.worktrees.clone(),
    };
    if let Ok(text) = serde_json::to_string(&cached) {
        let _ = atomic_write(&file, &text);
    }
    found
}

/// Write to a temporary sibling, then rename: readers never see a half-written file.
fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

/// Holds an exclusive advisory lock on `path` until dropped.
fn lock(path: &Path) -> std::io::Result<fs::File> {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    f.lock()?;
    Ok(f)
}

fn log_error(root: &Path, message: &str) {
    let dir = root.join(DATA_DIR).join("shadow");
    if fs::create_dir_all(&dir).is_ok() {
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("errors.log"))
        {
            let _ = writeln!(f, "{} {message}", Utc::now().to_rfc3339());
        }
    }
}

/// Runs one hook event in shadow mode. Returns Ok(()) even when ELO is not set up here.
pub fn shadow(event_name: &str, stdin: &str) -> Result<(), String> {
    prepare(event_name, stdin).map(|_| ())
}

/// Returns context only after the complete hook succeeds; errors remain silent.
pub fn prepare(event_name: &str, stdin: &str) -> Result<Option<String>, String> {
    let input: HookInput = match serde_json::from_str(stdin) {
        Ok(input) => input,
        Err(e) => {
            if let Some(root) = std::env::current_dir().ok().as_deref().and_then(find_root) {
                log_error(&root, &format!("bad hook input: {e}"));
            }
            return Err(format!("bad hook input: {e}"));
        }
    };
    let cwd = PathBuf::from(&input.cwd);
    let Some(root) = find_root(&cwd) else {
        return Ok(None);
    };
    let result = run(&root, event_name, &input);
    if let Err(e) = &result {
        log_error(&root, e);
    }
    Ok(result.ok().flatten())
}

fn run(root: &Path, event_name: &str, input: &HookInput) -> Result<Option<String>, String> {
    let sessions = root.join(DATA_DIR).join("sessions");
    fs::create_dir_all(&sessions).map_err(|e| e.to_string())?;
    let session_file = sessions.join(format!("{}.json", safe_id(&input.session_id)));
    // One event at a time per session: read-modify-write of the session file is locked.
    let _guard = lock(&session_file.with_extension("lock")).map_err(|e| e.to_string())?;
    let found = discovery(root);
    let cwd_path = PathBuf::from(&input.cwd);
    let mut session: Session = fs::read_to_string(&session_file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    session.seq += 1;

    let mut touches = if matches!(event_name, "post-tool-use" | "pre-tool-use") {
        crate::usage::extract(
            root,
            &cwd_path,
            &input.tool_name,
            input
                .tool_input
                .as_ref()
                .unwrap_or(&serde_json::Value::Null),
        )
    } else {
        crate::usage::Touches::default()
    };
    for path in &mut touches.paths {
        *path = relative(root, root, Path::new(path), &found.worktrees);
    }
    if event_name == "post-tool-use" {
        if matches!(
            input.tool_name.as_str(),
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit"
        ) {
            if let Some(path) = input.tool_input.as_ref().and_then(|tool| {
                // A null or missing file_path falls back to notebook_path.
                tool.get("file_path")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| {
                        tool.get("notebook_path")
                            .and_then(serde_json::Value::as_str)
                    })
            }) {
                crate::events::sync_changed_file(root, &cwd_path, Path::new(path));
            }
        }
        // Automatic evidence must never prevent the hook from completing.
        let _ = crate::usage::record(
            root,
            &input.session_id,
            session.seq,
            crate::usage::DEFAULT_WINDOW,
            &touches,
        );
    }
    // The event keeps its old meaning: only the file the tool names (file_path).
    // The full list of touched paths serves only the automatic usefulness signal above.
    let touched: Vec<String> = input
        .tool_input
        .as_ref()
        .and_then(|t| t.get("file_path").and_then(|p| p.as_str()))
        .map(|p| vec![relative(root, &cwd_path, Path::new(p), &found.worktrees)])
        .unwrap_or_default();

    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let ev: Event = serde_json::from_value(serde_json::json!({
        "id": format!("{}:{}", input.session_id, session.seq),
        "event_type": event_name,
        "session_id": input.session_id,
        "family": "live",
        "event_seq": session.seq,
        "cwd": relative(root, &cwd_path, &cwd_path, &found.worktrees),
        "prompt": input.prompt,
        "tool_paths": if touched.is_empty() { serde_json::Value::Null } else { serde_json::json!(touched) },
        "now": now,
        "allowed_scopes": [], "required": [], "forbidden": []
    }))
    .map_err(|e| e.to_string())?;
    session.state.begin_event(&ev);

    let mut output = if event_name == "post-tool-use" {
        session.pending_context.take()
    } else {
        None
    };
    if event_name == "session-start"
        || (event_name == "user-prompt-submit" && !input.defer_prompt)
        || (event_name == "post-tool-use" && input.defer_prompt)
    {
        if let Some((project, block)) = crate::work::session_start_project_block(root, &cwd_path) {
            if event_name == "session-start"
                || session.shown_work_project.as_ref() != Some(&project)
            {
                output = Some(match output {
                    Some(context) => format!("{block}\n{context}"),
                    None => block,
                });
                session.shown_work_project = Some(project);
            }
        }
    }
    if event_name == "pre-tool-use" && !input.suppress_pre_context && !touches.paths.is_empty() {
        // Deliver the rules that cite the file the tool is about to touch (arXiv 2607.20972).
        if let Some(mut store) = open_for_hook(root)? {
            let mut items = load_items(&store).map_err(|e| e.to_string())?;
            items.retain(|item| deliverable(item, Utc::now()));
            let seq = i64::try_from(session.seq).unwrap_or(i64::MAX);
            let recent = recently_delivered(&store, &input.session_id, seq)?;
            let nonce = u64::from_le_bytes(
                crate::store::now_seconds()
                    .wrapping_mul(0x9e37_79b9)
                    .wrapping_add(seq)
                    .to_le_bytes(),
            );
            if let Some(cue) = crate::cue::select(&touches.paths, &items, &recent, &ev.id, nonce) {
                let ids: Vec<(&str, Mode)> =
                    cue.ids.iter().map(|i| (i.as_str(), Mode::Full)).collect();
                store
                    .record_injection(&input.session_id, seq, cue.rendered.len() as u64, &ids)
                    .map_err(|e| e.to_string())?;
                output = Some(cue.rendered);
            }
        }
    }
    if event_name == "user-prompt-submit" {
        let config = config_for(root, &found)?;
        let mut store = match Store::open_no_migrate(root) {
            Ok(store) => store,
            // Waiting for `elo doctor` to migrate, or no store yet: stay silent, log nothing.
            Err(
                crate::store::StoreError::MigrationPending { .. }
                | crate::store::StoreError::NotEloStore,
            ) => return Ok(None),
            // Another elo is writing (e.g. `elo doctor` migrating): skip this prompt quietly.
            Err(crate::store::StoreError::Sqlite(rusqlite::Error::SqliteFailure(f, _)))
                if matches!(
                    f.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                return Ok(None)
            }
            Err(e) => return Err(e.to_string()),
        };
        let mut items = load_items(&store).map_err(|e| e.to_string())?;
        items.retain(|item| deliverable(item, Utc::now()));
        let fx = live_fixture(root, config, items);
        let selection = E1.select(&fx, &ev, &mut session.state);
        let rendered = render(&fx, &ev, &selection);
        let ids: Vec<(&str, Mode)> = selection
            .full_ids
            .iter()
            .map(|i| (i.as_str(), Mode::Full))
            .chain(
                selection
                    .pointer_ids
                    .iter()
                    .map(|i| (i.as_str(), Mode::Pointer)),
            )
            .collect();
        let seq = i64::try_from(session.seq).unwrap_or(i64::MAX);
        store
            .record_injection(&input.session_id, seq, selection.used_bytes as u64, &ids)
            .map_err(|e| e.to_string())?;
        let shadow = root.join(DATA_DIR).join("shadow");
        fs::create_dir_all(&shadow).map_err(|e| e.to_string())?;
        let record = serde_json::json!({
            "at": now, "session_id": input.session_id, "event_seq": session.seq,
            "prompt": input.prompt.as_deref().map(logged_prompt),
            "cwd": ev.cwd, "full_ids": selection.full_ids, "pointer_ids": selection.pointer_ids,
            "used_bytes": selection.used_bytes, "rendered": rendered,
        });
        // Lines can exceed PIPE_BUF: appends from parallel sessions go through a lock.
        let _log_guard = lock(&shadow.join("log.lock")).map_err(|e| e.to_string())?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(shadow.join("log.jsonl"))
            .map_err(|e| e.to_string())?;
        writeln!(f, "{record}").map_err(|e| e.to_string())?;
        session.state.record(&selection);
        if !rendered.is_empty() {
            output = Some(match output {
                Some(block) => format!("{block}\n{rendered}"),
                None => rendered,
            });
        }
    }
    if event_name == "user-prompt-submit" && input.defer_prompt {
        session.pending_context = output.take();
    }
    let text = serde_json::to_string(&session).map_err(|e| e.to_string())?;
    atomic_write(&session_file, &text).map_err(|e| e.to_string())?;
    Ok(output)
}

/// Opens the store for a hook: `None` (silence, nothing logged) while a migration is
/// pending, without a store, or while another elo holds the write lock.
fn open_for_hook(root: &Path) -> Result<Option<Store>, String> {
    match Store::open_no_migrate(root) {
        Ok(store) => Ok(Some(store)),
        Err(
            crate::store::StoreError::MigrationPending { .. }
            | crate::store::StoreError::NotEloStore,
        ) => Ok(None),
        Err(crate::store::StoreError::Sqlite(rusqlite::Error::SqliteFailure(f, _)))
            if matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Items delivered in full in this session in the last `cue::DEDUP_WINDOW` events.
fn recently_delivered(
    store: &Store,
    session: &str,
    seq: i64,
) -> Result<std::collections::BTreeSet<String>, String> {
    let lower = seq - i64::try_from(crate::cue::DEDUP_WINDOW).unwrap_or(i64::MAX);
    let mut stmt = store
        .conn
        .prepare(
            "SELECT DISTINCT item_id FROM injections \
             WHERE session_id = ?1 AND event_seq >= ?2 AND event_seq < ?3 AND mode = 'full'",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params![session, lower, seq], |r| {
            r.get::<_, String>(0)
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::logged_prompt;

    #[test]
    fn prompt_cut_is_bytes_and_utf8_safe() {
        let prompt = format!("{}🦀fim", "á ".repeat(1333));
        let logged = logged_prompt(&prompt);
        assert_eq!(logged.len(), 3999);
        assert!(prompt.starts_with(&logged));
        assert_eq!(logged_prompt("pedido normal"), "pedido normal");
    }

    fn assert_removed(secret: &str) {
        for prompt in [
            format!("pedido {secret} fim"),
            format!("{}{secret}", "á ".repeat(2000)),
        ] {
            assert_eq!(
                logged_prompt(&prompt),
                "[secret removed]",
                "caso: {secret:?}"
            );
        }
    }

    #[test]
    fn secret_assignments_accept_case_spacing_and_quotes() {
        for key in [
            "token",
            "senha",
            "password",
            "passwd",
            "secret",
            "api_key",
            "apikey",
            "authorization",
        ] {
            for key in [key.to_owned(), key.to_uppercase()] {
                for sep in [":", "=", " : ", " = ", "\t=\t", ":\n"] {
                    assert_removed(&format!("{key}{sep}abc"));
                    assert_removed(&format!("\"{key}\"{sep}\"abc\""));
                }
            }
        }
    }

    #[test]
    fn known_short_secrets_are_removed_before_cut() {
        for secret in [
            "Bearer abc",
            "sk-curta",
            "sk_live_abc",
            "sk_test_abc",
            "rk_abc",
            "pk_live_abc",
            "AKIA1234567890123456",
            "ASIA1234567890123456",
            "xoxb-abc",
            "ghp_abc",
            "gho_abc",
            "github_pat_abc",
            "eyJabc.def",
            "AIzaabc",
            "-----BEGIN PRIVATE KEY-----\nabc",
            "BEGIN RSA PRIVATE KEY",
            "BEGIN EC PRIVATE KEY",
            "postgres://user:abc@host/db",
        ] {
            assert_removed(secret);
            assert_removed(&secret.to_uppercase());
            assert_removed(&secret.to_lowercase());
        }
    }

    #[test]
    fn mixed_values_and_unmarked_multiline_values_are_removed() {
        for secret in [
            "YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4",
            "YWJjZGVmZ2hpamts\nbW5vcHFyc3R1dnd4",
            "YWJjZGVmZ2hpamts\r\n  bW5vcHFyc3R1dnd4",
            "to\nken = abc",
            "Bea\nrer abc",
        ] {
            assert_removed(secret);
        }
        assert_removed(&format!("Aa0{}", "z".repeat(29)));
    }

    #[test]
    fn commits_uuids_and_ordinary_identifiers_survive() {
        for value in [
            "0123456789abcdef0123456789abcdef01234567",
            "0123456789aBcDeF".repeat(4).as_str(),
            "123e4567-e89b-12d3-a456-426614174000",
            "123E4567-e89B-12d3-A456-426614174000",
            "CustomAuthenticationFailureHandlerProvider",
            "abcdefghijklmnopqrstuvwxyz0123456789",
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
            "postgres://user@host/db",
            "postgres://user:@host/db",
            "documentar senha e password",
        ] {
            let prompt = format!("investigue {value}");
            assert_eq!(logged_prompt(&prompt), prompt);
        }
        let short = format!("Aa0{}", "z".repeat(28));
        assert_eq!(logged_prompt(&short), short);
    }
}
