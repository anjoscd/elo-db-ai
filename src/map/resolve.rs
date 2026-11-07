//! Resolution of raw calls and imports into typed edges (SPEC §7, part 2).
//!
//! Order for a call: (a) definition in the same file; (b) imported name, following relative
//! imports, re-exports and tsconfig/deno import-map aliases; (c) member of a namespace import.
//! Whatever fails is stored as `unresolved` with a reason.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::rc::Rc;

use rusqlite::{params, Connection};

use super::{Kind, MapError};

const CODE_EXTS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];

const ASSET_EXTS: &[&str] = &[
    "css", "scss", "sass", "less", "json", "svg", "png", "jpg", "jpeg", "gif", "webp", "ico",
    "woff", "woff2", "ttf", "md", "html", "txt", "mp3", "mp4", "wasm", "yaml", "yml", "sql",
];

const GLOBALS: &[&str] = &[
    "console",
    "JSON",
    "Math",
    "Object",
    "Array",
    "Promise",
    "Date",
    "String",
    "Number",
    "Boolean",
    "Symbol",
    "BigInt",
    "Error",
    "TypeError",
    "RangeError",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "RegExp",
    "Response",
    "Request",
    "Headers",
    "URL",
    "URLSearchParams",
    "fetch",
    "setTimeout",
    "clearTimeout",
    "setInterval",
    "clearInterval",
    "parseInt",
    "parseFloat",
    "isNaN",
    "isFinite",
    "encodeURIComponent",
    "decodeURIComponent",
    "encodeURI",
    "decodeURI",
    "atob",
    "btoa",
    "structuredClone",
    "queueMicrotask",
    "Deno",
    "process",
    "Buffer",
    "require",
    "window",
    "document",
    "navigator",
    "localStorage",
    "sessionStorage",
    "crypto",
    "performance",
    "AbortController",
    "FormData",
    "Blob",
    "File",
    "TextEncoder",
    "TextDecoder",
    "Intl",
    "Reflect",
    "Proxy",
    "Uint8Array",
    "ArrayBuffer",
    "EventTarget",
    "Event",
    "CustomEvent",
    "WebSocket",
    "ReadableStream",
    "WritableStream",
    "TransformStream",
    "alert",
    "confirm",
    "prompt",
    "globalThis",
    "self",
    "Function",
    "requestAnimationFrame",
    "cancelAnimationFrame",
    "IntersectionObserver",
    "ResizeObserver",
    "MutationObserver",
    "Image",
];

/// What changed in one sync; decides which files must be resolved again.
pub struct Plan {
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    /// Files that pointed at a changed or removed file (see `dependents_of`), taken before the edit.
    pub dependents: Vec<String>,
    /// Export names that a changed file gained or lost.
    pub export_names: Vec<String>,
    /// A file appeared or vanished: previously missing modules may exist now.
    pub membership_changed: bool,
    /// A tsconfig / import map changed: everything is resolved again.
    pub everything: bool,
}

pub fn is_config_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ((lower.starts_with("tsconfig") || lower.starts_with("jsconfig")) && lower.ends_with(".json"))
        || lower == "deno.json"
        || lower == "deno.jsonc"
        || lower == "package.json"
        || (lower.starts_with("import_map") && lower.ends_with(".json"))
}

/// Files that import or re-export (transitively) any of `files`, per the edges stored now.
pub fn dependents_of(conn: &Connection, files: &[String]) -> Result<Vec<String>, MapError> {
    let mut out: HashSet<String> = HashSet::new();
    let mut frontier: Vec<String> = files.to_vec();
    let mut visited: HashSet<String> = files.iter().cloned().collect();
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT src_file FROM edges WHERE dst_file = ?1 AND kind IN ('imports', 're_exports')",
    )?;
    while let Some(file) = frontier.pop() {
        let rows = stmt.query_map(params![file], |row| row.get::<_, String>(0))?;
        for row in rows {
            let dependent = row?;
            out.insert(dependent.clone());
            if visited.insert(dependent.clone()) {
                frontier.push(dependent);
            }
        }
    }
    let mut out: Vec<String> = out.into_iter().collect();
    out.sort();
    Ok(out)
}

/// Resolve the files in `plan` and the files that depend on them; returns how many were resolved.
/// Runs inside the caller's transaction.
pub fn run(conn: &Connection, root: &Path, plan: &Plan) -> Result<usize, MapError> {
    let targets = select_targets(conn, plan)?;
    if targets.is_empty() {
        return Ok(0);
    }
    let mut resolver = Resolver::new(conn, root)?;
    for file in &targets {
        conn.execute("DELETE FROM edges WHERE src_file = ?1", params![file])?;
        conn.execute("DELETE FROM unresolved WHERE file = ?1", params![file])?;
        resolver.resolve_file(file)?;
    }
    Ok(targets.len())
}

fn select_targets(conn: &Connection, plan: &Plan) -> Result<Vec<String>, MapError> {
    let mut all: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
        let rows = stmt.query_map(params![], |row| row.get::<_, String>(0))?;
        for row in rows {
            all.push(row?);
        }
    }
    if plan.everything {
        return Ok(all);
    }
    let known: HashSet<&String> = all.iter().collect();
    let mut set: HashSet<String> = plan.changed.iter().cloned().collect();
    set.extend(plan.dependents.iter().cloned());
    if plan.membership_changed {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT file FROM unresolved WHERE kind IN ('imports', 're_exports') AND reason != 'external'",
        )?;
        let rows = stmt.query_map(params![], |row| row.get::<_, String>(0))?;
        for row in rows {
            set.insert(row?);
        }
    }
    // a file that gained or lost an export may fix (or break) unresolved uses of that name
    {
        let mut by_binding = conn.prepare_cached(
            "SELECT DISTINCT b.file FROM raw_bindings b JOIN unresolved u ON u.file = b.file \
             AND (u.name = b.local OR u.name LIKE b.local || '.%' ESCAPE '\\') WHERE b.imported = ?1",
        )?;
        let mut by_member = conn.prepare_cached(
            "SELECT DISTINCT file FROM unresolved WHERE name LIKE ?1 ESCAPE '\\'",
        )?;
        for name in &plan.export_names {
            let rows = by_binding.query_map(params![name], |row| row.get::<_, String>(0))?;
            for row in rows {
                set.insert(row?);
            }
            let pattern = format!("%.{}", super::store::like_escape(name));
            let rows = by_member.query_map(params![pattern], |row| row.get::<_, String>(0))?;
            for row in rows {
                set.insert(row?);
            }
        }
    }
    let mut out: Vec<String> = set.into_iter().filter(|f| known.contains(f)).collect();
    out.sort();
    Ok(out)
}

#[derive(Clone)]
struct Ent {
    id: String,
    kind: Kind,
    name: String,
    line: u32,
    container: Option<String>,
    parent_id: Option<String>,
}

struct Bind {
    local: String,
    spec: String,
    imported: String,
    kind: String,
}

struct Exp {
    exported: String,
    local: Option<String>,
    spec: Option<String>,
    kind: String,
    line: u32,
}

struct FileData {
    ents: Vec<Ent>,
    binds: Vec<Bind>,
    exps: Vec<Exp>,
    /// (scope entity id, name) of parameters and local variables.
    locals: HashSet<(String, String)>,
}

#[derive(Clone)]
struct Target {
    id: String,
    file: String,
    kind: Kind,
    name: String,
}

#[derive(Clone, PartialEq)]
enum Module {
    File(String),
    External,
    /// A bare specifier that is no known package, alias or import map entry.
    Alias,
    Missing,
    Asset,
}

enum Res {
    Hit(Target),
    Unres(&'static str, String),
    Skip,
}

#[derive(Default)]
struct Config {
    base_url: Option<String>,
    paths_base: String,
    paths: Vec<(String, Vec<String>)>,
}

struct Resolver<'a> {
    conn: &'a Connection,
    root: &'a Path,
    files: HashSet<String>,
    data: HashMap<String, Rc<FileData>>,
    modules: HashMap<(String, String), Module>,
    configs: HashMap<String, Rc<Vec<Config>>>,
    packages: HashMap<String, Rc<HashSet<String>>>,
    import_maps: HashMap<String, Rc<Vec<String>>>,
}

struct Call {
    name: String,
    line: u32,
    owner_id: Option<String>,
    shape: String,
    class: Option<String>,
}

impl<'a> Resolver<'a> {
    fn new(conn: &'a Connection, root: &'a Path) -> Result<Self, MapError> {
        let mut files = HashSet::new();
        {
            let mut stmt = conn.prepare("SELECT path FROM files")?;
            let rows = stmt.query_map(params![], |row| row.get::<_, String>(0))?;
            for row in rows {
                files.insert(row?);
            }
        }
        Ok(Self {
            conn,
            root,
            files,
            data: HashMap::new(),
            modules: HashMap::new(),
            configs: HashMap::new(),
            packages: HashMap::new(),
            import_maps: HashMap::new(),
        })
    }

    fn data(&mut self, file: &str) -> Result<Rc<FileData>, MapError> {
        if let Some(found) = self.data.get(file) {
            return Ok(found.clone());
        }
        let mut ents = Vec::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT id, kind, name, line, container, parent_id FROM entities WHERE file = ?1",
            )?;
            let rows = stmt.query_map(params![file], |row| {
                let kind: String = row.get(1)?;
                Ok(Ent {
                    id: row.get(0)?,
                    kind: Kind::parse(&kind).unwrap_or(Kind::Function),
                    name: row.get(2)?,
                    line: row.get::<_, i64>(3)? as u32,
                    container: row.get(4)?,
                    parent_id: row.get(5)?,
                })
            })?;
            for row in rows {
                ents.push(row?);
            }
        }
        let mut binds = Vec::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT local, spec, imported, kind FROM raw_bindings WHERE file = ?1",
            )?;
            let rows = stmt.query_map(params![file], |row| {
                Ok(Bind {
                    local: row.get(0)?,
                    spec: row.get(1)?,
                    imported: row.get(2)?,
                    kind: row.get(3)?,
                })
            })?;
            for row in rows {
                binds.push(row?);
            }
        }
        let mut exps = Vec::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT exported, local, spec, kind, line FROM raw_exports WHERE file = ?1",
            )?;
            let rows = stmt.query_map(params![file], |row| {
                Ok(Exp {
                    exported: row.get(0)?,
                    local: row.get(1)?,
                    spec: row.get(2)?,
                    kind: row.get(3)?,
                    line: row.get::<_, i64>(4)? as u32,
                })
            })?;
            for row in rows {
                exps.push(row?);
            }
        }
        let mut locals = HashSet::new();
        {
            let mut stmt = self
                .conn
                .prepare_cached("SELECT scope_id, name FROM raw_locals WHERE file = ?1")?;
            let rows = stmt.query_map(params![file], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                locals.insert(row?);
            }
        }
        let data = Rc::new(FileData {
            ents,
            binds,
            exps,
            locals,
        });
        self.data.insert(file.to_string(), data.clone());
        Ok(data)
    }

    fn file_target(&mut self, file: &str) -> Result<Option<Target>, MapError> {
        let data = self.data(file)?;
        Ok(data
            .ents
            .iter()
            .find(|e| e.kind == Kind::File)
            .map(|e| Target {
                id: e.id.clone(),
                file: file.to_string(),
                kind: Kind::File,
                name: e.name.clone(),
            }))
    }

    fn resolve_file(&mut self, file: &str) -> Result<(), MapError> {
        let Some(file_ent) = self.file_target(file)? else {
            return Ok(());
        };
        let data = self.data(file)?;
        let mut imports: Vec<(String, u32)> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare_cached("SELECT spec, line FROM raw_imports WHERE file = ?1")?;
            let rows = stmt.query_map(params![file], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32))
            })?;
            for row in rows {
                imports.push(row?);
            }
        }
        for (spec, line) in imports {
            self.link_module(file, &file_ent, "imports", &spec, line)?;
        }
        let mut seen: HashSet<(String, u32)> = HashSet::new();
        for export in &data.exps {
            if let Some(spec) = &export.spec {
                if seen.insert((spec.clone(), export.line)) {
                    self.link_module(file, &file_ent, "re_exports", spec, export.line)?;
                }
            }
        }
        let mut calls: Vec<Call> = Vec::new();
        for ent in data.ents.iter().filter(|e| e.kind == Kind::Callback) {
            let src_id = ent.parent_id.clone().unwrap_or_else(|| file_ent.id.clone());
            self.conn
                .prepare_cached(
                    "INSERT INTO edges (kind, src_id, src_file, dst_id, dst_file, name, line) VALUES ('registers_callback', ?1, ?2, ?3, ?2, ?4, ?5)",
                )?
                .execute(params![src_id, file, ent.id, ent.name, ent.line as i64])?;
        }
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT name, line, owner_id, shape, class FROM raw_calls WHERE file = ?1",
            )?;
            let rows = stmt.query_map(params![file], |row| {
                Ok(Call {
                    name: row.get(0)?,
                    line: row.get::<_, i64>(1)? as u32,
                    owner_id: row.get(2)?,
                    shape: row.get(3)?,
                    class: row.get(4)?,
                })
            })?;
            for row in rows {
                calls.push(row?);
            }
        }
        for call in calls {
            let src_id = call.owner_id.clone().unwrap_or_else(|| file_ent.id.clone());
            let kind = match call.shape.as_str() {
                "jsx" => "renders",
                "ref" => "registers_callback",
                _ => "calls",
            };
            match self.resolve_call(file, &data, &call)? {
                Res::Hit(target) => {
                    self.conn
                        .prepare_cached(
                            "INSERT INTO edges (kind, src_id, src_file, dst_id, dst_file, name, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        )?
                        .execute(params![
                            kind,
                            src_id,
                            file,
                            target.id,
                            target.file,
                            call.name,
                            call.line as i64
                        ])?;
                }
                Res::Unres(reason, detail) => {
                    self.conn
                        .prepare_cached(
                            "INSERT INTO unresolved (file, kind, name, line, owner_id, reason, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        )?
                        .execute(params![
                            file,
                            kind,
                            call.name,
                            call.line as i64,
                            call.owner_id,
                            reason,
                            detail
                        ])?;
                }
                Res::Skip => {}
            }
        }
        Ok(())
    }

    fn link_module(
        &mut self,
        file: &str,
        file_ent: &Target,
        kind: &str,
        spec: &str,
        line: u32,
    ) -> Result<(), MapError> {
        match self.module(file, spec)? {
            Module::File(target) => {
                if let Some(dst) = self.file_target(&target)? {
                    self.conn
                        .prepare_cached(
                            "INSERT INTO edges (kind, src_id, src_file, dst_id, dst_file, name, line) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        )?
                        .execute(params![
                            kind,
                            file_ent.id,
                            file,
                            dst.id,
                            dst.file,
                            spec,
                            line as i64
                        ])?;
                }
            }
            Module::Asset => {}
            other => {
                let (reason, detail) = match other {
                    Module::External => ("external", "package, jsr or URL".to_string()),
                    Module::Alias => ("unknown-binding", alias_detail(spec)),
                    _ => ("unknown-binding", "module not found".to_string()),
                };
                self.conn
                    .prepare_cached(
                        "INSERT INTO unresolved (file, kind, name, line, owner_id, reason, detail) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
                    )?
                    .execute(params![file, kind, spec, line as i64, reason, detail])?;
            }
        }
        Ok(())
    }

    fn resolve_call(&mut self, file: &str, data: &FileData, call: &Call) -> Result<Res, MapError> {
        let is_ref = call.shape == "ref";
        match call.shape.as_str() {
            "dynamic" => {
                return Ok(Res::Unres("dynamic", "dynamic import()".to_string()));
            }
            "subscript" => {
                return Ok(Res::Unres(
                    "computed-member",
                    "member computed at runtime".to_string(),
                ));
            }
            "other" => {
                let detail = if call.name.starts_with("(expr).") {
                    "receiver is the result of another expression"
                } else {
                    "called expression is not a name"
                };
                return Ok(Res::Unres("unknown-binding", detail.to_string()));
            }
            _ => {}
        }
        let segments: Vec<&str> = call.name.split('.').collect();
        let root_name = segments[0];
        if segments.len() == 1 {
            match self.scoped_definition(file, data, root_name, call) {
                Scoped::Found(target) => return Ok(Res::Hit(target)),
                Scoped::Shadowed => return Ok(unknown_name(root_name, is_ref)),
                Scoped::None => {}
            }
            if let Some(bind) = data.binds.iter().find(|b| b.local == root_name) {
                if bind.kind == "namespace" {
                    return Ok(if is_ref {
                        Res::Skip
                    } else {
                        Res::Unres(
                            "unknown-binding",
                            format!("'{root_name}' is a namespace, not a function"),
                        )
                    });
                }
                return self.follow_binding(file, bind, is_ref);
            }
            if let Some(target) = top_level_definition(file, data, root_name) {
                return Ok(Res::Hit(target));
            }
            return Ok(unknown_name(root_name, is_ref));
        }
        if root_name == "this" {
            if segments.len() == 2 {
                let method = data.ents.iter().find(|e| {
                    e.kind == Kind::Method
                        && e.name == segments[1]
                        && (call.class.is_none() || e.container == call.class)
                });
                if let Some(method) = method {
                    return Ok(Res::Hit(Target {
                        id: method.id.clone(),
                        file: file.to_string(),
                        kind: method.kind,
                        name: method.name.clone(),
                    }));
                }
            }
            return Ok(Res::Unres(
                "unknown-binding",
                "method of this not found in the class".to_string(),
            ));
        }
        if is_shadowed(data, call, root_name) {
            return Ok(Res::Unres(
                "unknown-binding",
                format!("'{root_name}' is a parameter or local variable"),
            ));
        }
        if let Some(bind) = data.binds.iter().find(|b| b.local == root_name) {
            return self.follow_member(file, bind, &segments);
        }
        if segments.len() == 2 {
            let class = data
                .ents
                .iter()
                .find(|e| e.kind == Kind::Class && e.name == root_name);
            if class.is_some() {
                if let Some(method) = data.ents.iter().find(|e| {
                    e.kind == Kind::Method
                        && e.name == segments[1]
                        && e.container.as_deref() == Some(root_name)
                }) {
                    return Ok(Res::Hit(Target {
                        id: method.id.clone(),
                        file: file.to_string(),
                        kind: method.kind,
                        name: method.name.clone(),
                    }));
                }
            }
        }
        if GLOBALS.contains(&root_name) {
            return Ok(Res::Unres("external", format!("global '{root_name}'")));
        }
        Ok(Res::Unres(
            "unknown-binding",
            format!("'{root_name}' is neither an import nor a definition in the file"),
        ))
    }

    /// A function, component or class named `name` visible from the call's scope.
    fn scoped_definition(&self, file: &str, data: &FileData, name: &str, call: &Call) -> Scoped {
        for scope in scope_chain(data, call.owner_id.as_deref()) {
            if data.locals.contains(&(scope.clone(), name.to_string())) {
                return Scoped::Shadowed;
            }
            let found = data
                .ents
                .iter()
                .filter(|e| {
                    e.name == name
                        && e.parent_id.as_deref() == Some(scope.as_str())
                        && matches!(e.kind, Kind::Function | Kind::Component | Kind::Class)
                })
                .min_by(|a, b| (a.line, &a.id).cmp(&(b.line, &b.id)));
            if let Some(ent) = found {
                return Scoped::Found(target_of(file, ent));
            }
        }
        Scoped::None
    }

    fn follow_binding(&mut self, file: &str, bind: &Bind, is_ref: bool) -> Result<Res, MapError> {
        let module = self.module(file, &bind.spec)?;
        Ok(match module {
            Module::External => {
                if is_ref {
                    Res::Skip
                } else {
                    Res::Unres("external", format!("imported from '{}'", bind.spec))
                }
            }
            Module::Asset => Res::Skip,
            Module::Alias => {
                if is_ref {
                    Res::Skip
                } else {
                    Res::Unres("unknown-binding", alias_detail(&bind.spec))
                }
            }
            Module::Missing => {
                if is_ref {
                    Res::Skip
                } else {
                    Res::Unres(
                        "unknown-binding",
                        format!("module '{}' not found", bind.spec),
                    )
                }
            }
            Module::File(target) => {
                let mut seen = HashSet::new();
                match self.find_export(&target, &bind.imported, &mut seen)? {
                    Some(found) => Res::Hit(found),
                    None if is_ref => Res::Skip,
                    None => self.missing_export(&target, &bind.imported)?,
                }
            }
        })
    }

    /// Why `name` cannot be taken from `file`: declared there but not exported, or absent.
    fn missing_export(&mut self, file: &str, name: &str) -> Result<Res, MapError> {
        let data = self.data(file)?;
        let declared = data.ents.iter().any(|e| {
            e.name == name
                && e.container.is_none()
                && matches!(e.kind, Kind::Function | Kind::Component | Kind::Class)
        });
        Ok(if declared {
            Res::Unres(
                "not-exported",
                format!("'{name}' exists in {file} but is not exported"),
            )
        } else {
            Res::Unres(
                "unknown-binding",
                format!("'{name}' is not exported by {file}"),
            )
        })
    }

    fn follow_member(
        &mut self,
        file: &str,
        bind: &Bind,
        segments: &[&str],
    ) -> Result<Res, MapError> {
        let module = self.module(file, &bind.spec)?;
        let target_file = match module {
            Module::External => {
                return Ok(Res::Unres(
                    "external",
                    format!("imported from '{}'", bind.spec),
                ))
            }
            Module::Alias => {
                return Ok(Res::Unres("unknown-binding", alias_detail(&bind.spec)));
            }
            Module::Missing | Module::Asset => {
                return Ok(Res::Unres(
                    "unknown-binding",
                    format!("module '{}' not found", bind.spec),
                ))
            }
            Module::File(target) => target,
        };
        let mut seen = HashSet::new();
        if bind.kind == "namespace" {
            if segments.len() == 2 {
                return match self.find_export(&target_file, segments[1], &mut seen)? {
                    Some(found) => Ok(Res::Hit(found)),
                    None => self.missing_export(&target_file, segments[1]),
                };
            }
            return Ok(Res::Unres(
                "unknown-binding",
                "nested namespace member".to_string(),
            ));
        }
        // `export * as ns from "./x"` seen through `import { ns } from ...; ns.fn()`
        if segments.len() == 2 {
            let data = self.data(&target_file)?;
            let namespace = data
                .exps
                .iter()
                .find(|e| e.kind == "starns" && e.exported == bind.imported)
                .and_then(|e| e.spec.clone());
            if let Some(spec) = namespace {
                if let Module::File(next) = self.module(&target_file, &spec)? {
                    let mut inner = HashSet::new();
                    return match self.find_export(&next, segments[1], &mut inner)? {
                        Some(found) => Ok(Res::Hit(found)),
                        None => self.missing_export(&next, segments[1]),
                    };
                }
            }
        }
        // `import { Cls } from ...; Cls.method()`
        if segments.len() == 2 {
            if let Some(class) = self.find_export(&target_file, &bind.imported, &mut seen)? {
                if class.kind == Kind::Class {
                    let data = self.data(&class.file)?;
                    if let Some(method) = data.ents.iter().find(|e| {
                        e.kind == Kind::Method
                            && e.name == segments[1]
                            && e.container.as_deref() == Some(class.name.as_str())
                    }) {
                        return Ok(Res::Hit(Target {
                            id: method.id.clone(),
                            file: class.file.clone(),
                            kind: method.kind,
                            name: method.name.clone(),
                        }));
                    }
                }
            }
        }
        Ok(Res::Unres(
            "unknown-binding",
            format!("member of imported object '{}'", segments[0]),
        ))
    }

    /// The entity exported as `name` by `file`, following re-exports.
    fn find_export(
        &mut self,
        file: &str,
        name: &str,
        seen: &mut HashSet<(String, String)>,
    ) -> Result<Option<Target>, MapError> {
        if !seen.insert((file.to_string(), name.to_string())) {
            return Ok(None);
        }
        let data = self.data(file)?;
        for export in data.exps.iter().filter(|e| e.exported == name) {
            let found = match (export.kind.as_str(), &export.local) {
                ("reexport", Some(local)) => match &export.spec {
                    Some(spec) => match self.module(file, spec)? {
                        Module::File(next) => self.find_export(&next, local, seen)?,
                        _ => None,
                    },
                    None => None,
                },
                ("named" | "default", Some(local)) => {
                    self.local_or_imported(file, &data, local, seen)?
                }
                _ => None,
            };
            if found.is_some() {
                return Ok(found);
            }
        }
        if name == "default" {
            return Ok(None);
        }
        for export in data.exps.iter().filter(|e| e.kind == "star") {
            let Some(spec) = &export.spec else { continue };
            if let Module::File(next) = self.module(file, spec)? {
                if let Some(found) = self.find_export(&next, name, seen)? {
                    return Ok(Some(found));
                }
            }
        }
        Ok(None)
    }

    fn local_or_imported(
        &mut self,
        file: &str,
        data: &FileData,
        local: &str,
        seen: &mut HashSet<(String, String)>,
    ) -> Result<Option<Target>, MapError> {
        if let Some(ent) = data.ents.iter().find(|e| {
            e.name == local
                && e.container.is_none()
                && matches!(
                    e.kind,
                    Kind::Function | Kind::Component | Kind::Class | Kind::Callback
                )
        }) {
            return Ok(Some(Target {
                id: ent.id.clone(),
                file: file.to_string(),
                kind: ent.kind,
                name: ent.name.clone(),
            }));
        }
        if let Some(bind) = data.binds.iter().find(|b| b.local == local) {
            if bind.kind != "namespace" {
                if let Module::File(next) = self.module(file, &bind.spec)? {
                    return self.find_export(&next, &bind.imported, seen);
                }
            }
        }
        Ok(None)
    }

    fn module(&mut self, from: &str, spec: &str) -> Result<Module, MapError> {
        let key = (from.to_string(), spec.to_string());
        if let Some(found) = self.modules.get(&key) {
            return Ok(found.clone());
        }
        let module = self.compute_module(from, spec);
        self.modules.insert(key, module.clone());
        Ok(module)
    }

    fn compute_module(&mut self, from: &str, spec: &str) -> Module {
        let dir = from.rsplit_once('/').map_or("", |(d, _)| d);
        let relative =
            spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../");
        if relative {
            return match join_norm(dir, spec).and_then(|p| self.try_candidates(&p)) {
                Some(found) => Module::File(found),
                None if is_asset(spec) => Module::Asset,
                None => Module::Missing,
            };
        }
        if spec.starts_with('/') {
            return Module::Missing;
        }
        if ["jsr:", "npm:", "node:", "data:", "http://", "https://"]
            .iter()
            .any(|p| spec.starts_with(p))
        {
            return Module::External;
        }
        let configs = self.configs_for(dir);
        let mut matched = false;
        for config in configs.iter() {
            let base = config
                .base_url
                .clone()
                .unwrap_or_else(|| config.paths_base.clone());
            let mut hits: Vec<(usize, &Vec<String>, String)> = Vec::new();
            for (pattern, targets) in &config.paths {
                if let Some(capture) = match_pattern(pattern, spec) {
                    let prefix = pattern.split('*').next().map_or(0, str::len);
                    hits.push((prefix, targets, capture));
                }
            }
            hits.sort_by_key(|a| std::cmp::Reverse(a.0));
            for (_, targets, capture) in hits {
                matched = true;
                for target in targets {
                    let replaced = target.replacen('*', &capture, 1);
                    if let Some(found) =
                        join_norm(&base, &replaced).and_then(|p| self.try_candidates(&p))
                    {
                        return Module::File(found);
                    }
                }
            }
            if let Some(base_url) = &config.base_url {
                if let Some(found) = join_norm(base_url, spec).and_then(|p| self.try_candidates(&p))
                {
                    return Module::File(found);
                }
            }
        }
        if matched {
            Module::Missing
        } else if self.is_known_package(dir, spec) {
            Module::External
        } else {
            Module::Alias
        }
    }

    /// A package from a package.json up the tree, a node_modules folder, an import map or a Node builtin.
    fn is_known_package(&mut self, dir: &str, spec: &str) -> bool {
        let package = package_name(spec);
        if NODE_BUILTINS.contains(&package) {
            return true;
        }
        let mut current = dir.to_string();
        loop {
            if self.package_names(&current).contains(package)
                || self
                    .root
                    .join(&current)
                    .join("node_modules")
                    .join(package)
                    .is_dir()
                || self.import_map_keys(&current).iter().any(|key| {
                    key == spec || (key.ends_with('/') && spec.starts_with(key.as_str()))
                })
            {
                return true;
            }
            if current.is_empty() {
                return false;
            }
            current = current
                .rsplit_once('/')
                .map_or(String::new(), |(parent, _)| parent.to_string());
        }
    }

    fn package_names(&mut self, dir: &str) -> Rc<HashSet<String>> {
        if let Some(found) = self.packages.get(dir) {
            return found.clone();
        }
        let rel = if dir.is_empty() {
            "package.json".to_string()
        } else {
            format!("{dir}/package.json")
        };
        let mut names = HashSet::new();
        if let Some(json) = read_jsonc(&self.root.join(rel)) {
            for section in [
                "dependencies",
                "devDependencies",
                "peerDependencies",
                "optionalDependencies",
            ] {
                if let Some(map) = json.get(section).and_then(|v| v.as_object()) {
                    names.extend(map.keys().cloned());
                }
            }
        }
        let names = Rc::new(names);
        self.packages.insert(dir.to_string(), names.clone());
        names
    }

    /// Keys of the `imports` map in `dir` (deno.json, deno.jsonc or import_map.json).
    fn import_map_keys(&mut self, dir: &str) -> Rc<Vec<String>> {
        if let Some(found) = self.import_maps.get(dir) {
            return found.clone();
        }
        let mut keys = Vec::new();
        for name in ["deno.json", "deno.jsonc", "import_map.json"] {
            let rel = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}/{name}")
            };
            if let Some(json) = read_jsonc(&self.root.join(rel)) {
                if let Some(map) = json.get("imports").and_then(|v| v.as_object()) {
                    keys.extend(map.keys().cloned());
                }
            }
        }
        let keys = Rc::new(keys);
        self.import_maps.insert(dir.to_string(), keys.clone());
        keys
    }

    fn try_candidates(&self, path: &str) -> Option<String> {
        if self.files.contains(path) {
            return Some(path.to_string());
        }
        if let Some((stem, ext)) = path.rsplit_once('.') {
            if matches!(ext, "js" | "jsx" | "mjs" | "cjs") {
                for alt in ["ts", "tsx"] {
                    let candidate = format!("{stem}.{alt}");
                    if self.files.contains(&candidate) {
                        return Some(candidate);
                    }
                }
            }
        }
        for ext in CODE_EXTS {
            let candidate = format!("{path}.{ext}");
            if self.files.contains(&candidate) {
                return Some(candidate);
            }
        }
        for ext in CODE_EXTS {
            let candidate = if path.is_empty() {
                format!("index.{ext}")
            } else {
                format!("{path}/index.{ext}")
            };
            if self.files.contains(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Alias configs of the nearest folder (walking up) that has a tsconfig or import map.
    fn configs_for(&mut self, dir: &str) -> Rc<Vec<Config>> {
        if let Some(found) = self.configs.get(dir) {
            return found.clone();
        }
        let mut current = dir.to_string();
        let found = loop {
            let loaded = load_folder_configs(self.root, &current);
            if !loaded.is_empty() {
                break loaded;
            }
            if current.is_empty() {
                break Vec::new();
            }
            current = current
                .rsplit_once('/')
                .map_or(String::new(), |(parent, _)| parent.to_string());
        };
        let found = Rc::new(found);
        self.configs.insert(dir.to_string(), found.clone());
        found
    }
}

fn unknown_name(name: &str, is_ref: bool) -> Res {
    if is_ref {
        Res::Skip
    } else if GLOBALS.contains(&name) {
        Res::Unres("external", format!("global '{name}'"))
    } else {
        Res::Unres(
            "unknown-binding",
            format!("'{name}' is a local variable, a parameter or an unknown global"),
        )
    }
}

enum Scoped {
    Found(Target),
    /// A parameter or local variable hides any outer definition of the name.
    Shadowed,
    None,
}

fn target_of(file: &str, ent: &Ent) -> Target {
    Target {
        id: ent.id.clone(),
        file: file.to_string(),
        kind: ent.kind,
        name: ent.name.clone(),
    }
}

/// Function, component or class declared at the top level; the first one by line.
fn top_level_definition(file: &str, data: &FileData, name: &str) -> Option<Target> {
    data.ents
        .iter()
        .filter(|e| {
            e.name == name
                && e.parent_id.is_none()
                && matches!(e.kind, Kind::Function | Kind::Component | Kind::Class)
        })
        .min_by(|a, b| (a.line, &a.id).cmp(&(b.line, &b.id)))
        .map(|e| target_of(file, e))
}

fn is_shadowed(data: &FileData, call: &Call, name: &str) -> bool {
    scope_chain(data, call.owner_id.as_deref())
        .into_iter()
        .any(|scope| data.locals.contains(&(scope, name.to_string())))
}

fn alias_detail(spec: &str) -> String {
    format!("alias-not-found: '{spec}' is not a known alias, package or import map entry")
}

/// `@scope/name/sub` -> `@scope/name`; `name/sub` -> `name`.
fn package_name(spec: &str) -> &str {
    let mut parts = spec.splitn(3, '/');
    let first = parts.next().unwrap_or(spec);
    if first.starts_with('@') {
        match parts.next() {
            Some(second) => &spec[..first.len() + 1 + second.len()],
            None => first,
        }
    } else {
        first
    }
}

const NODE_BUILTINS: &[&str] = &[
    "assert",
    "buffer",
    "child_process",
    "cluster",
    "crypto",
    "dns",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "querystring",
    "readline",
    "stream",
    "string_decoder",
    "timers",
    "tls",
    "url",
    "util",
    "vm",
    "worker_threads",
    "zlib",
];

/// Entity ids from the one that owns the call up to the outermost, nearest first.
fn scope_chain(data: &FileData, owner_id: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = owner_id.map(str::to_string);
    while let Some(id) = current {
        let Some(ent) = data.ents.iter().find(|e| e.id == id) else {
            break;
        };
        out.push(ent.id.clone());
        current = ent.parent_id.clone();
        if out.len() > 64 {
            break;
        }
    }
    out
}

fn is_asset(spec: &str) -> bool {
    let last = spec.rsplit('/').next().unwrap_or(spec);
    last.rsplit_once('.')
        .is_some_and(|(_, ext)| ASSET_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Join a relative path onto a folder (both relative to the root); `None` when it escapes the root.
fn join_norm(dir: &str, rel: &str) -> Option<String> {
    if rel.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// Capture of the `*` when `spec` matches a tsconfig `paths` pattern.
fn match_pattern(pattern: &str, spec: &str) -> Option<String> {
    match pattern.split_once('*') {
        None => (pattern == spec).then(String::new),
        Some((prefix, suffix)) => {
            if spec.len() >= prefix.len() + suffix.len()
                && spec.starts_with(prefix)
                && spec.ends_with(suffix)
            {
                Some(spec[prefix.len()..spec.len() - suffix.len()].to_string())
            } else {
                None
            }
        }
    }
}

fn read_jsonc(path: &Path) -> Option<serde_json::Value> {
    let raw = fs::read_to_string(path).ok()?;
    let cleaned = strip_trailing_commas(&strip_comments(raw.trim_start_matches('\u{feff}')));
    serde_json::from_str(&cleaned).ok()
}

fn strip_comments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

fn strip_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    for (i, &c) in chars.iter().enumerate() {
        if in_string {
            out.push(c);
            if c == '"' && chars[..i].iter().rev().take_while(|&&p| p == '\\').count() % 2 == 0 {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|n| !n.is_whitespace());
            if matches!(next, Some('}') | Some(']')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn load_folder_configs(root: &Path, dir: &str) -> Vec<Config> {
    let mut out = Vec::new();
    for name in ["tsconfig.json", "jsconfig.json"] {
        let rel = if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        };
        if root.join(&rel).is_file() {
            load_ts_config(root, &rel, 0, &mut out);
        }
    }
    for name in ["deno.json", "deno.jsonc", "import_map.json"] {
        let rel = if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        };
        if let Some(config) = load_import_map(root, &rel) {
            out.push(config);
        }
    }
    out
}

fn load_ts_config(root: &Path, rel: &str, depth: usize, out: &mut Vec<Config>) {
    let Some(config) = merged_ts_config(root, rel, 0) else {
        return;
    };
    if config.base_url.is_some() || !config.paths.is_empty() {
        out.push(config);
    }
    if depth > 0 {
        return;
    }
    let Some(json) = read_jsonc(&root.join(rel)) else {
        return;
    };
    let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
    let references = json.get("references").and_then(|v| v.as_array());
    for reference in references.into_iter().flatten() {
        let Some(path) = reference.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(joined) = join_norm(dir, path) else {
            continue;
        };
        let file = if joined.ends_with(".json") {
            joined
        } else if joined.is_empty() {
            "tsconfig.json".to_string()
        } else {
            format!("{joined}/tsconfig.json")
        };
        if root.join(&file).is_file() {
            load_ts_config(root, &file, depth + 1, out);
        }
    }
}

fn merged_ts_config(root: &Path, rel: &str, depth: usize) -> Option<Config> {
    let json = read_jsonc(&root.join(rel))?;
    let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
    let options = json.get("compilerOptions");
    let mut config = Config {
        paths_base: dir.to_string(),
        ..Config::default()
    };
    if let Some(base) = options
        .and_then(|o| o.get("baseUrl"))
        .and_then(|v| v.as_str())
    {
        config.base_url = join_norm(dir, base);
    }
    if let Some(paths) = options
        .and_then(|o| o.get("paths"))
        .and_then(|v| v.as_object())
    {
        for (pattern, targets) in paths {
            let list: Vec<String> = targets
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect();
            config.paths.push((pattern.clone(), list));
        }
    }
    if depth < 5 && (config.base_url.is_none() || config.paths.is_empty()) {
        if let Some(parent) = json.get("extends").and_then(|v| v.as_str()) {
            if parent.starts_with('.') {
                if let Some(joined) = join_norm(dir, parent) {
                    let candidates = [joined.clone(), format!("{joined}.json")];
                    for candidate in candidates {
                        if root.join(&candidate).is_file() {
                            if let Some(base) = merged_ts_config(root, &candidate, depth + 1) {
                                if config.base_url.is_none() {
                                    config.base_url = base.base_url;
                                }
                                if config.paths.is_empty() {
                                    config.paths = base.paths;
                                    config.paths_base = base.paths_base;
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
    Some(config)
}

/// Local entries of a Deno `imports` map, as `paths`-like patterns.
fn load_import_map(root: &Path, rel: &str) -> Option<Config> {
    if !root.join(rel).is_file() {
        return None;
    }
    let json = read_jsonc(&root.join(rel))?;
    let imports = json.get("imports")?.as_object()?;
    let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
    let mut config = Config {
        paths_base: dir.to_string(),
        ..Config::default()
    };
    for (key, value) in imports {
        let Some(value) = value.as_str() else {
            continue;
        };
        if !(value.starts_with("./") || value.starts_with("../")) {
            continue;
        }
        if let Some(prefix) = key.strip_suffix('/') {
            let target = value.trim_end_matches('/');
            config
                .paths
                .push((format!("{prefix}/*"), vec![format!("{target}/*")]));
        } else {
            config.paths.push((key.clone(), vec![value.to_string()]));
        }
    }
    (!config.paths.is_empty()).then_some(config)
}
