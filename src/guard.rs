//! Guards: owner laws turned into checks that run before a tool call (PreToolUse).
//!
//! A guard matches a tool call by tool name and patterns in one input field.
//! Commands use cleaned shell segments anchored at the program; other fields use substrings. In shadow mode (the default) a match is only recorded under
//! `.elo/shadow/guards.jsonl`; with `enforce = true` the hook answers `deny` or `ask`.
//! Any error makes the hook allow the call: a guard must never break the session.
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::store::DATA_DIR;

/// One `[[guard]]` entry of `elo.toml`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Guard {
    /// Stable name, shown in the reason and in the log.
    pub id: String,
    /// Item id of the law this guard enforces (for `elo explain` and the message).
    pub law: String,
    /// Tool name (`Bash`, `Agent`, `Edit`, ...) or `*` for any tool.
    pub tool: String,
    /// Field of the tool input to inspect (`command`, `file_path`, `prompt`, ...);
    /// `*` inspects the whole input as JSON.
    pub field: String,
    /// Every substring must appear; for command, the first anchors the segment.
    pub all: Vec<String>,
    /// At least one substring must appear (ignored when empty).
    /// For command without all, each alternative anchors the segment.
    pub any: Vec<String>,
    /// No substring may appear.
    pub none: Vec<String>,
    /// `deny` or `ask`.
    pub action: String,
    /// Message shown to the agent (and to the owner on `ask`).
    pub message: String,
    /// Only enforced guards answer the hook; the others are recorded (shadow).
    pub enforce: bool,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_input: Value,
}

/// Text of the inspected field: strings as they are, other values as JSON.
fn field_text(input: &Value, field: &str) -> Option<String> {
    if field.is_empty() || field == "*" {
        return Some(input.to_string());
    }
    match input.get(field)? {
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

// This is a lexical filter, not a shell interpreter: quoted text (including
// quoted substitutions) is deliberately opaque except shell -c arguments. Never execute or expand input.
fn heredoc_marker(chars: &[char], mut i: usize) -> (usize, String, bool) {
    i += 2;
    let tabs = chars.get(i) == Some(&'-');
    if tabs {
        i += 1;
    }
    while chars.get(i).is_some_and(|c| matches!(c, ' ' | '\t')) {
        i += 1;
    }
    let mut marker = String::new();
    while i < chars.len()
        && !chars[i].is_whitespace()
        && !matches!(chars[i], ';' | '&' | '|' | '(' | ')' | '<' | '>')
    {
        if matches!(chars[i], '\'' | '"') {
            let quote = chars[i];
            i += 1;
            while i < chars.len() && chars[i] != quote {
                if chars[i] == '\\' && quote == '"' && i + 1 < chars.len() {
                    i += 1;
                }
                marker.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
        } else {
            if chars[i] == '\\' && i + 1 < chars.len() {
                i += 1;
            }
            marker.push(chars[i]);
            i += 1;
        }
    }
    (i, marker, tabs)
}

fn skip_heredocs(chars: &[char], mut i: usize, heredocs: &mut Vec<(String, bool)>) -> usize {
    for (marker, tabs) in heredocs.drain(..) {
        while i < chars.len() {
            let start = i;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            let line: String = chars[start..i].iter().collect();
            if i < chars.len() {
                i += 1;
            }
            let line = if tabs {
                line.trim_start_matches('\t')
            } else {
                &line
            };
            if line == marker {
                break;
            }
        }
    }
    i
}

fn quoted_end(chars: &[char], start: usize) -> usize {
    let mut stack = vec![chars[start]];
    let mut heredocs = Vec::new();
    let mut i = start + 1;
    while i < chars.len() && !stack.is_empty() {
        let c = chars[i];
        let mode = *stack.last().unwrap();
        if mode == ')' && c == '<' && chars.get(i + 1) == Some(&'<') {
            if chars.get(i + 2) == Some(&'<') {
                i += 3;
            } else {
                let (end, marker, tabs) = heredoc_marker(chars, i);
                heredocs.push((marker, tabs));
                i = end;
            }
            continue;
        }
        if mode == ')' && c == '\n' && !heredocs.is_empty() {
            i = skip_heredocs(chars, i + 1, &mut heredocs);
            continue;
        }
        if c == '\\' && mode != '\'' {
            i += 2;
            continue;
        }
        if mode == '\'' {
            if c == '\'' {
                stack.pop();
            }
        } else if mode == '"' {
            if c == '"' {
                stack.pop();
            } else if c == '$' && chars.get(i + 1) == Some(&'(') {
                stack.push(')');
                i += 1;
            }
        } else if matches!(c, '\'' | '"') {
            stack.push(c);
        } else if c == '(' {
            stack.push(')');
        } else if c == ')' {
            stack.pop();
        }
        i += 1;
    }
    i.min(chars.len())
}

const MAX_COMMAND_BYTES: usize = 256 * 1024;

fn shell_segments(command: &str) -> Vec<String> {
    let mut end = command.len().min(MAX_COMMAND_BYTES);
    while !command.is_char_boundary(end) {
        end -= 1;
    }
    shell_segments_at(&command[..end], 0)
}

fn shell_segments_at(command: &str, depth: usize) -> Vec<String> {
    let mut quotes = Vec::new();
    let mut nested = Vec::new();
    let chars: Vec<char> = command.chars().collect();
    let mut cleaned = String::with_capacity(command.len());
    let mut heredocs = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                if let Some(&next) = chars.get(i + 1) {
                    if next != '\n' {
                        // Escaped operators are literal arguments, not separators.
                        cleaned.push(next);
                    }
                    i += 2;
                    continue;
                }
            }
            '\'' | '"' => {
                let end = quoted_end(&chars, i);
                let stop = if chars.get(end.wrapping_sub(1)) == Some(&c) {
                    end - 1
                } else {
                    end
                };
                let mut text = String::new();
                let mut j = i + 1;
                while j < stop {
                    if c == '"'
                        && chars[j] == '\\'
                        && j + 1 < stop
                        && matches!(chars[j + 1], '"' | '\\' | '$' | '`' | '\n')
                    {
                        j += 1;
                        if chars[j] == '\n' {
                            j += 1;
                            continue;
                        }
                    }
                    text.push(chars[j]);
                    j += 1;
                }
                quotes.push(text);
                i = end;
                // Keep an opaque word so `echo "" git push` cannot promote git.
                cleaned.push_str(&format!("\0{}\0", quotes.len() - 1));
                continue;
            }
            '#' if cleaned.ends_with(char::is_whitespace) || cleaned.is_empty() => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) == Some(&'<') => {
                cleaned.push_str(" <<< ");
                i += 3;
                continue;
            }
            '<' if chars.get(i + 1) == Some(&'<') => {
                let (end, marker, tabs) = heredoc_marker(&chars, i);
                i = end;
                heredocs.push((marker, tabs));
                cleaned.push_str(" \0 ");
                continue;
            }
            '\n' => {
                cleaned.push('\n');
                i += 1;
                i = skip_heredocs(&chars, i, &mut heredocs);
                continue;
            }
            ';' | '&' | '|' | '(' | ')' | '`' => {
                cleaned.push('\n');
                i += 1;
                continue;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                cleaned.push('\n');
                i += 2;
                continue;
            }
            _ => {}
        }
        cleaned.push(c);
        i += 1;
    }
    let mut segments: Vec<String> = cleaned
        .lines()
        .filter_map(|s| normalize_segment(s, &quotes, depth, &mut nested))
        .collect();
    segments.extend(nested);
    segments
}

fn assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn normalize_segment(
    segment: &str,
    quotes: &[String],
    depth: usize,
    nested: &mut Vec<String>,
) -> Option<String> {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let mut i = 0;
    while let Some(word) = words.get(i) {
        if matches!(
            *word,
            "if" | "then"
                | "else"
                | "elif"
                | "fi"
                | "do"
                | "done"
                | "while"
                | "until"
                | "for"
                | "!"
                | "{"
                | "}"
        ) || assignment(word)
        {
            i += 1;
            continue;
        }
        if *word == "case" {
            // The selector and first pattern are data; ')' starts the arm command.
            return None;
        }
        let program = word.rsplit('/').next()?;
        let wrapper_start = i;
        if !matches!(
            program,
            "bash"
                | "sh"
                | "zsh"
                | "env"
                | "nohup"
                | "exec"
                | "command"
                | "time"
                | "npx"
                | "sudo"
                | "timeout"
                | "xargs"
        ) {
            let mut normalized = program.to_string();
            for arg in &words[i + 1..] {
                normalized.push(' ');
                if arg.contains('\0') {
                    normalized.push('\0');
                } else {
                    normalized.push_str(arg);
                }
            }
            return Some(normalized);
        }
        i += 1;
        // Skip wrapper flags; only known options consume a separate operand.
        while let Some(option) = words.get(i).copied().filter(|s| s.starts_with('-')) {
            i += 1;
            if matches!(program, "bash" | "sh" | "zsh")
                && option.starts_with('-')
                && !option.starts_with("--")
                && option.contains('c')
            {
                if depth < 2 {
                    if let Some(arg) = words.get(i) {
                        let quoted = arg
                            .strip_prefix('\0')
                            .and_then(|s| s.strip_suffix('\0'))
                            .and_then(|s| s.parse::<usize>().ok())
                            .and_then(|n| quotes.get(n));
                        nested.extend(shell_segments_at(
                            quoted.map_or(*arg, String::as_str),
                            depth + 1,
                        ));
                    }
                }
                return None;
            }
            if option == "--" {
                break;
            }
            let operand = match program {
                "sudo" => matches!(
                    option,
                    "-u" | "-g" | "-h" | "-p" | "-C" | "-T" | "--user" | "--group" | "--host"
                ),
                "timeout" => matches!(option, "-k" | "-s" | "--kill-after" | "--signal"),
                "env" => matches!(option, "-u" | "--unset" | "-C" | "--chdir"),
                "xargs" => matches!(option, "-I" | "-n" | "-P" | "-L" | "-s" | "-E" | "-d"),
                "npx" => matches!(option, "-p" | "--package" | "-c" | "--call"),
                "time" => matches!(option, "-f" | "-o" | "--format" | "--output"),
                "exec" => option == "-a",
                // `bash -o pipefail -c '...'`: these options take a separate operand.
                "bash" | "sh" | "zsh" => {
                    matches!(option, "-o" | "-O" | "--rcfile" | "--init-file")
                }
                _ => false,
            };
            if operand {
                i += 1;
            }
        }
        if program == "timeout" {
            let Some(duration) = words.get(i) else {
                return Some(format!(
                    "{program}{}",
                    if words.len() > wrapper_start + 1 {
                        format!(" {}", words[wrapper_start + 1..].join(" "))
                    } else {
                        String::new()
                    }
                ));
            };
            let number = duration.trim_end_matches(['s', 'm', 'h', 'd']);
            if number.parse::<f64>().is_err() {
                return None;
            }
            i += 1;
        }
        if matches!(program, "env" | "timeout")
            && words
                .get(i..)
                .is_none_or(|rest| rest.iter().all(|w| assignment(w)))
        {
            // No child program: preserve the wrapper itself.
            return Some(
                std::iter::once(program)
                    .chain(words[wrapper_start + 1..].iter().copied())
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
    }
    None
}

fn command_anchor(segment: &str, pattern: &str) -> bool {
    if pattern == "env" {
        segment.split_whitespace().next() == Some("env")
    } else {
        segment.starts_with(pattern)
    }
}

impl Guard {
    /// True when this guard applies to the call.
    pub fn matches(&self, tool: &str, input: &Value) -> bool {
        let text = field_text(input, &self.field);
        let segments = if self.field == "command" {
            text.as_deref().map(shell_segments)
        } else {
            None
        };
        self.matches_prepared(tool, text.as_deref(), segments.as_deref())
    }

    fn matches_prepared(
        &self,
        tool: &str,
        text: Option<&str>,
        segments: Option<&[String]>,
    ) -> bool {
        if self.tool != "*" && self.tool != tool {
            return false;
        }
        let Some(text) = text else {
            return false;
        };
        if self.field == "command" {
            return segments.unwrap_or_default().iter().any(|segment| {
                let anchored = if let Some(first) = self.all.first() {
                    command_anchor(segment, first)
                } else {
                    self.any.iter().any(|s| command_anchor(segment, s))
                };
                anchored
                    && self
                        .all
                        .iter()
                        .skip(1)
                        .all(|s| segment.contains(s.as_str()))
                    && (self.all.is_empty()
                        || self.any.is_empty()
                        || self.any.iter().any(|s| segment.contains(s.as_str())))
                    && !self.none.iter().any(|s| segment.contains(s.as_str()))
            });
        }
        self.all.iter().all(|s| text.contains(s.as_str()))
            && (self.any.is_empty() || self.any.iter().any(|s| text.contains(s.as_str())))
            && !self.none.iter().any(|s| text.contains(s.as_str()))
    }

    fn decision(&self) -> &'static str {
        if self.action == "ask" {
            "ask"
        } else {
            "deny"
        }
    }

    fn reason(&self) -> String {
        let law = if self.law.is_empty() {
            String::new()
        } else {
            format!(" (law {})", self.law)
        };
        format!("ELO guard {}{law}: {}", self.id, self.message)
    }
}

/// Problems that make a guard useless; reported by `elo doctor`-style checks and tests.
pub fn validate(guards: &[Guard]) -> Vec<String> {
    let mut problems = Vec::new();
    for (n, g) in guards.iter().enumerate() {
        let name = if g.id.is_empty() {
            format!("guard #{}", n + 1)
        } else {
            g.id.clone()
        };
        if g.id.is_empty() {
            problems.push(format!("{name}: missing id"));
        }
        if g.field.trim().is_empty() {
            problems.push(format!("{name}: missing field"));
        }
        if g.tool.is_empty() {
            problems.push(format!("{name}: missing tool"));
        }
        if g.all.is_empty() && g.any.is_empty() {
            problems.push(format!(
                "{name}: needs all or any (otherwise it matches everything)"
            ));
        }
        if !matches!(g.action.as_str(), "deny" | "ask") {
            problems.push(format!("{name}: action must be deny or ask"));
        }
        if g.message.trim().is_empty() {
            problems.push(format!("{name}: missing message"));
        }
    }
    problems
}

/// Evaluates the guards of `config` for one PreToolUse input. Returns the hook's JSON
/// answer when an enforced guard matches; records every match under `root`.
pub fn evaluate(root: &Path, config: &Config, stdin: &str) -> Option<String> {
    let call: ToolCall = serde_json::from_str(stdin).ok()?;
    let valid = validate(&config.guards).is_empty();
    // Serializing a large Write/Edit input costs; do it only when a guard matches.
    let mut input_bytes: Option<usize> = None;
    let mut fields = HashMap::new();
    for guard in &config.guards {
        fields
            .entry(guard.field.as_str())
            .or_insert_with(|| field_text(&call.tool_input, &guard.field));
    }
    let segments = fields
        .get("command")
        .and_then(|s| s.as_deref())
        .map(shell_segments);
    let mut answer: Option<&Guard> = None;
    for guard in &config.guards {
        if !guard.matches_prepared(
            &call.tool_name,
            fields[guard.field.as_str()].as_deref(),
            segments.as_deref(),
        ) {
            continue;
        }
        let enforced = valid && guard.enforce;
        let bytes = *input_bytes.get_or_insert_with(|| call.tool_input.to_string().len());
        record(root, &call, guard, enforced, bytes);
        if enforced {
            // `deny` wins over `ask` when several enforced guards match.
            let stronger =
                answer.is_none_or(|a| a.decision() == "ask" && guard.decision() == "deny");
            if stronger {
                answer = Some(guard);
            }
        }
    }
    let guard = answer?;
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": guard.decision(),
                "permissionDecisionReason": guard.reason(),
            }
        })
        .to_string(),
    )
}

/// Appends one line per match. Only ids and sizes: the tool input may hold secrets.
fn record(root: &Path, call: &ToolCall, guard: &Guard, enforced: bool, input_bytes: usize) {
    let dir = root.join(DATA_DIR).join("shadow");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let line = json!({
        "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "session_id": call.session_id,
        "cwd_tail": Path::new(&call.cwd).file_name().and_then(|s| s.to_str()).unwrap_or(""),
        "tool": call.tool_name,
        "guard": guard.id,
        "law": guard.law,
        "action": guard.action,
        "enforced": enforced,
        "input_bytes": input_bytes,
    });
    let path = dir.join("guards.jsonl");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 * 1024 * 1024)
        && std::fs::rename(&path, dir.join("guards.jsonl.1")).is_err()
    {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

/// Entry point of `elo hook pre-tool-use`: finds the workspace from the input's `cwd`,
/// loads `elo.toml` and evaluates its guards. `None` = allow (also on any error).
pub fn hook(stdin: &str) -> Option<String> {
    let call: ToolCall = serde_json::from_str(stdin).ok()?;
    let root = crate::hook::find_root(Path::new(&call.cwd))?;
    if !root.join("elo.toml").is_file() {
        return None;
    }
    let config = Config::load(&root).ok()?;
    if config.guards.is_empty() {
        return None;
    }
    evaluate(&root, &config, stdin)
}
