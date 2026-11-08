use std::fs;
use std::path::Path;
use std::time::Instant;

use elo_ctx::map::refs::RefReport;
use elo_ctx::map::{
    query_callees, query_calls, query_contains, query_file, query_imports, sync, Kind,
};

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn render(report: &RefReport) -> String {
    let mut out = Vec::new();
    elo_ctx::map::refs::print(&mut out, report, false).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn parameter_shadows_import() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "u.ts",
        "export function helper() { return 1; }\n",
    );
    write(
        dir.path(),
        "a.ts",
        "import { helper } from \"./u\";\nexport function run(helper: () => number) { return helper(); }\nexport function ok() { return helper(); }\n",
    );
    sync(dir.path(), None).unwrap();
    let report = query_calls(dir.path(), "helper").unwrap();
    let whos: Vec<&str> = report.resolved.iter().map(|h| h.who.as_str()).collect();
    assert_eq!(whos, vec!["ok"], "{whos:?}");
}

#[test]
fn local_const_shadows_outer_function() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "a.ts",
        "function f() { return 1; }\nexport function run() {\n  const f = make();\n  return f();\n}\nexport function other() { return f(); }\n",
    );
    sync(dir.path(), None).unwrap();
    let report = query_calls(dir.path(), "f").unwrap();
    let whos: Vec<&str> = report.resolved.iter().map(|h| h.who.as_str()).collect();
    assert_eq!(whos, vec!["other"], "{whos:?}");
}

#[test]
fn target_must_stay_inside_root() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(dir.path(), "a.ts", "export function a() { return 1; }\n");
    sync(dir.path(), None).unwrap();
    let err = sync(dir.path(), Some(Path::new("../x"))).unwrap_err();
    assert!(!err.to_string().is_empty());
    assert!(sync(dir.path(), Some(outside.path())).is_err());
    // o índice continua intacto
    assert!(!query_file(dir.path(), "a.ts").unwrap().hits.is_empty());
}

#[test]
fn unreadable_or_missing_base_never_wipes_the_index() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "sub/a.ts",
        "export function a() { return 1; }\n",
    );
    sync(dir.path(), None).unwrap();
    assert!(sync(dir.path(), Some(Path::new("nao-existe"))).is_err());
    assert!(!query_file(dir.path(), "a.ts").unwrap().hits.is_empty());
}

#[cfg(unix)]
#[test]
fn symlink_loop_terminates() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.ts", "export function a() { return 1; }\n");
    std::os::unix::fs::symlink(".", dir.path().join("loop")).unwrap();
    let report = sync(dir.path(), None).unwrap();
    assert_eq!(report.indexed, 1);
}

#[test]
fn big_binary_and_invalid_files_are_skipped_and_dropped() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "ok.ts", "export function ok() { return 1; }\n");
    write(
        dir.path(),
        "gone.ts",
        "export function gone() { return 1; }\n",
    );
    sync(dir.path(), None).unwrap();
    let has = |n: &str| {
        query_contains(dir.path(), n)
            .unwrap()
            .hits
            .iter()
            .any(|e| e.name == n)
    };
    assert!(has("gone"));
    let big = format!(
        "export function big() {{}}\n// {}\n",
        "x".repeat(2 * 1024 * 1024)
    );
    write(dir.path(), "big.ts", &big);
    fs::write(dir.path().join("nul.ts"), b"export function nul() {}\0\n").unwrap();
    fs::write(
        dir.path().join("bad.ts"),
        b"export function bad() {}\xff\xfe\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("gone.ts"),
        b"export function gone() {}\xff\xfe\n",
    )
    .unwrap();
    let report = sync(dir.path(), None).unwrap();
    assert_eq!(report.skipped, 4);
    assert_eq!(report.removed, 1);
    assert!(!has("gone"));
    assert!(!has("big"));
}

fn many_files(dir: &Path, count: usize) {
    for i in 0..count {
        write(
            dir,
            &format!("src/d{}/f{i}.ts", i % 20),
            &format!("export function fn{i}() {{ return {i}; }}\n"),
        );
    }
}

/// Rodar com: cargo test --test map_fix_test -- --ignored (limite folgado para CI).
#[test]
#[ignore = "temporização; rodar manualmente"]
fn query_and_noop_sync_are_fast_with_2000_files() {
    let dir = tempfile::tempdir().unwrap();
    many_files(dir.path(), 2000);
    sync(dir.path(), None).unwrap();
    let started = Instant::now();
    let report = query_file(dir.path(), "f10.ts").unwrap();
    let query_ms = started.elapsed().as_millis();
    assert!(!report.hits.is_empty());
    assert!(query_ms < 50, "consulta levou {query_ms} ms");
    // sem mudanças (arquivos já fora da janela de corrida)
    std::thread::sleep(std::time::Duration::from_millis(2200));
    sync(dir.path(), None).unwrap();
    let started = Instant::now();
    let report = sync(dir.path(), None).unwrap();
    let noop_ms = started.elapsed().as_millis();
    assert_eq!(report.unchanged, 2000);
    assert!(noop_ms < 1500, "sync noop levou {noop_ms} ms");
}

#[test]
fn query_with_many_files_is_quick_and_noop_sync_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    many_files(dir.path(), 2000);
    sync(dir.path(), None).unwrap();
    let started = Instant::now();
    let report = query_file(dir.path(), "f10.ts").unwrap();
    let query_ms = started.elapsed().as_millis();
    assert!(!report.hits.is_empty());
    assert!(query_ms < 500, "consulta levou {query_ms} ms");
    let report = sync(dir.path(), None).unwrap();
    assert_eq!(report.indexed, 0);
    assert_eq!(report.unchanged, 2000);
}

#[test]
fn coverage_counts_the_query_scope() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "a.ts",
        "export function go() { return 1; }\nexport function x(o: any) { return o.go(); }\nexport function y() { return go(); }\n",
    );
    sync(dir.path(), None).unwrap();
    let report = query_calls(dir.path(), "go").unwrap();
    assert!(report.unresolved_count > 0);
    assert_eq!(report.resolved_count, report.resolved.len());
    let text = render(&report);
    assert!(text.contains(&format!("resolved {}", report.resolved_count)));
    assert!(text.contains(&format!("unresolved {}", report.unresolved_count)));
}

#[test]
fn callbacks_count_as_calls() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "a.ts",
        "function handle() { return 1; }\nexport function run() { return [1].map(handle); }\nexport function setup() { return Deno.serve(handle); }\n",
    );
    sync(dir.path(), None).unwrap();
    let report = query_calls(dir.path(), "handle").unwrap();
    assert!(report
        .resolved
        .iter()
        .any(|h| h.edge == "registers_callback"));
}

#[test]
fn removed_callee_leaves_no_dead_destination() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "b.ts", "export function f() { return 2; }\n");
    write(
        dir.path(),
        "a.ts",
        "import { f } from \"./b\";\nexport function g() { return f(); }\n",
    );
    sync(dir.path(), None).unwrap();
    assert_eq!(query_callees(dir.path(), "g").unwrap().resolved.len(), 1);
    fs::remove_file(dir.path().join("b.ts")).unwrap();
    sync(dir.path(), None).unwrap();
    let report = query_callees(dir.path(), "g").unwrap();
    assert!(report.resolved.is_empty());
    assert!(query_calls(dir.path(), "f").unwrap().resolved.is_empty());
}

#[test]
fn losing_an_export_re_resolves_importers() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "b.ts", "export function f() { return 2; }\n");
    write(
        dir.path(),
        "a.ts",
        "import { f } from \"./b\";\nexport function g() { return f(); }\n",
    );
    sync(dir.path(), None).unwrap();
    assert_eq!(query_calls(dir.path(), "f").unwrap().resolved.len(), 1);
    write(dir.path(), "b.ts", "function f() { return 3; }\n");
    sync(dir.path(), None).unwrap();
    let report = query_calls(dir.path(), "f").unwrap();
    assert!(report.resolved.is_empty());
    assert!(report.unresolved.iter().any(|m| m.reason == "not-exported"));
}

#[test]
fn unknown_alias_is_unresolved_and_package_json_is_external() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "package.json",
        "{\"dependencies\":{\"react\":\"18\"}}",
    );
    write(
        dir.path(),
        "a.ts",
        "import { useState } from \"react\";\nimport { thing } from \"@foo/bar\";\nexport function run() {\n  useState();\n  thing();\n}\n",
    );
    sync(dir.path(), None).unwrap();
    let report = query_callees(dir.path(), "run").unwrap();
    let find = |n: &str| report.unresolved.iter().find(|m| m.name == n).unwrap();
    assert_eq!(find("useState").reason, "external");
    assert_eq!(find("thing").reason, "unknown-binding");
    assert!(find("thing").detail.contains("alias-not-found"));
}

#[test]
fn callback_name_has_line_and_is_searchable_by_short_name() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "p.tsx",
        "export function P() {\n  useEffect(() => { a(); }, []);\n  useEffect(() => { b(); }, []);\n  return null;\n}\n",
    );
    sync(dir.path(), None).unwrap();
    let hits = query_contains(dir.path(), "useEffect#1").unwrap();
    assert!(hits
        .hits
        .iter()
        .any(|e| e.name == "useEffect#1@L2" && e.kind == Kind::Callback));
    assert!(hits.hits.iter().any(|e| e.name == "useEffect#1@L3"));
}

#[test]
fn import_alias_namespace_reexport_cycle_and_anonymous_default() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "x.ts", "export function x() { return 1; }\n");
    write(
        dir.path(),
        "a.ts",
        "import { x as y } from \"./x\";\nimport * as ns from \"./ns\";\nimport def from \"./def\";\nexport function run() { y(); ns.inner.x(); return def(); }\n",
    );
    write(dir.path(), "ns.ts", "export * as inner from \"./x\";\n");
    write(
        dir.path(),
        "def.ts",
        "export default function () { return 1; }\n",
    );
    write(dir.path(), "c1.ts", "export * from \"./c2\";\n");
    write(dir.path(), "c2.ts", "export * from \"./c1\";\n");
    write(
        dir.path(),
        "c3.ts",
        "import { nothing } from \"./c1\";\nexport function z() { return nothing(); }\n",
    );
    sync(dir.path(), None).unwrap();
    let calls = query_calls(dir.path(), "x").unwrap();
    assert!(calls.resolved.iter().any(|h| h.who == "run"), "{calls:?}");
    let callees = query_callees(dir.path(), "z").unwrap();
    assert!(callees.resolved.is_empty());
    let file = query_file(dir.path(), "def.ts").unwrap();
    assert!(!file.hits.is_empty());
    let imports = query_imports(dir.path(), "x.ts").unwrap();
    assert!(!imports.resolved.is_empty());
}

#[test]
fn default_export_memo_links_to_inner_value() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Foo.tsx",
        "function Foo() { return 1; }\nexport default memo(Foo);\n",
    );
    write(
        dir.path(),
        "App.tsx",
        "import Foo from \"./Foo\";\nexport function App() { return Foo(); }\n",
    );
    sync(dir.path(), None).unwrap();
    let report = query_calls(dir.path(), "Foo").unwrap();
    assert!(report.resolved.iter().any(|h| h.who == "App"), "{report:?}");
}
