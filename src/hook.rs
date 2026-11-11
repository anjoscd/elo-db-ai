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
    #[serde(default)]
    transcript_path: Option<String>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Session {
    seq: u64,
    state: SessionState,
    #[serde(default)]
    shown_work_project: Option<String>,
    #[serde(default)]
    pending_context: Option<String>,
    /// event_seq of the last real prompt: its deliveries are scored against the answer.
    #[serde(default)]
    last_prompt_seq: u64,
}

/// Bytes read from the end of the transcript: enough for the last answer, cheap on the hook.
const TRANSCRIPT_TAIL: u64 = 512 * 1024;

/// Visible text of the last answer (Claude Code transcript JSONL): assistant `text` blocks
/// after the previous user prompt. Thinking and tool calls are not the answer. Any error
/// gives an empty string; a transcript in another format simply yields nothing.
fn last_answer(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TRANSCRIPT_TAIL);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // first line may be cut in the middle
    }
    let mut parts: Vec<String> = Vec::new();
    let mut seen_answer = false;
    let mut skipped_new_prompt = false;
    for line in lines.iter().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let content = value.pointer("/message/content");
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("assistant") => {
                seen_answer = true;
                // Walking backwards: blocks of one message are taken last to first too.
                if let Some(t) = content.and_then(serde_json::Value::as_str) {
                    parts.push(t.to_string());
                }
                for block in content
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .rev()
                {
                    if block.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                        if let Some(t) = block.get("text").and_then(serde_json::Value::as_str) {
                            parts.push(t.to_string());
                        }
                    }
                }
            }
            Some("user") => {
                let is_prompt = content.is_some_and(|c| {
                    c.is_string()
                        || c.as_array().is_some_and(|a| {
                            a.iter().any(|b| {
                                b.get("type").and_then(serde_json::Value::as_str) == Some("text")
                            })
                        })
                });
                // The new prompt may already be written (no answer after it yet): skip it.
                // Any other prompt ends the answer, even one with no visible text, so an
                // older turn is never taken for the last one.
                if is_prompt {
                    if seen_answer || skipped_new_prompt {
                        break;
                    }
                    skipped_new_prompt = true;
                }
            }
            _ => {}
        }
    }
    parts.reverse();
    parts.join("\n")
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

pub(crate) fn safe_id(id: &str) -> String {
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

/// Prompts that come from the agent runtime (subagent messages, system reminders,
/// notifications, etc.) carry many unrelated keywords and must not trigger rules.
fn is_automatic_prompt(prompt: &str) -> bool {
    let trimmed = prompt.trim_start();
    [
        "<agent-message",
        "<task-notification",
        "<system-reminder",
        "[SYSTEM NOTIFICATION",
        "Another Claude session sent a message",
        "<local-command",
        "<command-name",
    ]
    .iter()
    .any(|marker| trimmed.starts_with(marker))
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
                // Event and masked shape only (letters and digits become 'x'): never the content.
                let shape: String = stdin
                    .chars()
                    .take(24)
                    .map(|c| if c.is_alphanumeric() { 'x' } else { c })
                    .collect();
                log_error(
                    &root,
                    &format!(
                        "bad hook input ({event_name}, {} bytes, shape {shape:?}): {e}",
                        stdin.len()
                    ),
                );
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
    let file_path = input
        .tool_input
        .as_ref()
        .and_then(|t| t.get("file_path").and_then(|p| p.as_str()));
    if event_name == "pre-tool-use" && touches.paths.is_empty() && file_path.is_none() {
        let text = serde_json::to_string(&session).map_err(|e| e.to_string())?;
        atomic_write(&session_file, &text).map_err(|e| e.to_string())?;
        return Ok(None);
    }
    let found = discovery(root);
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
    let touched: Vec<String> = file_path
        .map(|p| vec![relative(root, &cwd_path, Path::new(p), &found.worktrees)])
        .unwrap_or_default();

    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let automatic = event_name == "user-prompt-submit"
        && input.prompt.as_deref().is_some_and(is_automatic_prompt);
    if event_name == "user-prompt-submit" && !automatic {
        // Laws and decisions rarely name a file: the answer to the previous prompt is the
        // evidence. Automatic evidence must never prevent the hook from completing.
        if let (Some(path), true) = (&input.transcript_path, session.last_prompt_seq > 0) {
            let answer = last_answer(Path::new(path));
            let window = session.seq.saturating_sub(session.last_prompt_seq);
            let _ = crate::usage::record_response(
                root,
                &input.session_id,
                session.seq,
                window,
                &answer,
            );
        }
        session.last_prompt_seq = session.seq;
    }
    let mut ev: Event = serde_json::from_value(serde_json::json!({
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
    if automatic {
        ev.prompt = None;
    }
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
    if event_name == "session-start" {
        if let (Ok(config), Some(store)) =
            (config_for(root, &found), open_for_hook(root).ok().flatten())
        {
            if config.maintain.auto_invite {
                let now = crate::store::now_seconds();
                if let Some(line) = crate::maintain::maybe_invite(&store, now).ok().flatten() {
                    output = Some(match output {
                        Some(block) => format!("{block}\n{line}"),
                        None => line,
                    });
                }
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
        if config.workspace.demote_unused_min > 0 {
            let mut windows = store.unused_windows(
                crate::store::now_seconds(),
                config.workspace.demote_unused_days,
            );
            for item in &mut items {
                item.usage_window = windows.remove(&item.id).unwrap_or_default();
            }
        }
        let fx = live_fixture(root, config.clone(), items);
        let mut selection = E1.select(&fx, &ev, &mut session.state);
        let seq = i64::try_from(session.seq).unwrap_or(i64::MAX);
        if session.seq > 0
            && config.feedback.ask_every > 0
            && session.seq.is_multiple_of(config.feedback.ask_every)
        {
            let prior = prior_session_deliveries(&store, &input.session_id, seq, 40)?;
            if !prior.is_empty() {
                let mut delivered: Vec<String> = Vec::new();
                for id in selection
                    .full_ids
                    .iter()
                    .chain(&selection.pointer_ids)
                    .chain(&prior)
                {
                    if !delivered.contains(id) {
                        delivered.push(id.clone());
                        if delivered.len() == 20 {
                            break;
                        }
                    }
                }
                // The session id comes from the harness and is echoed into the envelope: only an
                // opaque token (ASCII letters, digits, '-', '_', '.', ':' up to 128 chars) is
                // allowed, so a hostile id can never break the line or close the envelope.
                let sid_ok = !input.session_id.is_empty()
                    && input.session_id.len() <= 128
                    && input
                        .session_id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
                if !delivered.is_empty() && sid_ok {
                    let ids_text = delivered.join(", ");
                    let sid = &input.session_id;
                    selection.feedback_line = Some(format!(
                        "Call elo_feedback(session_id:\"{sid}\", used:[...], ignored:[...]) for these delivered ids: [{ids_text}]."
                    ));
                }
            }
        }
        let mut rendered = render(&fx, &ev, &selection);
        if selection.feedback_line.is_some()
            && rendered.len() > config.workspace.budget_bytes as usize
        {
            selection.feedback_line = None;
            rendered = render(&fx, &ev, &selection);
        }
        selection.used_bytes = rendered.len();
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
        store
            .record_injection(&input.session_id, seq, selection.used_bytes as u64, &ids)
            .map_err(|e| e.to_string())?;
        crate::shadow::record(&crate::store::data_dir(root), &fx, &ev, &selection);
        let shadow = root.join(DATA_DIR).join("shadow");
        fs::create_dir_all(&shadow).map_err(|e| e.to_string())?;
        let record = serde_json::json!({
            "at": now, "session_id": input.session_id, "event_seq": session.seq,
            "prompt": input.prompt.as_deref().map(logged_prompt),
            "automatic": automatic, "cwd": ev.cwd, "full_ids": selection.full_ids,
            "pointer_ids": selection.pointer_ids,
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
        session.state.record(&ev, &selection);
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

/// Items delivered earlier in this session within the `window`-event feedback window (`[seq - (window - 1), seq - 1]`).
fn prior_session_deliveries(
    store: &Store,
    session: &str,
    seq: i64,
    window: u64,
) -> Result<Vec<String>, String> {
    let lower = seq.saturating_sub(i64::try_from(window.saturating_sub(1)).unwrap_or(i64::MAX));
    let mut stmt = store
        .conn
        .prepare(
            "SELECT item_id, MAX(event_seq) AS last_seq FROM injections \
             WHERE session_id = ?1 AND event_seq >= ?2 AND event_seq < ?3 \
             GROUP BY item_id ORDER BY last_seq DESC, item_id",
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
    use super::{last_answer, logged_prompt};
    use std::fs;

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

    #[test]
    fn last_answer_takes_only_visible_text_after_the_previous_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"first prompt"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"old answer"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"second prompt"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"secret plan"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"part one"}]}}"#,
            "not json",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"part two"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"new prompt already written"}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        assert_eq!(last_answer(&path), "part one\npart two");
        assert_eq!(last_answer(&dir.path().join("missing.jsonl")), "");
        let only_tools = [
            r#"{"type":"user","message":{"role":"user","content":"old prompt"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"old answer"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"previous prompt"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"new prompt"}}"#,
        ];
        fs::write(&path, only_tools.join("\n")).unwrap();
        assert_eq!(last_answer(&path), "");
        let blocks = [
            r#"{"type":"user","message":{"role":"user","content":"p"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#,
            r#"{"type":"assistant","message":{"content":"c"}}"#,
        ];
        fs::write(&path, blocks.join("\n")).unwrap();
        assert_eq!(last_answer(&path), "a\nb\nc");
    }
}
