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
const HALF_LIFE_DAYS: f64 = 90.0;
const W_PROXIMITY: f64 = 0.4;
const W_RECENCY: f64 = 0.1;
const W_IMPORTANCE: f64 = 0.2;
const W_MATCH: f64 = 0.3;

/// Cap on note/decision pointers: past five, index lines crowd out the answer.
const MAX_NOTE_POINTERS: usize = 5;
/// A note/decision updated at most this long ago (seconds) may come in full on one common word.
const RECENT_NOTE_SECS: i64 = 48 * 3600;
/// At most this many recent single-word notes per prompt get that exception.
const MAX_RECENT_NOTES: usize = 2;

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
        // Recent single-word notes that may skip the index (at most `MAX_RECENT_NOTES`).
        let mut recent_notes: Vec<&Item> = Vec::new();
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
                // Real prompts: a note/decision updated in the last 48 h that matched the
                // current text (not only the history) may still come in full (top 2 only).
                let recent = real_prompt
                    && kw.iter().any(|k| has_word(&text, k))
                    && match (now, DateTime::parse_from_rfc3339(&item.updated_at)) {
                        (Some(now), Ok(upd)) => {
                            (0..RECENT_NOTE_SECS).contains(&(now - upd).num_seconds())
                        }
                        _ => false,
                    };
                if recent {
                    recent_notes.push(item);
                } else {
                    indexed.insert(item.id.clone());
                }
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

        // Only the most recent ones keep the exception; the rest go back to the index.
        recent_notes.sort_by(|a, b| {
            DateTime::parse_from_rfc3339(&b.updated_at)
                .ok()
                .cmp(&DateTime::parse_from_rfc3339(&a.updated_at).ok())
                .then_with(|| a.id.cmp(&b.id))
        });
        for item in recent_notes.iter().skip(MAX_RECENT_NOTES) {
            indexed.insert(item.id.clone());
        }

        let budget = usize::try_from(fx.config.workspace.budget_bytes).unwrap_or(usize::MAX);
        let mut full: Vec<String> = Vec::new();
        let fits = |ids: &[String], ptr: &[String]| {
            Selection::new(fx, ev, ids.to_vec(), ptr.to_vec()).used_bytes <= budget
        };
        // Laws first; a law already delivered this session is not repeated (SPEC §4.4).
        laws.sort_by(|a, b| a.id.cmp(&b.id));
        let mut law_reminders: Vec<String> = Vec::new();
        for law in laws {
            if state.served.contains(&law.id) {
                // SPEC §4.4: `[law] L3 (in force)` instead of the body again.
                law_reminders.push(law.id.clone());
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
                if indexed.contains(&item.id) || state.served.contains(&item.id) {
                    // Item outside the focus or already delivered: uses only the index line.
                    pointers.push(item.id.clone());
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
        for (item, _) in cross_pointer_entries {
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
        if real_prompt && ev.prompt.is_some() && !text.trim().is_empty() {
            let mut end = text.len().min(400);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            state.recent_prompts.push(text[..end].to_string());
            let excess = state.recent_prompts.len().saturating_sub(2);
            state.recent_prompts.drain(..excess);
        }
        selection.used_bytes = render(fx, ev, &selection).len();
        selection
    }
}

#[cfg(test)]
mod tests {
    use super::{has_word, prose, safe_relative, under};

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
    fn rule_pointers_are_never_capped() {
        let (fx, ev) = note_fixture(2, false);
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert_eq!(note_pointers(&sel).len(), MAX_NOTE_POINTERS);
        assert!(sel.pointer_ids.contains(&"rule-0".to_string()));
        assert!(sel.pointer_ids.contains(&"rule-1".to_string()));
    }

    #[test]
    fn benchmark_mode_keeps_the_old_pointer_policy() {
        let (fx, ev) = note_fixture(0, true);
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert_eq!(note_pointers(&sel).len(), 8);
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
    fn a_decision_from_10_hours_ago_comes_in_full_on_one_common_word() {
        let (fx, ev) = recent_fixture(&[10]);
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert_eq!(sel.full_ids, ["dec-0"]);
        assert!(sel.pointer_ids.is_empty());
    }

    #[test]
    fn the_same_decision_five_days_old_stays_a_pointer() {
        let (fx, ev) = recent_fixture(&[5 * 24]);
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        assert!(sel.full_ids.is_empty());
        assert_eq!(sel.pointer_ids, ["dec-0"]);
    }

    #[test]
    fn only_the_two_newest_recent_decisions_get_the_exception() {
        let (fx, ev) = recent_fixture(&[30, 5, 20, 1]);
        let sel = E1.select(&fx, &ev, &mut SessionState::default());
        let full: BTreeSet<_> = sel.full_ids.iter().cloned().collect();
        let pointers: BTreeSet<_> = sel.pointer_ids.iter().cloned().collect();
        assert_eq!(full, ["dec-1", "dec-3"].map(String::from).into());
        assert_eq!(pointers, ["dec-0", "dec-2"].map(String::from).into());
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
