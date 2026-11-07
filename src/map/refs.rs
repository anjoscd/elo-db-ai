//! Reference queries: who calls whom, who imports a file, impact (SPEC §7).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use rusqlite::{params, Connection};
use serde::Serialize;

use super::store::{like_escape, MapStore};
use super::{Kind, MapError};

const CALL_KINDS: [&str; 3] = ["calls", "renders", "registers_callback"];

/// A resolved reference.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub level: usize,
    pub file: String,
    pub line: u32,
    pub edge: String,
    /// The entity that contains the reference (callback plus its container).
    pub who: String,
    pub who_kind: String,
    pub to: Option<String>,
    pub to_file: Option<String>,
}

/// A reference that could not be resolved, with the reason.
#[derive(Debug, Clone, Serialize)]
pub struct Miss {
    pub level: usize,
    pub file: String,
    pub line: u32,
    pub edge: String,
    pub name: String,
    pub reason: String,
    pub detail: String,
    pub who: String,
}

#[derive(Debug, Serialize)]
pub struct RefReport {
    pub query: String,
    pub mode: &'static str,
    pub targets: Vec<String>,
    pub resolved: Vec<Hit>,
    pub unresolved: Vec<Miss>,
    pub resolved_count: usize,
    pub unresolved_count: usize,
    pub coverage: Option<f64>,
    pub stale: Vec<String>,
}

#[derive(Clone)]
struct Row {
    id: String,
    kind: Kind,
    name: String,
    file: String,
    line: u32,
    parent_id: Option<String>,
}

struct Db<'a> {
    conn: &'a Connection,
    cache: HashMap<String, Option<Row>>,
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn row_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let kind: String = row.get(1)?;
    Ok(Row {
        id: row.get(0)?,
        kind: Kind::parse(&kind).unwrap_or(Kind::Function),
        name: row.get(2)?,
        file: row.get(3)?,
        line: row.get::<_, i64>(4)? as u32,
        parent_id: row.get(5)?,
    })
}

const ROW_COLUMNS: &str = "id, kind, name, file, line, parent_id";

impl Db<'_> {
    fn ent(&mut self, id: &str) -> Result<Option<Row>, MapError> {
        if let Some(found) = self.cache.get(id) {
            return Ok(found.clone());
        }
        let found = self
            .conn
            .prepare_cached(&format!("SELECT {ROW_COLUMNS} FROM entities WHERE id = ?1"))?
            .query_map(params![id], row_of)?
            .next()
            .transpose()?;
        self.cache.insert(id.to_string(), found.clone());
        Ok(found)
    }

    fn parent(&mut self, row: &Row) -> Result<Option<Row>, MapError> {
        match &row.parent_id {
            Some(id) => self.ent(id),
            None => Ok(None),
        }
    }

    /// `name (in Container › Outer)`; a top-level callback is "in <file>".
    fn label(&mut self, row: &Row) -> Result<String, MapError> {
        if row.kind == Kind::File {
            return Ok(format!("(top level of {})", row.name));
        }
        let mut chain = Vec::new();
        let mut current = self.parent(row)?;
        while let Some(parent) = current {
            chain.push(parent.name.clone());
            if chain.len() > 8 {
                break;
            }
            current = self.parent(&parent)?;
        }
        if chain.is_empty() {
            return Ok(if row.kind == Kind::Callback {
                format!("{} (in {})", row.name, basename(&row.file))
            } else {
                row.name.clone()
            });
        }
        Ok(format!("{} (in {})", row.name, chain.join(" › ")))
    }

    /// Nearest enclosing entity that is not a callback.
    fn named_ancestor(&mut self, row: &Row) -> Result<Option<Row>, MapError> {
        let mut current = self.parent(row)?;
        while let Some(parent) = current {
            if parent.kind != Kind::Callback {
                return Ok(Some(parent));
            }
            current = self.parent(&parent)?;
        }
        Ok(None)
    }

    fn targets(&self, name: &str) -> Result<Vec<Row>, MapError> {
        let (file_filter, symbol) = match name.rsplit_once(':') {
            Some((file, symbol)) if !file.is_empty() && !symbol.is_empty() => (Some(file), symbol),
            _ => (None, name),
        };
        let mut found = self.entities_named(symbol)?;
        if found.is_empty() {
            if let Some((class, method)) = symbol.rsplit_once('.') {
                found = self
                    .entities_named(method)?
                    .into_iter()
                    .filter(|r| r.kind == Kind::Method)
                    .collect();
                let mut kept = Vec::new();
                for row in found {
                    let parent_matches = match &row.parent_id {
                        Some(id) => self
                            .conn
                            .query_row(
                                "SELECT name FROM entities WHERE id = ?1",
                                params![id],
                                |r| r.get::<_, String>(0),
                            )
                            .map(|n| n == class)
                            .unwrap_or(false),
                        None => false,
                    };
                    if parent_matches {
                        kept.push(row);
                    }
                }
                found = kept;
            }
        }
        if let Some(filter) = file_filter {
            found.retain(|r| r.file == filter || r.file.ends_with(&format!("/{filter}")));
        }
        found.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
        Ok(found)
    }

    /// Entities named `name`; a callback also matches without its `@L<line>` suffix.
    fn entities_named(&self, name: &str) -> Result<Vec<Row>, MapError> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {ROW_COLUMNS} FROM entities WHERE name = ?1 OR (kind = 'callback' AND name LIKE ?2 ESCAPE '\\') ORDER BY file, line LIMIT 500"
        ))?;
        let rows = stmt.query_map(params![name, format!("{}@L%", like_escape(name))], row_of)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Unresolved references whose last name segment is `name`.
    fn unresolved_named(&self, name: &str) -> Result<Vec<Miss>, MapError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT file, kind, name, line, owner_id, reason, detail FROM unresolved \
             WHERE (name = ?1 OR substr(name, -(length(?1) + 1)) = '.' || ?1) \
             AND kind IN ('calls', 'renders', 'registers_callback') \
             ORDER BY file, line LIMIT 2000",
        )?;
        let rows = stmt.query_map(params![name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        let mut raw = Vec::new();
        for row in rows {
            raw.push(row?);
        }
        let mut out = Vec::new();
        for (file, kind, name, line, owner_id, reason, detail) in raw {
            out.push(Miss {
                level: 1,
                file,
                line: line as u32,
                edge: kind,
                name,
                reason,
                detail,
                who: owner_id.unwrap_or_default(),
            });
        }
        Ok(out)
    }

    fn describe_miss_owner(&mut self, miss: &mut Miss) -> Result<(), MapError> {
        if miss.who.is_empty() {
            miss.who = format!("(top level of {})", basename(&miss.file));
            return Ok(());
        }
        let id = miss.who.clone();
        miss.who = match self.ent(&id)? {
            Some(row) => self.label(&row)?,
            None => String::new(),
        };
        Ok(())
    }
}

fn target_label(row: &Row) -> String {
    format!(
        "{}:{} {} {}",
        row.file,
        row.line,
        row.kind.as_str(),
        row.name
    )
}

/// Counts are over the query's own scope: the lists that the report shows.
fn finish(root: &Path, store: &MapStore, mut report: RefReport) -> Result<RefReport, MapError> {
    let mut files: Vec<String> = report
        .resolved
        .iter()
        .map(|h| h.file.clone())
        .chain(report.unresolved.iter().map(|m| m.file.clone()))
        .collect();
    files.sort();
    files.dedup();
    let resolved = report.resolved.len();
    let unresolved = report.unresolved.len();
    report.resolved_count = resolved;
    report.unresolved_count = unresolved;
    report.coverage = if resolved + unresolved == 0 {
        None
    } else {
        Some(resolved as f64 / (resolved + unresolved) as f64)
    };
    report.stale = store.stale_among(root, &files)?;
    Ok(report)
}

fn new_report(query: &str, mode: &'static str) -> RefReport {
    RefReport {
        query: query.to_string(),
        mode,
        targets: Vec::new(),
        resolved: Vec::new(),
        unresolved: Vec::new(),
        resolved_count: 0,
        unresolved_count: 0,
        coverage: None,
        stale: Vec::new(),
    }
}

/// Edges that point at `dst_id`: (kind, src_id, line).
fn edges_into(
    conn: &Connection,
    dst_id: &str,
    kinds: &[&str],
) -> Result<Vec<(String, String, String, u32)>, MapError> {
    let mut stmt = conn.prepare_cached(
        "SELECT kind, src_id, src_file, line FROM edges WHERE dst_id = ?1 ORDER BY src_file, line",
    )?;
    let rows = stmt.query_map(params![dst_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)? as u32,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        if kinds.contains(&row.0.as_str()) {
            out.push(row);
        }
    }
    Ok(out)
}

/// Who calls `name` (direct, includes calls made inside callbacks).
pub fn calls(root: &Path, name: &str) -> Result<RefReport, MapError> {
    let store = MapStore::open_read(root)?;
    let mut db = Db {
        conn: store.db(),
        cache: HashMap::new(),
    };
    let mut report = new_report(name, "calls");
    let targets = db.targets(name)?;
    report.targets = targets.iter().map(target_label).collect();
    let mut seen = HashSet::new();
    for target in &targets {
        for (edge, src_id, src_file, line) in edges_into(db.conn, &target.id, &CALL_KINDS)? {
            if !seen.insert((edge.clone(), src_id.clone(), line, target.id.clone())) {
                continue;
            }
            let Some(src) = db.ent(&src_id)? else {
                continue;
            };
            report.resolved.push(Hit {
                level: 1,
                file: src_file,
                line,
                edge,
                who: db.label(&src)?,
                who_kind: src.kind.as_str().to_string(),
                to: Some(target.name.clone()),
                to_file: Some(target.file.clone()),
            });
        }
    }
    report.unresolved = db.unresolved_named(symbol_of(name))?;
    for miss in &mut report.unresolved {
        db.describe_miss_owner(miss)?;
    }
    report
        .resolved
        .sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    finish(root, &store, report)
}

fn symbol_of(name: &str) -> &str {
    let symbol = match name.rsplit_once(':') {
        Some((file, symbol)) if !file.is_empty() && !symbol.is_empty() => symbol,
        _ => name,
    };
    symbol.rsplit_once('.').map_or(symbol, |(_, last)| last)
}

/// Everything `name` calls, including calls inside its callbacks.
pub fn callees(root: &Path, name: &str) -> Result<RefReport, MapError> {
    let store = MapStore::open_read(root)?;
    let mut db = Db {
        conn: store.db(),
        cache: HashMap::new(),
    };
    let mut report = new_report(name, "callees");
    let targets = db.targets(name)?;
    report.targets = targets.iter().map(target_label).collect();
    let mut source_ids: HashSet<String> = HashSet::new();
    for target in &targets {
        let mut stmt = db
            .conn
            .prepare_cached("SELECT id, parent_id FROM entities WHERE file = ?1")?;
        let rows = stmt.query_map(params![target.file], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut children: HashMap<Option<String>, Vec<String>> = HashMap::new();
        for row in rows {
            let (id, parent) = row?;
            children.entry(parent).or_default().push(id);
        }
        let mut stack = vec![target.id.clone()];
        if target.kind == Kind::File {
            stack.extend(children.get(&None).cloned().unwrap_or_default());
        }
        while let Some(id) = stack.pop() {
            if !source_ids.insert(id.clone()) {
                continue;
            }
            if let Some(next) = children.get(&Some(id)) {
                stack.extend(next.iter().cloned());
            }
        }
    }
    for target in &targets {
        let mut stmt = db.conn.prepare_cached(
            "SELECT kind, src_id, dst_id, dst_file, line FROM edges WHERE src_file = ?1 AND kind IN ('calls', 'renders', 'registers_callback') ORDER BY line",
        )?;
        let rows = stmt.query_map(params![target.file], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)? as u32,
            ))
        })?;
        let mut edges = Vec::new();
        for row in rows {
            edges.push(row?);
        }
        for (edge, src_id, dst_id, dst_file, line) in edges {
            if !source_ids.contains(&src_id) {
                continue;
            }
            let Some(dst) = db.ent(&dst_id)? else {
                continue;
            };
            // a callback's registration inside its own container is structure, not a call
            if dst.kind == Kind::Callback && dst.parent_id.as_deref().unwrap_or("") == src_id {
                continue;
            }
            let src = db.ent(&src_id)?;
            let who = match &src {
                Some(row) => db.label(row)?,
                None => String::new(),
            };
            report.resolved.push(Hit {
                level: 1,
                file: target.file.clone(),
                line,
                edge,
                who,
                who_kind: src.map_or("function".to_string(), |r| r.kind.as_str().to_string()),
                to: Some(format!("{} ({}:{})", dst.name, dst_file, dst.line)),
                to_file: Some(dst_file),
            });
        }
        let mut stmt = db.conn.prepare_cached(
            "SELECT kind, name, line, owner_id, reason, detail FROM unresolved WHERE file = ?1 AND kind IN ('calls', 'renders', 'registers_callback') ORDER BY line",
        )?;
        let rows = stmt.query_map(params![target.file], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u32,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut misses = Vec::new();
        for row in rows {
            misses.push(row?);
        }
        for (kind, miss_name, line, owner_id, reason, detail) in misses {
            let in_scope = match &owner_id {
                Some(id) => source_ids.contains(id),
                None => target.kind == Kind::File,
            };
            if !in_scope {
                continue;
            }
            let mut miss = Miss {
                level: 1,
                file: target.file.clone(),
                line,
                edge: kind,
                name: miss_name,
                reason,
                detail,
                who: owner_id.unwrap_or_default(),
            };
            db.describe_miss_owner(&mut miss)?;
            report.unresolved.push(miss);
        }
    }
    finish(root, &store, report)
}

/// Files that import (or re-export from) `file`.
pub fn imports(root: &Path, file: &str) -> Result<RefReport, MapError> {
    let store = MapStore::open_read(root)?;
    let db = Db {
        conn: store.db(),
        cache: HashMap::new(),
    };
    let mut report = new_report(file, "imports");
    let mut paths: Vec<String> = Vec::new();
    {
        let mut stmt = db
            .conn
            .prepare_cached("SELECT path FROM files WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'")?;
        let pattern = format!("%/{}", like_escape(file));
        let rows = stmt.query_map(params![file, pattern], |row| row.get::<_, String>(0))?;
        for row in rows {
            paths.push(row?);
        }
    }
    if paths.iter().any(|p| p == file) {
        paths.retain(|p| p == file);
    }
    report.targets = paths
        .iter()
        .map(|p| format!("{p}:1 file {}", basename(p)))
        .collect();
    for path in &paths {
        let mut stmt = db.conn.prepare_cached(
            "SELECT kind, src_id, src_file, line, name FROM edges WHERE dst_file = ?1 AND kind IN ('imports', 're_exports') AND dst_file IN (SELECT path FROM files) ORDER BY src_file, line",
        )?;
        let rows = stmt.query_map(params![path], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? as u32,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut edges = Vec::new();
        for row in rows {
            edges.push(row?);
        }
        for (edge, src_file, line, spec) in edges {
            report.resolved.push(Hit {
                level: 1,
                who: format!("import \"{spec}\""),
                who_kind: "file".to_string(),
                file: src_file,
                line,
                edge,
                to: Some(basename(path).to_string()),
                to_file: Some(path.clone()),
            });
        }
    }
    let stem = {
        let base = basename(file);
        base.rsplit_once('.').map_or(base, |(s, _)| s).to_string()
    };
    {
        let mut stmt = db.conn.prepare_cached(
            "SELECT file, kind, name, line, reason, detail FROM unresolved WHERE kind IN ('imports', 're_exports') ORDER BY file, line",
        )?;
        let rows = stmt.query_map(params![], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? as u32,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        for row in rows {
            let (miss_file, kind, spec, line, reason, detail) = row?;
            let last = spec.rsplit('/').next().unwrap_or(&spec);
            let last_stem = last.rsplit_once('.').map_or(last, |(s, _)| s);
            if last_stem == stem {
                report.unresolved.push(Miss {
                    level: 1,
                    who: format!("(top level of {})", basename(&miss_file)),
                    file: miss_file,
                    line,
                    edge: kind,
                    name: spec,
                    reason,
                    detail,
                });
            }
        }
    }
    finish(root, &store, report)
}

/// Transitive callers of `name`, up to `depth` levels.
pub fn impact(root: &Path, name: &str, depth: usize) -> Result<RefReport, MapError> {
    let store = MapStore::open_read(root)?;
    let mut db = Db {
        conn: store.db(),
        cache: HashMap::new(),
    };
    let mut report = new_report(name, "impact");
    let targets = db.targets(name)?;
    report.targets = targets.iter().map(target_label).collect();
    let mut visited: HashSet<String> = targets.iter().map(|t| t.id.clone()).collect();
    let mut frontier = targets;
    let mut seen_hits: HashSet<(usize, String, u32, String)> = HashSet::new();
    let mut seen_names: HashSet<String> = HashSet::new();
    for level in 1..=depth.max(1) {
        let mut next: Vec<Row> = Vec::new();
        for current in &frontier {
            for (edge, src_id, src_file, line) in edges_into(db.conn, &current.id, &CALL_KINDS)? {
                let structural = edge == "registers_callback"
                    && current.kind == Kind::Callback
                    && current.parent_id.as_deref().unwrap_or("") == src_id;
                if structural {
                    continue;
                }
                if !seen_hits.insert((level, src_id.clone(), line, current.id.clone())) {
                    continue;
                }
                let Some(src) = db.ent(&src_id)? else {
                    continue;
                };
                report.resolved.push(Hit {
                    level,
                    file: src_file,
                    line,
                    edge,
                    who: db.label(&src)?,
                    who_kind: src.kind.as_str().to_string(),
                    to: Some(current.name.clone()),
                    to_file: Some(current.file.clone()),
                });
                if src.kind == Kind::File {
                    continue;
                }
                let mut expand = vec![src.clone()];
                if src.kind == Kind::Callback {
                    // the container is the same level of the chain: continue from it
                    if let Some(container) = db.named_ancestor(&src)? {
                        expand.push(container);
                    }
                }
                for entity in expand {
                    if visited.insert(entity.id.clone()) {
                        next.push(entity);
                    }
                }
            }
            if current.kind != Kind::File && current.kind != Kind::Callback {
                let symbol = current.name.clone();
                if seen_names.insert(symbol.clone()) {
                    for mut miss in db.unresolved_named(&symbol)? {
                        miss.level = level;
                        db.describe_miss_owner(&mut miss)?;
                        report.unresolved.push(miss);
                    }
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    finish(root, &store, report)
}

fn pct(coverage: Option<f64>) -> String {
    match coverage {
        Some(value) => format!("{:.1}%", value * 100.0),
        None => "n/a".to_string(),
    }
}

pub fn print(out: &mut impl Write, report: &RefReport, json: bool) -> Result<(), MapError> {
    if json {
        let text = serde_json::to_string(&serde_json::json!({
            "query": report.query,
            "mode": report.mode,
            "targets": report.targets,
            "resolved": report.resolved,
            "unresolved": report.unresolved,
            "resolved_count": report.resolved_count,
            "unresolved_count": report.unresolved_count,
            "coverage": report.coverage,
            "stale": report.stale,
        }))
        .map_err(|error| MapError::Json(error.to_string()))?;
        writeln!(out, "{text}")?;
        return Ok(());
    }
    let title = match report.mode {
        "calls" => "callers of",
        "callees" => "callees of",
        "imports" => "importers of",
        _ => "impact of",
    };
    writeln!(out, "{title} {}", report.query)?;
    if report.targets.is_empty() {
        writeln!(out, "  no entity with that name in the map")?;
    }
    for target in &report.targets {
        writeln!(out, "  target: {target}")?;
    }
    if report.resolved.is_empty() {
        writeln!(out, "resolved: none")?;
    } else {
        writeln!(out, "resolved ({}):", report.resolved.len())?;
    }
    for hit in &report.resolved {
        let indent = "  ".repeat(hit.level.max(1));
        let tag = if hit.edge == "calls" {
            String::new()
        } else {
            format!("  [{}]", hit.edge)
        };
        let level = if report.mode == "impact" {
            format!("level {} · ", hit.level)
        } else {
            String::new()
        };
        match (&hit.to, report.mode) {
            (Some(to), "callees") => writeln!(
                out,
                "{indent}{level}{}:{} {} → {to}{tag}",
                hit.file, hit.line, hit.who
            )?,
            (Some(to), "impact") => writeln!(
                out,
                "{indent}{level}{}:{} {} → {to}{tag}",
                hit.file, hit.line, hit.who
            )?,
            _ => writeln!(out, "{indent}{}:{} {}{tag}", hit.file, hit.line, hit.who)?,
        }
    }
    if !report.unresolved.is_empty() {
        writeln!(
            out,
            "unresolved ({}) — may be real calls:",
            report.unresolved.len()
        )?;
    }
    for miss in &report.unresolved {
        let tag = if miss.edge == "calls" {
            String::new()
        } else {
            format!(" [{}]", miss.edge)
        };
        let who = if miss.who.is_empty() {
            String::new()
        } else {
            format!(" in {}", miss.who)
        };
        writeln!(
            out,
            "  ? {}:{} {}{tag}{who} — {}: {}",
            miss.file, miss.line, miss.name, miss.reason, miss.detail
        )?;
    }
    if !report.stale.is_empty() {
        writeln!(out, "stale: {}", report.stale.join(", "))?;
    }
    writeln!(
        out,
        "resolved {} · unresolved {} · coverage {}",
        report.resolved_count,
        report.unresolved_count,
        pct(report.coverage)
    )?;
    Ok(())
}
