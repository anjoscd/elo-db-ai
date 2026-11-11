//! Local replay with no persistence and aggregate-only output.
use std::{collections::BTreeMap, io::BufRead, io::Write as _, path::Path};

use serde::Deserialize;

use super::{
    elo_selector::E1,
    fixture::Event,
    selector::{Selector, SessionState},
};
use crate::{hook, live, store::Store};

#[derive(Deserialize)]
struct Record {
    at: String,
    cwd: String,
    event_seq: u64,
    session_id: String,
    prompt: Option<String>,
    tool_paths: Option<Vec<String>>,
    #[serde(default = "prompt_event")]
    event_type: String,
}

fn prompt_event() -> String {
    "user-prompt-submit".into()
}

pub fn run(log: &Path) -> Result<String, String> {
    let log = std::fs::canonicalize(log).map_err(|_| "could not open the log")?;
    // The log lives in <workspace>/.elo/shadow/: start the search at the folder that owns
    // `.elo`, so it never lands in a nested `.elo` with another (empty) store.
    let start = log
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == ".elo"))
        .and_then(Path::parent)
        .or_else(|| log.parent())
        .ok_or("log has no folder")?;
    let root = hook::find_root(start).ok_or("log workspace not found")?;
    let found = crate::discover::discover(&root);
    let config = hook::config_for(&root, &found).map_err(|_| "invalid configuration")?;
    let store = Store::open_read_only(&root).map_err(|_| "could not open the store for reading")?;
    let items = live::load_items(&store).map_err(|_| "could not load the items")?;
    let fx = live::live_fixture(&root, config, items);
    let file = std::fs::File::open(log).map_err(|_| "could not open the log")?;
    aggregate(&fx, std::io::BufReader::new(file), &found.worktrees)
}

pub fn aggregate(
    fx: &super::Fixture,
    reader: impl BufRead,
    worktrees: &[(String, std::path::PathBuf)],
) -> Result<String, String> {
    let mut sessions: BTreeMap<String, SessionState> = BTreeMap::new();
    let mut bytes = Vec::new();
    let (mut full_items, mut empty, mut invalid, mut missing) = (0usize, 0usize, 0usize, 0usize);
    let mut empty_output = 0;
    let mut records = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|_| "failed to read the log")?;
        let record = match serde_json::from_str::<Record>(&line) {
            Ok(r) if chrono::DateTime::parse_from_rfc3339(&r.at).is_ok() => r,
            _ => {
                invalid += 1;
                continue;
            }
        };
        records.push(record);
    }
    records.sort_by(|a, b| (&a.session_id, a.event_seq).cmp(&(&b.session_id, b.event_seq)));
    // ELO_REPLAY_DUMP=<file>: write each prompt and its rendered context (never the store).
    let mut dump = std::env::var_os("ELO_REPLAY_DUMP")
        .and_then(|path| std::fs::File::create(path).ok())
        .map(std::io::BufWriter::new);
    for record in records {
        let cwd = hook::relative(
            &fx.code_root,
            &fx.code_root,
            Path::new(&record.cwd),
            worktrees,
        );
        let ev = Event {
            id: format!("{}:{}", record.session_id, record.event_seq),
            event_type: record.event_type,
            session_id: record.session_id,
            family: "replay".into(),
            event_seq: record.event_seq,
            cwd,
            prompt: record.prompt,
            tool_paths: record.tool_paths,
            now: record.at,
            allowed_scopes: vec![],
            required: vec![],
            forbidden: vec![],
        };
        let state = sessions.entry(ev.session_id.clone()).or_default();
        state.begin_event(&ev);
        if ev.event_type != "user-prompt-submit" {
            continue;
        }
        missing += usize::from(ev.prompt.is_none());
        let selection = E1.select(fx, &ev, state);
        state.record(&ev, &selection);
        if let Some(out) = dump.as_mut() {
            // Evaluation only (blind tests): what the selector would deliver for this prompt.
            let line = serde_json::json!({
                "session_id": ev.session_id, "event_seq": ev.event_seq, "at": ev.now,
                "prompt": ev.prompt, "rendered": super::selector::render(fx, &ev, &selection),
            });
            let _ = writeln!(out, "{line}");
        }
        full_items += selection.full_ids.len();
        empty += usize::from(selection.full_ids.is_empty() && selection.pointer_ids.is_empty());
        empty_output += usize::from(selection.used_bytes == 0);
        bytes.push(selection.used_bytes);
    }
    bytes.sort_unstable();
    let percentile = |p: usize| {
        bytes
            .get((bytes.len() * p).div_ceil(100).saturating_sub(1))
            .copied()
            .unwrap_or(0)
    };
    let prompt_events = bytes.len();
    let mean = if prompt_events == 0 {
        0.0
    } else {
        full_items as f64 / prompt_events as f64
    };
    Ok(format!("events {}\nbytes p50/p95/total {} / {} / {}\nfull items per event (mean) {mean:.2}\nevents with 0 items {empty}\nevents with empty output {empty_output}\nevents without request {missing}\ninvalid lines {invalid}\n", prompt_events, percentile(50), percentile(95), bytes.iter().sum::<usize>()))
}
