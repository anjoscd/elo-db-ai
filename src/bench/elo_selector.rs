//! E1: ELO's scoped selector (SPEC §3–§4), first version.
//! Scope gate first (projects, links, eligibility), then score, laws, quotas, pointers.
use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use super::fixture::{Event, Fixture, Item, Kind, Status};
use super::selector::{render, render_content, Selection, Selector, SessionState};
use super::unicode::normalize;

pub struct E1;

/// How many recent events keep a touched file's project active (SPEC §3.1, `touch_window`).
const TOUCH_WINDOW: u64 = 20;
/// Real mode only: an item pointed within this many events is not pointed again.
const POINTER_REPEAT_WINDOW: u64 = 20;
/// Real mode only: total pointer cap per event across all categories.
const MAX_POINTERS: usize = 8;
const HALF_LIFE_DAYS: f64 = 90.0;
const W_PROXIMITY: f64 = 0.4;
const W_RECENCY: f64 = 0.1;
const W_IMPORTANCE: f64 = 0.2;
const W_MATCH: f64 = 0.3;
/// Cross-project keyword pointers are intentionally capped below otherwise
/// equally-matched eligible items, so they never crowd out the project's own rules.
const CROSS_MATCH_PENALTY: f64 = 0.7;

/// Cap on note/decision pointers: past five, index lines crowd out the answer.
const MAX_NOTE_POINTERS: usize = 5;
/// Prompt text with code removed (SPEC §3.1: words in code never trigger):
/// indented lines, fenced blocks (a run of 3+ backticks or tildes, anywhere, up to the
/// next run of the same length, across lines) and inline spans (1–2 backticks, same line).
fn prose(prompt: &str) -> String {
    let unindented: Vec<&str> = prompt
        .lines()
        .filter(|l| !(l.starts_with("    ") || l.starts_with('\t')))
        .collect();
    let chars: Vec<char> = unindented.join("\n").chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '`' && c != '~' {
            out.push(c);
            i += 1;
            continue;
        }
        let mut run = 0;
        while i + run < chars.len() && chars[i + run] == c {
            run += 1;
        }
        let fenced = run >= 3;
        if c == '~' && !fenced {
            out.push_str(&chars[i..i + run].iter().collect::<String>());
            i += run;
            continue;
        }
        let mut j = i + run;
        let mut close = None;
        while j < chars.len() {
            if chars[j] == '\n' && !fenced {
                break;
            }
            if chars[j] == c {
                let mut r = 0;
                while j + r < chars.len() && chars[j + r] == c {
                    r += 1;
                }
                if r == run {
                    close = Some(j + r);
                    break;
                }
                j += r;
            } else {
                j += 1;
            }
        }
        match (close, fenced) {
            (Some(end), _) => {
                out.push(' ');
                i = end;
            }
            // An unclosed fence hides everything after it; an unmatched inline tick is literal.
            (None, true) => break,
            (None, false) => {
                out.extend(&chars[i..i + run]);
                i += run;
            }
        }
    }
    normalize(&out)
}

/// Remove image metadata before matching prose or project aliases.
fn without_images(prompt: &str) -> String {
    let mut out = String::new();
    let mut rest = prompt;
    while let Some(start) = rest.find("[Image") {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(end) = rest.find(']') {
            let marker = &rest[..=end];
            let numbered = marker
                .strip_prefix("[Image #")
                .and_then(|s| s.strip_suffix(']'))
                .is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()));
            if marker.starts_with("[Image: source: ") || numbered {
                out.push(' ');
                rest = &rest[end + 1..];
                continue;
            }
        }
        out.push('[');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

fn word_count(text: &str) -> usize {
    text.split(|c: char| !c.is_alphabetic())
        .filter(|word| word.chars().count() >= 3)
        .count()
}

fn has_word(text: &str, word: &str) -> bool {
    let word = normalize(word);
    if word.is_empty() {
        return false;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    text.match_indices(&word).any(|(start, m)| {
        !text[..start].chars().next_back().is_some_and(is_word)
            && !text[start + m.len()..].chars().next().is_some_and(is_word)
    })
}

fn under(path: &str, root: &str) -> bool {
    if !safe_relative(path) || !safe_relative(root) {
        return false;
    }
    normalized_path(path).starts_with(normalized_path(root))
}

fn normalized_path(value: &str) -> std::path::PathBuf {
    std::path::Path::new(value)
        .components()
        .filter(|c| *c != std::path::Component::CurDir)
        .collect()
}

/// A workspace-relative path with no absolute prefix and no `..` component.
fn safe_relative(path: &str) -> bool {
    let p = std::path::Path::new(path);
    !path.is_empty()
        && !p.is_absolute()
        && p.components().all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

fn path_matches(pattern: &str, path: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => path.starts_with(prefix),
        None => pattern == path,
    }
}

struct Scope {
    primary: Option<String>,
    projects: BTreeSet<String>,
    links: BTreeSet<String>,
}

fn resolve(fx: &Fixture, ev: &Event, touched: &[String], text: &str) -> Scope {
    let root_of = |p: &super::super::config::Project| p.root.to_string_lossy().to_string();
    let primary = fx
        .config
        .projects
        .iter()
        .filter(|p| under(&ev.cwd, &root_of(p)))
        .max_by_key(|p| normalized_path(&root_of(p)).components().count())
        .map(|p| p.name.clone());
    let mut projects: BTreeSet<String> = primary.iter().cloned().collect();
    let descendants = fx
        .config
        .projects
        .iter()
        .filter(|p| {
            let root = root_of(p);
            under(&root, &ev.cwd) && normalized_path(&root) != normalized_path(&ev.cwd)
        })
        .count();
    let parent_scope = !normalized_path(&ev.cwd).as_os_str().is_empty()
        && descendants <= fx.config.workspace.parent_scope_max
        && fx.config.workspace.parent_scope_max > 0;
    for p in &fx.config.projects {
        let root = root_of(p);
        let contains = parent_scope && under(&root, &ev.cwd);
        if contains || touched.iter().any(|t| under(t, &root)) {
            projects.insert(p.name.clone());
        }
        let named = std::iter::once(&p.name).chain(&p.aliases);
        if named.into_iter().any(|a| has_word(text, a)) {
            projects.insert(p.name.clone());
        }
    }
    let workspace = fx.code_root.clone();
    let mut links = BTreeSet::new();
    for link in &fx.config.links {
        let active_members = link
            .projects
            .iter()
            .filter(|p| projects.contains(*p))
            .count();
        let members: Vec<String> = fx
            .config
            .projects
            .iter()
            .filter(|p| link.projects.contains(&p.name))
            .map(root_of)
            .collect();
        let signal = touched.iter().filter(|t| safe_relative(t)).any(|t| {
            members.iter().any(|r| under(t, r))
                && std::fs::read_to_string(workspace.join(t))
                    .map(|code| link.code_signals.iter().any(|s| code.contains(s.as_str())))
                    .unwrap_or(false)
        });
        let keyword = active_members >= 1 && link.keywords.iter().any(|k| has_word(text, k));
        if active_members >= 2 || signal || keyword {
            links.insert(link.name.clone());
        }
    }
    Scope {
        primary,
        projects,
        links,
    }
}

fn eligible(scope: &Scope, item: &Item) -> bool {
    match item.scope.split_once(':') {
        None => item.scope == "global",
        Some(("project", name)) => scope.projects.contains(name),
        Some(("link", name)) => scope.links.contains(name),
        Some(_) => false,
    }
}

fn is_current(item: &Item, now: &str) -> bool {
    if item.status != Status::Active {
        return false;
    }
    match (&item.expires_at, DateTime::parse_from_rfc3339(now)) {
        (Some(exp), Ok(now)) => DateTime::parse_from_rfc3339(exp).is_ok_and(|e| e > now),
        (Some(_), Err(_)) => false,
        (None, _) => true,
    }
}

fn layer(item: &Item) -> usize {
    if item.scope.starts_with("project:") {
        0
    } else if item.scope.starts_with("link:") {
        1
    } else {
        2
    }
}

fn is_note(item: &Item) -> bool {
    matches!(item.kind, Kind::Note | Kind::Decision)
}

/// Count distinct keywords of `item` that appear in `text`.
fn keyword_matches(item: &Item, text: &str) -> usize {
    item.triggers
        .keywords
        .iter()
        .map(|k| normalize(k))
        .filter(|k| !k.is_empty() && has_word(text, k))
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

/// Real-mode check: has `item` been pointed recently enough to be considered a repeat?
/// Exempt if the current prompt matches two or more distinct keywords of the item.
fn is_repeated_pointer(item: &Item, ev: &Event, state: &SessionState, text: &str) -> bool {
    if ev.allowed_scopes.is_empty() {
        if let Some(&last) = state.pointed.get(&item.id) {
            if ev.event_seq.saturating_sub(last) < POINTER_REPEAT_WINDOW
                && keyword_matches(item, text) < 2
            {
                return true;
            }
        }
    }
    false
}

/// Score for pointer candidates that did not go through the normal scoring pipeline
/// (mainly cross-project keyword pointers). It uses the same [0, 1] scale as `scored`
/// but deliberately keeps cross-project matches below an equally-matched eligible item.
fn fallback_score(
    item: &Item,
    cross_matches: usize,
    now: Option<DateTime<chrono::FixedOffset>>,
) -> f64 {
    let recency = match (now, DateTime::parse_from_rfc3339(&item.updated_at)) {
        (Some(now), Ok(upd)) => {
            let days = (now - upd).num_seconds().max(0) as f64 / 86_400.0;
            0.5_f64.powf(days / HALF_LIFE_DAYS)
        }
        _ => 0.5,
    };
    let proximity = 0.2; // cross-project / out-of-focus behaves like global
    let importance = item.importance.clamp(0.0, 1.0);
    let match_strength = if cross_matches > 0 {
        // Two matched keywords count as a full keyword match; cross-project is still
        // capped below an eligible item with the same raw features.
        (cross_matches as f64 / 2.0).min(1.0) * W_MATCH * CROSS_MATCH_PENALTY
    } else {
        0.0
    };
    W_PROXIMITY * proximity + W_RECENCY * recency + W_IMPORTANCE * importance + match_strength
}

/// Real-mode final pointer policy: remove already-served items, suppress repeats within
/// `POINTER_REPEAT_WINDOW`, and cap the total to `MAX_POINTERS` while keeping the
/// highest-scored candidates. Benchmark mode is left untouched.
fn finalize_pointers(
    fx: &Fixture,
    selection: &mut Selection,
    scored: &[(f64, &Item)],
    cross: &[(&Item, usize)],
    ev: &Event,
    state: &SessionState,
    text: &str,
) {
    if !ev.allowed_scopes.is_empty() {
        return;
    }
    let score_of: BTreeMap<&str, f64> = scored.iter().map(|(s, i)| (i.id.as_str(), *s)).collect();
    let cross_score_of: BTreeMap<&str, usize> = cross
        .iter()
        .map(|(item, matches)| (item.id.as_str(), *matches))
        .collect();
    // Build a lookup only for candidates we may need, not the whole workspace.
    let mut candidate_items: BTreeMap<&str, &Item> = scored
        .iter()
        .map(|(_, i)| (i.id.as_str(), *i))
        .chain(cross.iter().map(|(i, _)| (i.id.as_str(), *i)))
        .collect();
    // Fallback for ids that did not go through the scored/cross pipeline (e.g. image-only
    // fallback rules). We only scan the store for the handful of missing ids.
    for id in &selection.pointer_ids {
        if !candidate_items.contains_key(id.as_str()) {
            if let Some(item) = fx.items.iter().find(|i| i.id == *id) {
                candidate_items.insert(item.id.as_str(), item);
            }
        }
    }
    let now = DateTime::parse_from_rfc3339(&ev.now).ok();
    let mut candidates: Vec<(String, f64, usize)> = Vec::new();
    for (order, id) in selection.pointer_ids.drain(..).enumerate() {
        // Already delivered in full this session: do not point again.
        if state.served.contains(&id) {
            continue;
        }
        let item = match candidate_items.get(id.as_str()) {
            Some(item) => *item,
            None => {
                // Unknown pointer ids (should not happen) are dropped rather than kept.
                continue;
            }
        };
        // Pointed recently? Exempt when the current prompt matches two or more keywords.
        if is_repeated_pointer(item, ev, state, text) {
            continue;
        }
        let score = score_of.get(id.as_str()).copied().unwrap_or_else(|| {
            let matches = cross_score_of.get(id.as_str()).copied().unwrap_or(0);
            fallback_score(item, matches, now)
        });
        candidates.push((id, score, order));
    }
    // Highest score first; ties keep the original order (stable by input index).
    candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
    let kept = candidates
        .iter()
        .take(MAX_POINTERS)
        .map(|(id, _, _)| id.clone())
        .collect::<Vec<_>>();
    let dropped = candidates.len().saturating_sub(kept.len());
    selection.pointer_ids = kept;
    selection.omitted_pointers = dropped;
}

/// Drop the lowest-ranked pointers until the final rendered output fits the budget.
/// `used_bytes` is recalculated with `render`, so it always reflects the final state.
fn trim_pointers_to_budget(fx: &Fixture, ev: &Event, selection: &mut Selection) {
    let budget = fx.config.workspace.budget_bytes as usize;
    selection.used_bytes = render(fx, ev, selection).len();
    // Trim only in real mode. Benchmark mode must keep the legacy output byte-for-byte.
    if !ev.allowed_scopes.is_empty() {
        return;
    }
    while selection.used_bytes > budget && !selection.pointer_ids.is_empty() {
        selection.pointer_ids.pop();
        selection.omitted_pointers += 1;
        selection.used_bytes = render(fx, ev, selection).len();
    }
    if selection.used_bytes > budget {
        // The omission notice itself does not fit; prefer silence to a truncated envelope.
        selection.omitted_pointers = 0;
        selection.used_bytes = render(fx, ev, selection).len();
    }
}

// Fit notes under both ceilings first; append a notice only if it still fits.
fn limit_notes(
    fx: &Fixture,
    ev: &Event,
    text: &str,
    selection: &mut Selection,
    eligible: &BTreeSet<String>,
) {
    let items: BTreeMap<_, _> = fx.items.iter().map(|i| (i.id.as_str(), i)).collect();
    let belongs = |id: &String| items.get(id.as_str()).is_some_and(|i| is_note(i));
    let candidates: Vec<_> = selection
        .full_ids
        .iter()
        .filter(|id| belongs(id))
        .map(|id| (id.clone(), true))
        .chain(
            selection
                .pointer_ids
                .iter()
                .filter(|id| belongs(id))
                .map(|id| (id.clone(), false)),
        )
        .collect();
    selection.full_ids.retain(|id| !belongs(id));
    selection.pointer_ids.retain(|id| !belongs(id));
    let base = selection.clone();
    let mut kept = Selection::default();
    let with_notice = |notes: &mut Selection| {
        let omitted: Vec<_> = candidates
            .iter()
            .filter(|(id, _)| !notes.full_ids.contains(id) && !notes.pointer_ids.contains(id))
            .collect();
        notes.omitted_notes.clear();
        if !omitted.is_empty() {
            let keyword = omitted
                .iter()
                .flat_map(|(id, _)| &items[id.as_str()].triggers.keywords)
                .map(|k| normalize(k))
                .find(|k| has_word(text, k))
                .unwrap_or_else(|| "notas".into());
            notes.omitted_notes.push((omitted.len(), keyword));
        }
    };
    let combined = |notes: &Selection| {
        let mut all = base.clone();
        all.full_ids.extend(notes.full_ids.clone());
        all.pointer_ids.extend(notes.pointer_ids.clone());
        all.omitted_notes = notes.omitted_notes.clone();
        all
    };
    let fits = |notes: &Selection| {
        (fx.config.workspace.notes_budget_bytes == 0
            || render_content(fx, notes).len() as u64 <= fx.config.workspace.notes_budget_bytes)
            && render(fx, ev, &combined(notes)).len() as u64 <= fx.config.workspace.budget_bytes
    };
    let mut demoted: BTreeSet<String> = BTreeSet::new();
    for (id, full) in &candidates {
        // An oversized full note gets a second chance as a pointer.
        for as_full in [true, false]
            .into_iter()
            .filter(|as_full| !as_full || *full)
        {
            let mut next = kept.clone();
            if as_full {
                next.full_ids.push(id.clone());
            } else {
                next.pointer_ids.push(id.clone());
            }
            if fits(&next) {
                if *full && !as_full {
                    demoted.insert(id.clone());
                }
                kept = next;
                break;
            }
        }
    }
    // A full note demoted for size frees its slot: promote the next eligible
    // pointer that fits, keeping full_notes_max and both byte ceilings.
    let max = fx.config.workspace.full_notes_max;
    for (id, _) in &candidates {
        if max > 0 && kept.full_ids.len() >= max {
            break;
        }
        // A note demoted for size never competes for the slot it freed.
        if !eligible.contains(id) || !kept.pointer_ids.contains(id) || demoted.contains(id) {
            continue;
        }
        let mut next = kept.clone();
        next.pointer_ids.retain(|p| p != id);
        next.full_ids.push(id.clone());
        if fits(&next) {
            kept = next;
        }
    }
    with_notice(&mut kept);
    if !fits(&kept) {
        kept.omitted_notes.clear();
    }
    *selection = combined(&kept);
}

/// Keeps at most `MAX_NOTE_POINTERS` note/decision pointers: the highest scored ones
/// (ties: most recent `updated_at`, then id). Rule and law pointers keep their order.
fn limit_note_pointers(
    fx: &Fixture,
    ev: &Event,
    text: &str,
    scored: &[(f64, &Item)],
    selection: &mut Selection,
) {
    let items: BTreeMap<&str, &Item> = fx.items.iter().map(|i| (i.id.as_str(), i)).collect();
    let is_note_id = |id: &str| items.get(id).is_some_and(|i| is_note(i));
    if selection
        .pointer_ids
        .iter()
        .filter(|id| is_note_id(id.as_str()))
        .count()
        <= MAX_NOTE_POINTERS
    {
        return;
    }
    let score_of: BTreeMap<&str, f64> = scored.iter().map(|(s, i)| (i.id.as_str(), *s)).collect();
    let mut note_pointers: Vec<&String> = selection
        .pointer_ids
        .iter()
        .filter(|id| is_note_id(id.as_str()))
        .collect();
    note_pointers.sort_by(|a, b| {
        let sa = score_of.get(a.as_str()).copied().unwrap_or(f64::MIN);
        let sb = score_of.get(b.as_str()).copied().unwrap_or(f64::MIN);
        let ua = DateTime::parse_from_rfc3339(&items[a.as_str()].updated_at).ok();
        let ub = DateTime::parse_from_rfc3339(&items[b.as_str()].updated_at).ok();
        sb.total_cmp(&sa)
            .then_with(|| ub.cmp(&ua))
            .then_with(|| a.cmp(b))
    });
    let keep: BTreeSet<String> = note_pointers
        .into_iter()
        .take(MAX_NOTE_POINTERS)
        .cloned()
        .collect();
    let cut: Vec<String> = selection
        .pointer_ids
        .iter()
        .filter(|id| is_note_id(id.as_str()) && !keep.contains(id.as_str()))
        .cloned()
        .collect();
    selection
        .pointer_ids
        .retain(|id| !is_note_id(id.as_str()) || keep.contains(id.as_str()));
    // Cut pointers join the omission notice, so `elo recall` still finds them. The total
    // is recounted from every matched note (an earlier notice may have been dropped for
    // size), and the notice is kept only if the whole output still fits the budget.
    let shown = |id: &str| {
        selection.full_ids.iter().any(|f| f == id) || selection.pointer_ids.iter().any(|p| p == id)
    };
    let omitted = scored
        .iter()
        .filter(|(_, i)| is_note(i) && !shown(i.id.as_str()))
        .count();
    let keyword = selection
        .omitted_notes
        .first()
        .map(|(_, k)| k.clone())
        .or_else(|| {
            cut.iter()
                .flat_map(|id| &items[id.as_str()].triggers.keywords)
                .map(|k| normalize(k))
                .find(|k| has_word(text, k))
        })
        .unwrap_or_else(|| "notas".into());
    selection.omitted_notes = if omitted > 0 {
        vec![(omitted, keyword)]
    } else {
        vec![]
    };
    let budget = usize::try_from(fx.config.workspace.budget_bytes).unwrap_or(usize::MAX);
    if render(fx, ev, selection).len() > budget {
        selection.omitted_notes.clear();
    }
}

impl Selector for E1 {
    fn name(&self) -> &str {
        "E1"
    }

    fn select(&self, fx: &Fixture, ev: &Event, state: &mut SessionState) -> Selection {
        let real_prompt = ev.allowed_scopes.is_empty() && ev.event_type == "user-prompt-submit";
        let prompt = ev.prompt.as_deref().unwrap_or("");
        let text = prose(&if real_prompt {
            without_images(prompt)
        } else {
            prompt.into()
        });
        let words = word_count(&text);
        let matching_text = if real_prompt && ev.prompt.is_some() && words > 0 && words < 5 {
            format!("{text}\n{}", state.recent_prompts.join("\n"))
        } else {
            text.clone()
        };
        let mut matched = false;
        let touched: Vec<String> = state
            .touched
            .iter()
            .filter(|(_, seq)| ev.event_seq.saturating_sub(*seq) < TOUCH_WINDOW)
            .map(|(p, _)| p.clone())
            .collect();
        let scope = resolve(fx, ev, &touched, &text);

        // Keep note volume out of rule scoring, so adding notes cannot change
        // the law/rule baseline. Generic rule keywords still weigh less.
        let mut df: BTreeMap<String, usize> = BTreeMap::new();
        for item in fx.items.iter().filter(|i| !is_note(i)) {
            for k in &item.triggers.keywords {
                *df.entry(normalize(k)).or_default() += 1;
            }
        }
        let n = fx.items.iter().filter(|i| !is_note(i)).count().max(1) as f64;
        let max_idf = (n + 1.0).ln();
        let idf = |k: &str| {
            let d = *df.get(&normalize(k)).unwrap_or(&0) as f64;
            ((n + 1.0) / (d + 1.0)).ln() / max_idf
        };
        // A single matched word is rare when it has a digit ("150.000") or 8+ characters.
        let now = DateTime::parse_from_rfc3339(&ev.now).ok();

        let mut indexed = BTreeSet::new();
        let mut laws: Vec<&Item> = Vec::new();
        let mut scored: Vec<(f64, &Item)> = Vec::new();
        // Each cross-project pointer candidate stores the item and how many of its keywords matched.
        let mut cross_project_pointers: Vec<(&Item, usize)> = Vec::new();
        for item in fx.items.iter().filter(|i| is_current(i, &ev.now)) {
            let kw: Vec<&String> = item
                .triggers
                .keywords
                .iter()
                .filter(|k| {
                    has_word(
                        if item.kind == Kind::Law {
                            &text
                        } else {
                            &matching_text
                        },
                        k,
                    )
                })
                .collect();
            let path_hit = item
                .triggers
                .paths
                .iter()
                .any(|p| touched.iter().any(|t| path_matches(p, t)));
            let always = item.triggers.always
                && !crate::stats::demoted(item, fx.config.workspace.demote_bad_min);
            let project_scope_allowed = eligible(&scope, item);
            let project_keyword = item.scope.starts_with("project:")
                && !kw.is_empty()
                && (ev.allowed_scopes.is_empty() || ev.allowed_scopes.contains(&item.scope));
            if !project_scope_allowed && !project_keyword {
                continue;
            }
            matched |= !kw.is_empty() || path_hit;
            if kw.is_empty() && !path_hit && !always {
                continue;
            }
            // project:X items triggered by keyword while the item is NOT eligible by scope are
            // shown as short pointers, never in full text. path_hit keeps them full (the agent is
            // touching a file in that project). In benchmark mode (allowed_scopes non-empty) the
            // legacy behaviour is preserved so benchmarks keep measuring the same policy.
            let is_project_scope = item.scope.starts_with("project:");
            let real_mode = ev.allowed_scopes.is_empty();
            if is_project_scope && !project_scope_allowed && !path_hit && real_mode {
                cross_project_pointers.push((item, kw.len()));
                continue;
            }
            if item.kind == Kind::Law {
                laws.push(item);
                continue;
            }
            if real_mode
                && !path_hit
                && item
                    .usage_window
                    .demoted(item.kind, fx.config.workspace.demote_unused_min)
            {
                indexed.insert(item.id.clone());
            }
            if is_note(item)
                && fx.config.workspace.full_notes_max > 0
                && !path_hit
                && kw
                    .iter()
                    .map(|k| normalize(k))
                    .collect::<BTreeSet<_>>()
                    .len()
                    < 2
                && !(real_prompt
                    && kw
                        .iter()
                        .any(|k| k.chars().any(|c| c.is_numeric()) || k.chars().count() >= 8))
            {
                // An update within the last 48 hours does not waive trigger specificity.
                indexed.insert(item.id.clone());
            }
            if kw.is_empty()
                && !path_hit
                && always
                && fx.config.workspace.index_over_bytes > 0
                && item.body.len() as u64 > fx.config.workspace.index_over_bytes
            {
                indexed.insert(item.id.clone());
            }
            let proximity = if path_hit {
                1.0
            } else {
                match item.scope.split_once(':') {
                    Some(("project", p)) if scope.primary.as_deref() == Some(p) => 0.6,
                    Some(("project", _)) => 0.5,
                    Some(("link", _)) => 0.4,
                    _ => 0.2,
                }
            };
            let recency = match (now, DateTime::parse_from_rfc3339(&item.updated_at)) {
                (Some(now), Ok(upd)) => {
                    let days = (now - upd).num_seconds().max(0) as f64 / 86_400.0;
                    0.5_f64.powf(days / HALF_LIFE_DAYS)
                }
                _ => 0.5,
            };
            let strength = if path_hit || always {
                1.0
            } else {
                kw.iter().map(|k| idf(k)).fold(0.0, f64::max)
            };
            let strength = if real_prompt
                && !kw.is_empty()
                && !kw.iter().any(|k| has_word(&text, k))
                && !path_hit
                && !always
            {
                strength * 0.5
            } else {
                strength
            };
            let score = W_PROXIMITY * proximity
                + W_RECENCY * recency
                + W_IMPORTANCE * item.importance.clamp(0.0, 1.0)
                + W_MATCH * strength;
            if score >= fx.config.workspace.min_score {
                scored.push((score, item));
            }
        }

        let budget = usize::try_from(fx.config.workspace.budget_bytes).unwrap_or(usize::MAX);
        let mut full: Vec<String> = Vec::new();
        let fits = |ids: &[String], ptr: &[String]| {
            Selection::new(fx, ev, ids.to_vec(), ptr.to_vec()).used_bytes <= budget
        };
        // Laws first; a law already delivered this session is not repeated (SPEC §4.4).
        // In real mode the one-line reminder is also dropped to avoid pointer noise; the
        // law returns in full after the next compaction/session-start.
        laws.sort_by(|a, b| a.id.cmp(&b.id));
        let mut law_reminders: Vec<String> = Vec::new();
        for law in laws {
            if state.served.contains(&law.id) {
                if !ev.allowed_scopes.is_empty() {
                    // Benchmark mode keeps the legacy reminder line.
                    law_reminders.push(law.id.clone());
                }
                continue;
            }
            let mut next = full.clone();
            next.push(law.id.clone());
            if fits(&next, &[]) {
                full = next;
            }
        }
        // Then layers in order with their quota of the remaining budget; leftovers become pointers.
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        let quotas = [
            fx.config.quotas.project,
            fx.config.quotas.link,
            fx.config.quotas.global,
        ];
        let base = Selection::new(fx, ev, full.clone(), vec![]).used_bytes;
        let remaining = budget.saturating_sub(base) as f64;
        let mut carry = 0.0;
        let mut pointers: Vec<String> = Vec::new();
        for (idx, quota) in quotas.iter().enumerate() {
            let allowance = remaining * quota + carry;
            let start = Selection::new(fx, ev, full.clone(), vec![]).used_bytes;
            for (_, item) in scored
                .iter()
                .filter(|(_, i)| layer(i) == idx && !is_note(i))
            {
                if indexed.contains(&item.id) {
                    // Item outside the focus: uses only the index line.
                    pointers.push(item.id.clone());
                    continue;
                }
                if state.served.contains(&item.id) {
                    // Already delivered in full this session. Benchmark mode keeps the legacy
                    // pointer; real mode drops it to reduce noise.
                    if !ev.allowed_scopes.is_empty() {
                        pointers.push(item.id.clone());
                    }
                    continue;
                }
                let mut next = full.clone();
                next.push(item.id.clone());
                let used = Selection::new(fx, ev, next.clone(), vec![]).used_bytes;
                if used <= budget && (used - start) as f64 <= allowance {
                    full = next;
                } else {
                    pointers.push(item.id.clone());
                }
            }
            let spent = Selection::new(fx, ev, full.clone(), vec![])
                .used_bytes
                .saturating_sub(start);
            carry = (allowance - spent as f64).max(0.0);
        }
        // Preserve the existing law/rule pointer fit predicate, including the envelope.
        let mut kept = Vec::new();
        for id in law_reminders.into_iter().chain(pointers.iter().cloned()) {
            let mut next = kept.clone();
            next.push(id);
            if fits(&full, &next) {
                kept = next;
            }
        }
        // Freeze laws and rules before notes can consume any budget.
        let mut selection = Selection::new(fx, ev, full, kept);
        let remaining = budget.saturating_sub(selection.used_bytes) as f64;
        let mut carry = 0.0;
        let mut full_notes = 0;
        let mut pending_pointers = Vec::new();
        // Notes allowed in full (not indexed, not served yet), for slot reuse.
        let eligible: BTreeSet<String> = scored
            .iter()
            .filter(|(_, i)| {
                is_note(i) && !indexed.contains(&i.id) && !state.served.contains(&i.id)
            })
            .map(|(_, i)| i.id.clone())
            .collect();
        for (idx, quota) in quotas.iter().enumerate() {
            let allowance = remaining * quota + carry;
            let start = render(fx, ev, &selection).len();
            let mut notes: Vec<_> = scored
                .iter()
                .filter(|(_, i)| layer(i) == idx && is_note(i))
                .map(|(_, i)| *i)
                .collect();
            // Recency only orders notes inside their own layer.
            notes.sort_by(|a, b| {
                DateTime::parse_from_rfc3339(&b.updated_at)
                    .ok()
                    .cmp(&DateTime::parse_from_rfc3339(&a.updated_at).ok())
                    .then_with(|| a.id.cmp(&b.id))
            });
            let mut note_pointers = Vec::new();
            for item in notes {
                let mut next = selection.clone();
                next.full_ids.push(item.id.clone());
                let used = render(fx, ev, &next).len();
                if !indexed.contains(&item.id)
                    && !state.served.contains(&item.id)
                    && (fx.config.workspace.full_notes_max == 0
                        || full_notes < fx.config.workspace.full_notes_max)
                    && used <= budget
                    && (used - start) as f64 <= allowance
                {
                    selection = next;
                    full_notes += 1;
                } else {
                    note_pointers.push(item.id.clone());
                }
            }
            let spent = render(fx, ev, &selection).len().saturating_sub(start);
            carry = (allowance - spent as f64).max(0.0);
            pending_pointers.extend(note_pointers);
        }
        selection.pointer_ids.extend(pending_pointers);
        limit_notes(fx, ev, &text, &mut selection, &eligible);
        // Real mode only: too many note/decision pointers is noise; keep the best five.
        // Benchmark mode (allowed_scopes filled) keeps measuring the legacy policy.
        if ev.allowed_scopes.is_empty() {
            limit_note_pointers(fx, ev, &text, &scored, &mut selection);
        }
        // Add cross-project keyword pointers (max 3) that fit inside the budget.
        // Candidates are sorted by relevance (more keyword matches first, then id), duplicates
        // against existing pointers/full items are skipped, and the budget is checked for each
        // candidate in order; if one does not fit the next candidate is tried instead of stopping.
        let mut cross_pointer_entries = cross_project_pointers;
        cross_pointer_entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
        let mut inserted = 0;
        for (item, _) in &cross_pointer_entries {
            if inserted >= 3 {
                break;
            }
            let id = &item.id;
            if selection.pointer_ids.contains(id) || selection.full_ids.contains(id) {
                continue;
            }
            let mut next = selection.pointer_ids.clone();
            next.push(id.clone());
            if fits(&selection.full_ids, &next) {
                selection.pointer_ids.push(id.clone());
                inserted += 1;
            }
        }
        if real_prompt && ev.prompt.is_some() && words == 0 && !matched {
            if let Some(primary) = &scope.primary {
                let project = format!("project:{primary}");
                let mut candidates: Vec<_> = fx
                    .items
                    .iter()
                    .filter(|item| {
                        matches!(item.kind, Kind::Rule | Kind::Law)
                            && item.scope == project
                            && is_current(item, &ev.now)
                            && !state.served.contains(&item.id)
                    })
                    .collect();
                candidates.sort_by(|a, b| {
                    b.importance
                        .total_cmp(&a.importance)
                        .then_with(|| {
                            DateTime::parse_from_rfc3339(&b.updated_at)
                                .ok()
                                .cmp(&DateTime::parse_from_rfc3339(&a.updated_at).ok())
                        })
                        .then_with(|| a.id.cmp(&b.id))
                });
                // Up to 3 rules actually added (an item already in full does not use a slot).
                let mut added = 0;
                for item in candidates {
                    if added >= 3 {
                        break;
                    }
                    if selection.full_ids.contains(&item.id) {
                        continue;
                    }
                    let path_hit = item
                        .triggers
                        .paths
                        .iter()
                        .any(|p| touched.iter().any(|t| path_matches(p, t)));
                    if ev.allowed_scopes.is_empty()
                        && !path_hit
                        && item
                            .usage_window
                            .demoted(item.kind, fx.config.workspace.demote_unused_min)
                    {
                        if !selection.pointer_ids.contains(&item.id) {
                            let mut next = selection.clone();
                            next.pointer_ids.push(item.id.clone());
                            if render(fx, ev, &next).len() <= budget {
                                selection = next;
                            }
                        }
                        continue;
                    }
                    let mut next = selection.clone();
                    next.pointer_ids.retain(|id| id != &item.id);
                    next.full_ids.push(item.id.clone());
                    if render(fx, ev, &next).len() <= budget {
                        selection = next;
                        added += 1;
                    } else if !selection.pointer_ids.contains(&item.id) {
                        let mut next = selection.clone();
                        next.pointer_ids.push(item.id.clone());
                        if render(fx, ev, &next).len() <= budget {
                            selection = next;
                        }
                    }
                }
            }
        }
        finalize_pointers(
            fx,
            &mut selection,
            &scored,
            &cross_pointer_entries,
            ev,
            state,
            &text,
        );
        if real_prompt && ev.prompt.is_some() && !text.trim().is_empty() {
            let mut end = text.len().min(400);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            state.recent_prompts.push(text[..end].to_string());
            let excess = state.recent_prompts.len().saturating_sub(2);
            state.recent_prompts.drain(..excess);
        }
        trim_pointers_to_budget(fx, ev, &mut selection);
        if real_prompt && fx.config.shadow.enabled {
            let mut candidates = scored.iter().filter(|(_, item)| item.kind != Kind::Law);
            let best = candidates.next();
            let top = best.map_or(0.0, |(score, _)| *score);
            let second = candidates.next().map_or(0.0, |(score, _)| *score);
            let matched_words = best.map_or(0, |(_, item)| keyword_matches(item, &matching_text));
            selection.shadow = Some(crate::shadow::ShadowDecision::new(
                top,
                top - second,
                matched_words,
                fx.config.shadow.abstain_min_score,
            ));
        }
        selection
    }
}

#[cfg(test)]
mod tests {
    use super::{has_word, prose, safe_relative, under};

    #[test]
    fn shadow_preserves_delivery_and_only_observes_real_prompts() {
        let (mut fx, mut ev) = prompt_fixture();
        let mut enabled = SessionState::default();
        let mut disabled = enabled.clone();
        for (seq, prompt) in ["zebra herd", "unrelated topic", "zebra", "zebra herd again"]
            .iter()
            .enumerate()
        {
            ev.event_seq = seq as u64;
            ev.prompt = Some((*prompt).into());
            fx.config.shadow.enabled = true;
            let a = E1.select(&fx, &ev, &mut enabled);
            assert!(a.shadow.is_some());
            fx.config.shadow.enabled = false;
            let b = E1.select(&fx, &ev, &mut disabled);
            assert!(b.shadow.is_none());
            assert_eq!(a.full_ids, b.full_ids);
            assert_eq!(a.pointer_ids, b.pointer_ids);
            assert_eq!(a.used_bytes, b.used_bytes);
            assert_eq!(render(&fx, &ev, &a), render(&fx, &ev, &b));
            enabled.record(&ev, &a);
            disabled.record(&ev, &b);
        }
        fx.config.shadow.enabled = true;
        fx.items.truncate(1);
        fx.items[0].kind = Kind::Rule;
        fx.items[0].importance = 1.0;
        fx.items[0].triggers.always = true;
        fx.items[0].updated_at = ev.now.clone();
        let high = E1.select(&fx, &ev, &mut enabled).shadow.unwrap();
        assert!(high.top >= 0.55);
        assert!(!high.would_abstain);
        fx.items.clear();
        assert!(
            E1.select(&fx, &ev, &mut enabled)
                .shadow
                .unwrap()
                .would_abstain
        );
        ev.allowed_scopes.push("global".into());
        assert!(E1.select(&fx, &ev, &mut enabled).shadow.is_none());
        ev.allowed_scopes.clear();
        ev.event_type = "session-start".into();
        assert!(E1.select(&fx, &ev, &mut enabled).shadow.is_none());
    }

    #[test]
    fn a_fence_opened_mid_line_hides_the_block() {
        let p = prose("Sort these: ```text\nrelease-check\naudit\n```. done api");
        assert!(!has_word(&p, "release-check"));
        assert!(has_word(&p, "api"));
        let q = prose("open ```\nbilling never closed");
        assert!(!has_word(&q, "billing"));
    }

    #[test]
    fn words_inside_code_never_count() {
        let p = prose("fix api\n```\nbilling.charge()\n```\nand `web` too");
        assert!(has_word(&p, "api"));
        assert!(!has_word(&p, "billing"));
        assert!(!has_word(&p, "web"));
    }

    #[test]
    fn double_backtick_spans_and_unmatched_ticks() {
        let p = prose("see ``a `web` b`` then billing and a lone ` tick");
        assert!(!has_word(&p, "web"));
        assert!(has_word(&p, "billing"));
    }

    #[test]
    fn indented_code_blocks_are_ignored() {
        let p = prose("check this:\n    billing.run()\nok api");
        assert!(!has_word(&p, "billing"));
        assert!(has_word(&p, "api"));
    }

    #[test]
    fn root_dot_covers_workspace_and_paths_are_confined() {
        assert!(under("api/src/x.ts", "."));
        assert!(under(".", "."));
        assert!(!under("", "."));
        assert!(!under("api2/x.ts", "api"));
        assert!(under("api/x.ts", "api/"));
        assert!(under("./api/x.ts", "./api/"));
        assert!(!under("../api/x.ts", "api"));
        assert!(!under(".api/x.ts", "api"));
        assert!(!safe_relative("/etc/passwd"));
        assert!(!safe_relative("../outside.ts"));
        assert!(safe_relative("api/src/x.ts"));
    }

    use super::*;

    fn note_fixture(rules: usize, benchmark: bool) -> (Fixture, Event) {
        let mut fx = super::super::load_fixture("bench/fixture/v0").unwrap();
        fx.config.workspace.min_score = 0.0;
        fx.config.workspace.full_notes_max = 1;
        let note_base = fx
            .items
            .iter()
            .find(|i| is_note(i))
            .cloned()
            .expect("fixture has a note");
        let rule_base = fx
            .items
            .iter()
            .find(|i| !is_note(i) && i.kind != Kind::Law)
            .cloned()
            .expect("fixture has a rule");
        let mut items = Vec::new();
        for n in 0..8 {
            let mut it = note_base.clone();
            it.id = format!("note-{n}");
            it.kind = Kind::Note;
            it.status = Status::Active;
            it.scope = "global".into();
            it.triggers.keywords = vec!["zebra".into()];
            it.triggers.paths.clear();
            it.triggers.always = false;
            it.importance = 0.5;
            it.expires_at = None;
            // note-7 is the most recent, so it scores highest; bodies never fit in full.
            it.updated_at = format!("2026-01-0{}T00:00:00Z", n + 1);
            it.body = "n".repeat(50_000);
            items.push(it);
        }
        for r in 0..rules {
            let mut it = rule_base.clone();
            it.id = format!("rule-{r}");
            it.status = Status::Active;
            it.scope = "global".into();
            it.triggers.keywords = vec!["zebra".into()];
            it.triggers.paths.clear();
            it.triggers.always = false;
            it.importance = 0.9;
            it.expires_at = None;
            it.updated_at = "2026-01-09T00:00:00Z".into();
            it.body = "r".repeat(50_000);
            items.push(it);
        }
        fx.items = items;
        let mut ev = fx.events[0].clone();
        ev.prompt = Some("zebra herd".into());
        ev.now = "2026-01-10T00:00:00Z".into();
        ev.cwd = ".".into();
        ev.allowed_scopes = if benchmark {
            ["project:other".to_string()].into_iter().collect()
        } else {
            Default::default()
        };
        (fx, ev)
    }

    fn prompt_fixture() -> (Fixture, Event) {
        let (mut fx, mut ev) = note_fixture(1, false);
        ev.event_type = "user-prompt-submit".into();
        fx.config.workspace.budget_bytes = 100_000;
        for item in &mut fx.items {
            item.body = "Short body".into();
        }
        (fx, ev)
    }

    #[test]
    fn vague_prompts_inherit_only_recent_topics() {
        let (mut fx, mut ev) = prompt_fixture();
        fx.items[0].triggers.keywords = vec!["widget".into()];
        let mut state = SessionState::default();
        ev.prompt = Some("corrigir o widget".into());
        E1.select(&fx, &ev, &mut state);
        ev.prompt = Some("pode fazer o 2".into());
        let inherited = E1.select(&fx, &ev, &mut state);
        // One common inherited word: the note comes back (as an index line).
        assert!(
            inherited.full_ids.contains(&"note-0".into())
                || inherited.pointer_ids.contains(&"note-0".into())
        );
        let fresh = E1.select(&fx, &ev, &mut SessionState::default());
        assert!(fresh.full_ids.is_empty() && fresh.pointer_ids.is_empty());
        ev.prompt = Some("please repair these other broken components".into());
        let long = E1.select(&fx, &ev, &mut state);
        assert!(long.full_ids.is_empty() && long.pointer_ids.is_empty());
        assert_eq!(state.recent_prompts.len(), 2);
        let saved = state.recent_prompts.clone();
        ev.prompt = None;
        let absent = E1.select(&fx, &ev, &mut state);
        assert!(absent.full_ids.is_empty() && absent.pointer_ids.is_empty());
        assert_eq!(state.recent_prompts, saved);
        ev.event_type = "compaction".into();
        state.begin_event(&ev);
        assert_eq!(state.recent_prompts, saved);
        ev.event_type = "session-start".into();
        state.begin_event(&ev);
        assert!(state.recent_prompts.is_empty());
    }

    #[test]
    fn history_is_utf8_safe_and_old_states_load() {
        let (fx, mut ev) = prompt_fixture();
        let mut state: SessionState =
            serde_json::from_str(r#"{"served":[],"touched":[]}"#).unwrap();
        assert!(state.recent_prompts.is_empty());
        ev.prompt = Some(format!("aa{}", "界".repeat(200)));
        E1.select(&fx, &ev, &mut state);
        assert_eq!(state.recent_prompts[0].len(), 398);
    }

    #[test]
    fn rare_single_keywords_allow_full_notes_only_for_real_prompts() {
        for keyword in ["150.000", "longkeyword", "widget", "zebra"] {
            let rare = keyword.chars().any(char::is_numeric) || keyword.chars().count() >= 8;
            for benchmark in [false, true] {
                let (mut fx, mut ev) = prompt_fixture();
                fx.items
                    .retain(|item| item.id == "note-0" || !is_note(item));
                fx.items[0].triggers.keywords = vec![keyword.into()];
                ev.prompt = Some(keyword.into());
                if benchmark {
                    ev.allowed_scopes.push("global".into());
                }
                let sel = E1.select(&fx, &ev, &mut SessionState::default());
                assert_eq!(sel.full_ids.contains(&"note-0".into()), !benchmark && rare);
                assert_eq!(
                    sel.pointer_ids.contains(&"note-0".into()),
                    benchmark || !rare
                );
            }
        }
    }

    #[test]
    fn image_only_prompts_use_top_unserved_project_rules() {
        let (mut fx, mut ev) = prompt_fixture();
        fx.config.projects = vec![crate::config::Project {
            name: "app".into(),
            root: "app".into(),
            aliases: vec![],
        }];
        ev.cwd = "app/src".into();
        ev.prompt = Some("[Image: source: /tmp/x/app/y.png] [Image #2]".into());
        let base = fx.items.last().unwrap().clone();
        fx.items = (0..5)
            .map(|n| {
                let mut item = base.clone();
                item.id = format!("rule-{n}");
                item.kind = if n == 3 { Kind::Law } else { Kind::Rule };
                item.scope = "project:app".into();
                item.importance = n as f64 / 5.0;
                item.triggers.keywords = vec!["app".into()];
                item
            })
            .collect();
        let mut state = SessionState::default();
        state.served.insert("rule-4".into());
        let sel = E1.select(&fx, &ev, &mut state);
        assert_eq!(sel.full_ids, ["rule-3", "rule-2", "rule-1"]);
        assert!(!sel.pointer_ids.contains(&"rule-4".into()));
        assert!(state.recent_prompts.is_empty());
        // History never hides the project rules of an image-only prompt.
        let mut with_history = SessionState::default();
        with_history.served.insert("rule-4".into());
        with_history.recent_prompts = vec!["corrigir o widget".into()];
        let mut note = base.clone();
        note.id = "note-widget".into();
        note.kind = Kind::Note;
        note.scope = "global".into();
        note.triggers.keywords = vec!["widget".into()];
        fx.items.push(note);
        let sel = E1.select(&fx, &ev, &mut with_history);
        fx.items.pop();
        assert_eq!(sel.full_ids, ["rule-3", "rule-2", "rule-1"]);
        assert!(!sel.pointer_ids.contains(&"note-widget".into()));
        let image_prompt = ev.prompt.clone();
        ev.prompt = Some("[Image #2]".into());
        ev.allowed_scopes.push("project:app".into());
        let legacy = E1.select(&fx, &ev, &mut state);
        assert!(legacy.full_ids.is_empty() && legacy.pointer_ids.is_empty());
        ev.allowed_scopes.clear();
        ev.event_type = "tool-use".into();
        let legacy = E1.select(&fx, &ev, &mut state);
        assert!(legacy.full_ids.is_empty() && legacy.pointer_ids.is_empty());
        ev.event_type = "user-prompt-submit".into();
        ev.prompt = image_prompt;
        fx.items[3].body = "x".repeat(200_000);
        let sel = E1.select(&fx, &ev, &mut state);
        assert!(sel.pointer_ids.contains(&"rule-3".into()));
        assert!(sel.used_bytes <= fx.config.workspace.budget_bytes as usize);
    }

    #[test]
    fn benchmark_and_other_events_keep_legacy_prompt_behavior() {
        for benchmark in [false, true] {
            let (mut fx, mut ev) = prompt_fixture();
            fx.items[0].triggers.keywords = vec!["widget".into()];
            if benchmark {
                ev.allowed_scopes.push("global".into());
            } else {
                ev.event_type = "tool-use".into();
            }
            let mut state = SessionState {
                recent_prompts: vec!["widget".into()],
                ..Default::default()
            };
            ev.prompt = Some("please do it".into());
            let sel = E1.select(&fx, &ev, &mut state);
            assert!(sel.full_ids.is_empty() && sel.pointer_ids.is_empty());
            assert_eq!(state.recent_prompts, ["widget"]);
            ev.prompt = Some("[Image: source: /tmp/widget/image.png]".into());
            let sel = E1.select(&fx, &ev, &mut state);
            assert!(sel.pointer_ids.contains(&"note-0".into()));
            ev.prompt = Some("[Image #2]".into());
            let sel = E1.select(&fx, &ev, &mut state);
            assert!(sel.full_ids.is_empty() && sel.pointer_ids.is_empty());
        }
    }

    fn note_pointers(sel: &Selection) -> BTreeSet<String> {
        sel.pointer_ids
            .iter()
            .filter(|id| id.starts_with("note-"))
            .cloned()
            .collect()
    }

    #[test]
    fn many_note_pointers_keep_only_the_best_five() {
        let (fx, ev) = note_fixture(0, false);
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        let expected: BTreeSet<String> = ["note-7", "note-6", "note-5", "note-4", "note-3"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(note_pointers(&sel), expected);
    }

    #[test]
    fn rule_pointers_below_the_cap_survive_with_note_pointers() {
        // With only two rule pointers in this fixture they survive alongside the best
        // note pointers; the cap is not exercised here.
        let (fx, ev) = note_fixture(2, false);
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert_eq!(note_pointers(&sel).len(), MAX_NOTE_POINTERS);
        assert!(sel.pointer_ids.contains(&"rule-0".to_string()));
        assert!(sel.pointer_ids.contains(&"rule-1".to_string()));
    }

    #[test]
    fn real_mode_caps_total_pointers_at_eight_with_twelve_rules() {
        // Enough rules to trigger the cap and force an omitted-pointers line.
        // Notes are removed and the budget is tight so every rule becomes a pointer.
        let (mut fx, mut ev) = note_fixture(12, false);
        fx.config.workspace.notes_budget_bytes = 0;
        fx.config.workspace.budget_bytes = 2_000;
        fx.items.retain(|i| !is_note(i));
        for item in &mut fx.items {
            item.body = "b".repeat(3_000);
        }
        ev.prompt = Some("zebra".into());
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert_eq!(sel.pointer_ids.len(), MAX_POINTERS);
        assert_eq!(sel.omitted_pointers, 4);
        let out = render(&fx, &ev, &sel);
        assert!(out.contains("+4 more → elo context"));
        assert!(sel.used_bytes <= fx.config.workspace.budget_bytes as usize);
    }

    #[test]
    fn benchmark_mode_keeps_the_old_pointer_policy() {
        let (fx, ev) = note_fixture(0, true);
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert_eq!(note_pointers(&sel).len(), 8);
    }

    #[test]
    fn served_law_reminder_is_skipped_in_real_mode_and_returns_after_compaction() {
        let (mut fx, mut ev) = prompt_fixture();
        fx.config.workspace.budget_bytes = 10_000;
        let mut law = fx.items[0].clone();
        law.id = "L1".into();
        law.kind = Kind::Law;
        law.scope = "global".into();
        law.body = "law body".into();
        law.triggers.keywords = vec!["kw".into()];
        law.triggers.paths.clear();
        let mut rule = fx.items[0].clone();
        rule.id = "R1".into();
        rule.kind = Kind::Rule;
        rule.scope = "global".into();
        rule.body = "rule body".into();
        rule.triggers.keywords = vec!["kw".into()];
        rule.triggers.paths.clear();
        fx.items = vec![law.clone(), rule];
        ev.prompt = Some("kw".into());
        let mut state = SessionState::default();
        let sel1 = E1.select(&fx, &ev, &mut state);
        assert!(sel1.full_ids.contains(&law.id));
        state.record(&ev, &sel1);
        ev.event_seq = 2;
        let sel2 = E1.select(&fx, &ev, &mut state);
        assert!(!sel2.full_ids.contains(&law.id));
        assert!(!sel2.pointer_ids.contains(&law.id));
        ev.event_type = "compaction".into();
        state.begin_event(&ev);
        ev.event_type = "user-prompt-submit".into();
        ev.event_seq = 3;
        let sel3 = E1.select(&fx, &ev, &mut state);
        assert!(sel3.full_ids.contains(&law.id));
    }

    #[test]
    fn served_full_rule_is_not_pointed_again_in_real_mode() {
        let (mut fx, mut ev) = note_fixture(1, false);
        fx.config.workspace.budget_bytes = 10_000;
        let mut rule = fx
            .items
            .iter()
            .find(|i| !is_note(i) && i.kind != Kind::Law)
            .cloned()
            .unwrap();
        rule.body = "rule body".into();
        rule.triggers.keywords = vec!["kw".into()];
        fx.items.retain(|i| i.id == rule.id || is_note(i));
        for item in fx.items.iter_mut() {
            if !is_note(item) {
                item.body = "rule body".into();
                item.triggers.keywords = vec!["kw".into()];
            }
        }
        ev.prompt = Some("kw".into());
        let mut state = SessionState::default();
        let sel1 = E1.select(&fx, &ev, &mut state);
        assert!(sel1.full_ids.contains(&rule.id));
        state.record(&ev, &sel1);
        ev.event_seq = 2;
        let sel2 = E1.select(&fx, &ev, &mut state);
        assert!(!sel2.pointer_ids.contains(&rule.id));
        assert!(!sel2.full_ids.contains(&rule.id));
    }

    #[test]
    fn repeated_pointer_is_suppressed_within_window_except_on_two_keywords() {
        let (mut fx, mut ev) = note_fixture(0, false);
        // Keep the note as a pointer by giving it an oversized body and a tight budget.
        fx.config.workspace.budget_bytes = 500;
        fx.config.workspace.full_notes_max = 0;
        let mut note = fx.items.iter().find(|i| is_note(i)).cloned().unwrap();
        note.triggers.keywords = vec!["alpha".into(), "beta".into()];
        note.body = "b".repeat(1000);
        fx.items = vec![note.clone()];
        ev.prompt = Some("alpha".into());
        let mut state = SessionState::default();
        let sel1 = E1.select(&fx, &ev, &mut state);
        assert!(sel1.pointer_ids.contains(&note.id));
        state.record(&ev, &sel1);
        ev.event_seq = 2;
        let sel2 = E1.select(&fx, &ev, &mut state);
        assert!(!sel2.pointer_ids.contains(&note.id));
        ev.prompt = Some("alpha beta".into());
        let sel3 = E1.select(&fx, &ev, &mut state);
        assert!(sel3.pointer_ids.contains(&note.id));
        state.record(&ev, &sel3);
        ev.prompt = Some("alpha".into());
        ev.event_seq = 22;
        let sel4 = E1.select(&fx, &ev, &mut state);
        assert!(sel4.pointer_ids.contains(&note.id));
    }

    #[test]
    fn real_mode_caps_total_pointers_at_eight_and_shows_omitted_line() {
        let (mut fx, mut ev) = note_fixture(1, false);
        // Small budget forces every rule into a pointer; 30 rules become 8 kept + 22 omitted.
        fx.config.workspace.budget_bytes = 2_000;
        fx.config.workspace.notes_budget_bytes = 0;
        let base = fx
            .items
            .iter()
            .find(|i| !is_note(i) && i.kind != Kind::Law)
            .cloned()
            .unwrap();
        fx.items = (0..30)
            .map(|n| {
                let mut it = base.clone();
                it.id = format!("rule-{n:02}");
                it.kind = Kind::Rule;
                it.scope = "global".into();
                it.triggers.keywords = vec!["zebra".into()];
                it.triggers.paths.clear();
                it.body = "b".repeat(3_000);
                it
            })
            .collect();
        ev.prompt = Some("zebra".into());
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert_eq!(sel.pointer_ids.len(), MAX_POINTERS);
        assert_eq!(sel.omitted_pointers, 22);
        let out = render(&fx, &ev, &sel);
        assert!(out.contains("+22 more → elo context"));
    }

    #[test]
    fn candidate_items_follows_score_order_not_alphabetical() {
        // Regression for the zip bug: score_of.keys() is alphabetical while scored is by
        // score. Two candidates with different scores and ids out of alphabetical order:
        // the higher-scored one was pointed 5 events ago and must be suppressed, the
        // never-pointed one must appear. The old zip mapped ids to the wrong items.
        let (mut fx, mut ev) = note_fixture(0, false);
        fx.config.workspace.budget_bytes = 500;
        fx.config.workspace.full_notes_max = 0;
        let mut high = fx.items.iter().find(|i| is_note(i)).cloned().unwrap();
        high.id = "z-high".into(); // alphabetically after the other item
        high.triggers.keywords = vec!["alpha".into()];
        high.importance = 0.9;
        high.body = "b".repeat(1_000);
        let mut low = high.clone();
        low.id = "a-low".into();
        low.importance = 0.5;
        low.updated_at = "2025-01-01T00:00:00Z".into();
        fx.items = vec![high, low];
        ev.prompt = Some("alpha".into());
        let mut state = SessionState::default();
        state.pointed.insert("z-high".into(), 1);
        ev.event_seq = 6;
        let sel2 = E1.select(&fx, &ev, &mut state);
        assert!(!sel2.pointer_ids.contains(&"z-high".to_string()));
        assert!(sel2.pointer_ids.contains(&"a-low".to_string()));
    }

    #[test]
    fn real_mode_trim_pops_pointers_and_respects_budget() {
        // Force at least one pop: many pointers, tiny budget.
        let (mut fx, mut ev) = note_fixture(12, false);
        fx.config.workspace.notes_budget_bytes = 0;
        fx.config.workspace.budget_bytes = 1_400;
        fx.items.retain(|i| !is_note(i));
        for item in &mut fx.items {
            item.body = "b".repeat(3_000);
        }
        ev.prompt = Some("zebra".into());
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert!(sel.used_bytes <= fx.config.workspace.budget_bytes as usize);
        assert!(sel.omitted_pointers >= 1);
        let out = render(&fx, &ev, &sel);
        assert!(out.contains("more → elo context"));
    }

    #[test]
    fn real_mode_trim_zeros_omitted_when_notice_does_not_fit() {
        // Tight budget where even the omission notice would overflow, so the trim
        // prefers silence (omitted_pointers = 0) and still respects the hard budget.
        let (mut fx, mut ev) = note_fixture(2, false);
        fx.config.workspace.notes_budget_bytes = 0;
        fx.config.workspace.budget_bytes = 1_350;
        fx.items.retain(|i| !is_note(i));
        for item in &mut fx.items {
            item.body = "b".repeat(3_000);
        }
        ev.prompt = Some("zebra".into());
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert!(sel.used_bytes <= fx.config.workspace.budget_bytes as usize);
        assert_eq!(sel.omitted_pointers, 0);
    }

    #[test]
    fn benchmark_mode_does_not_trim_or_emit_omission_line() {
        // Benchmark must keep the legacy output, so trim is a no-op even when the
        // budget is exceeded.
        let (mut fx, mut ev) = note_fixture(12, true);
        fx.config.workspace.notes_budget_bytes = 0;
        fx.config.workspace.budget_bytes = 1_500;
        fx.items.retain(|i| !is_note(i));
        for item in &mut fx.items {
            item.body = "b".repeat(3_000);
        }
        ev.prompt = Some("zebra".into());
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert_eq!(sel.omitted_pointers, 0);
        let out = render(&fx, &ev, &sel);
        assert!(!out.contains("more → elo context"));
    }

    #[test]
    fn cross_project_pointers_rank_below_eligible_project_rules() {
        // Nine eligible project rules plus one cross-project pointer, each with two
        // keyword matches. The cross-project item scores lower than otherwise equally
        // matched eligible rules and is the one dropped by the cap.
        let (mut fx, mut ev) = note_fixture(1, false);
        fx.config.workspace.notes_budget_bytes = 0;
        fx.config.workspace.budget_bytes = 2_000;
        let base = fx
            .items
            .iter()
            .find(|i| !is_note(i) && i.kind != Kind::Law)
            .cloned()
            .unwrap();
        let mut words: Vec<String> = Vec::new();
        fx.items = (0..9)
            .map(|n| {
                let a = format!("kw{n}a");
                let b = format!("kw{n}b");
                let mut it = base.clone();
                it.id = format!("proj-{n}");
                it.kind = Kind::Rule;
                it.scope = "global".into();
                it.triggers.keywords = vec![a.clone(), b.clone()];
                it.triggers.paths.clear();
                it.body = "b".repeat(3_000);
                it
            })
            .collect();
        for n in 0..9 {
            words.push(format!("kw{n}a"));
            words.push(format!("kw{n}b"));
        }
        let mut cross = base.clone();
        cross.id = "cross-1".into();
        cross.kind = Kind::Rule;
        cross.scope = "project:other".into();
        cross.triggers.keywords = vec!["zebra".into(), "herd".into()];
        cross.triggers.paths.clear();
        cross.body = "b".repeat(3_000);
        fx.items.push(cross);
        words.extend(["zebra".into(), "herd".into()]);
        ev.prompt = Some(words.join(" "));
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert_eq!(sel.pointer_ids.len(), MAX_POINTERS);
        // The cross-project pointer is the lowest-ranked and is dropped.
        assert!(!sel.pointer_ids.contains(&"cross-1".to_string()));
        assert_eq!(sel.omitted_pointers, 2);
    }

    #[test]
    fn final_render_fits_budget_and_recalculates_used_bytes() {
        // Tight budget where the omitted-pointers line itself would push us over.
        let (mut fx, mut ev) = note_fixture(12, false);
        fx.config.workspace.notes_budget_bytes = 0;
        fx.config.workspace.budget_bytes = 1_500;
        let base = fx
            .items
            .iter()
            .find(|i| !is_note(i) && i.kind != Kind::Law)
            .cloned()
            .unwrap();
        let mut items = Vec::new();
        for n in 0..12 {
            let mut it = base.clone();
            it.id = format!("rule-{n:02}");
            it.kind = Kind::Rule;
            it.scope = "global".into();
            it.triggers.keywords = vec!["zebra".into()];
            it.triggers.paths.clear();
            it.body = "b".repeat(3_000);
            items.push(it);
        }
        fx.items = items;
        ev.prompt = Some("zebra".into());
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert!(sel.used_bytes <= fx.config.workspace.budget_bytes as usize);
        assert!(sel.used_bytes == render(&fx, &ev, &sel).len());
    }

    #[test]
    fn benchmark_mode_keeps_legacy_served_law_reminder_and_served_pointer() {
        // Served law still emits the one-line legacy reminder in benchmark mode, and a
        // served item still becomes a pointer.
        let (mut fx, mut ev) = note_fixture(1, true);
        fx.config.workspace.budget_bytes = 10_000;
        let mut law = fx.items[0].clone();
        law.id = "L1".into();
        law.kind = Kind::Law;
        law.scope = "global".into();
        law.body = "law body".into();
        law.triggers.keywords = vec!["kw".into()];
        law.triggers.paths.clear();
        let mut rule = fx.items[0].clone();
        rule.id = "R1".into();
        rule.kind = Kind::Rule;
        rule.scope = "global".into();
        rule.body = "rule body".into();
        rule.triggers.keywords = vec!["kw".into()];
        rule.triggers.paths.clear();
        fx.items = vec![law.clone(), rule];
        ev.prompt = Some("kw".into());
        let mut state = SessionState::default();
        let sel1 = E1.select(&fx, &ev, &mut state);
        assert!(sel1.full_ids.contains(&law.id));
        state.record(&ev, &sel1);
        ev.event_seq = 2;
        let sel2 = E1.select(&fx, &ev, &mut state);
        let out = render(&fx, &ev, &sel2);
        assert!(out.contains("[law] L1 (in force)") || out.contains("L1"));
        assert!(sel2.pointer_ids.contains(&"R1".to_string()));
    }

    #[test]
    fn old_session_state_without_pointed_loads() {
        let state: SessionState = serde_json::from_str(r#"{"served":[],"touched":[]}"#).unwrap();
        assert!(state.pointed.is_empty());
    }

    /// `ages` (hours before `ev.now`) become decisions `dec-0..` on the common word "widget".
    fn recent_fixture(ages: &[i64]) -> (Fixture, Event) {
        let (mut fx, mut ev) = prompt_fixture();
        fx.config.workspace.full_notes_max = 2;
        let base = fx.items[0].clone();
        let now = DateTime::parse_from_rfc3339(&ev.now).unwrap();
        fx.items = ages
            .iter()
            .enumerate()
            .map(|(n, hours)| {
                let mut item = base.clone();
                item.id = format!("dec-{n}");
                item.kind = Kind::Decision;
                item.triggers.keywords = vec!["widget".into()];
                item.updated_at = (now - chrono::Duration::hours(*hours)).to_rfc3339();
                item
            })
            .collect();
        ev.prompt = Some("o que falta para melhorar o widget".into());
        (fx, ev)
    }

    #[test]
    fn recent_decision_with_one_common_word_stays_a_pointer() {
        let (fx, ev) = recent_fixture(&[10]);
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert!(sel.full_ids.is_empty());
        assert_eq!(sel.pointer_ids, ["dec-0"]);
    }

    #[test]
    fn recent_decision_requires_a_specific_word_or_two_words() {
        for words in [vec!["widget42"], vec!["payments"], vec!["widget", "view"]] {
            let (mut fx, mut ev) = recent_fixture(&[10]);
            fx.items[0].triggers.keywords = words.iter().map(|s| (*s).into()).collect();
            ev.prompt = Some(words.join(" "));
            let sel = E1.select(&fx, &ev, &mut SessionState::default());
            assert_eq!(sel.full_ids, ["dec-0"]);
            assert!(sel.pointer_ids.is_empty());
        }
    }

    #[test]
    fn the_same_decision_five_days_old_stays_a_pointer() {
        let (fx, ev) = recent_fixture(&[5 * 24]);
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert!(sel.full_ids.is_empty());
        assert_eq!(sel.pointer_ids, ["dec-0"]);
    }

    #[test]
    fn recent_decisions_with_a_common_word_stay_as_pointers() {
        let (fx, ev) = recent_fixture(&[30, 5, 20, 1]);
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        let full: BTreeSet<_> = sel.full_ids.iter().cloned().collect();
        let pointers: BTreeSet<_> = sel.pointer_ids.iter().cloned().collect();
        assert!(full.is_empty());
        assert_eq!(
            pointers,
            ["dec-0", "dec-1", "dec-2", "dec-3"]
                .map(String::from)
                .into()
        );
    }

    #[test]
    fn a_match_only_through_the_history_gets_no_exception() {
        let (fx, mut ev) = recent_fixture(&[10]);
        let mut state = SessionState {
            recent_prompts: vec!["corrigir o widget".into()],
            ..Default::default()
        };
        ev.prompt = Some("pode fazer o 2".into());
        let sel = E1.select(&fx, &ev, &mut state);
        assert!(sel.full_ids.is_empty());
        assert_eq!(sel.pointer_ids, ["dec-0"]);
    }

    #[test]
    fn benchmark_mode_ignores_the_recent_decision_exception() {
        let (fx, mut ev) = recent_fixture(&[10]);
        ev.allowed_scopes.push("global".into());
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert!(sel.full_ids.is_empty());
        assert_eq!(sel.pointer_ids, ["dec-0"]);
    }
}

#[cfg(test)]
mod scope_v2_tests {
    use super::*;
    #[test]
    fn workspace_root_and_equivalent_depth() {
        let config = crate::config::Config::parse(
            "[[project]]\nname='workspace'\nroot='.'\n[[project]]\nname='first'\nroot='./a/b/'\n[[project]]\nname='last'\nroot='a/b'",
            std::path::Path::new("test"),
        ).unwrap();
        let mut fx = super::super::load_fixture("bench/fixture/v0").unwrap();
        fx.config = config;
        let mut ev = fx.events[0].clone();
        ev.cwd = ".".into();
        let scope = resolve(&fx, &ev, &[], "");
        assert_eq!(scope.primary.as_deref(), Some("workspace"));
        assert_eq!(scope.projects.len(), 1);
        ev.cwd = "other/src".into();
        assert_eq!(
            resolve(&fx, &ev, &[], "").primary.as_deref(),
            Some("workspace")
        );
        ev.cwd = "a/b/src".into();
        assert_eq!(resolve(&fx, &ev, &[], "").primary.as_deref(), Some("last"));
        fx.config.projects[1].root = "a/b".into();
        assert_eq!(resolve(&fx, &ev, &[], "").primary.as_deref(), Some("last"));
    }

    #[test]
    fn pasta_mae_nao_vira_primaria_e_raiz_vazia_nao_ativa() {
        let mut fx = super::super::load_fixture("bench/fixture/v0").unwrap();
        fx.config.projects = [
            ("pai", "Exemplo-Sistema"),
            ("app", "Exemplo-Sistema/app-exemplo"),
            ("vazio", ""),
        ]
        .into_iter()
        .map(|(name, root)| crate::config::Project {
            name: name.into(),
            root: root.into(),
            aliases: vec![],
        })
        .collect();
        let mut ev = fx.events[0].clone();
        ev.cwd = "Exemplo-Sistema".into();
        let scope = resolve(&fx, &ev, &[], "");
        assert_eq!(scope.primary.as_deref(), Some("pai"));
        assert!(scope.projects.contains("app"));
        assert!(!scope.projects.contains("vazio"));
        ev.cwd = "Exemplo-Sistema/app-exemplo/src".into();
        assert_eq!(resolve(&fx, &ev, &[], "").primary.as_deref(), Some("app"));
        ev.cwd = ".".into();
        let scope = resolve(&fx, &ev, &[], "");
        assert!(scope.primary.is_none());
        assert_eq!(scope.projects.len(), 0);
        ev.cwd = "".into();
        assert!(resolve(&fx, &ev, &[], "").projects.is_empty());
        ev.cwd = "Exemplo".into();
        assert!(resolve(&fx, &ev, &[], "").projects.is_empty());
    }
}
