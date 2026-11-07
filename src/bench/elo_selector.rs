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

impl Selector for E1 {
    fn name(&self) -> &str {
        "E1"
    }

    fn select(&self, fx: &Fixture, ev: &Event, state: &mut SessionState) -> Selection {
        let text = prose(ev.prompt.as_deref().unwrap_or(""));
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
        let now = DateTime::parse_from_rfc3339(&ev.now).ok();

        let mut indexed = BTreeSet::new();
        let mut laws: Vec<&Item> = Vec::new();
        let mut scored: Vec<(f64, &Item)> = Vec::new();
        for item in fx
            .items
            .iter()
            .filter(|i| eligible(&scope, i) && is_current(i, &ev.now))
        {
            let kw: Vec<&String> = item
                .triggers
                .keywords
                .iter()
                .filter(|k| has_word(&text, k))
                .collect();
            let path_hit = item
                .triggers
                .paths
                .iter()
                .any(|p| touched.iter().any(|t| path_matches(p, t)));
            let always = item.triggers.always
                && !crate::stats::demoted(item, fx.config.workspace.demote_bad_min);
            if kw.is_empty() && !path_hit && !always {
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
            {
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
            ("app", "Exemplo-Sistema/app-face"),
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
        ev.cwd = "Exemplo-Sistema/app-face/src".into();
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
