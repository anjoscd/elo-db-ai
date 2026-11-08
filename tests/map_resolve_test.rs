use std::fs;
use std::path::Path;

use elo_ctx::map::refs::RefReport;
use elo_ctx::map::{query_callees, query_calls, query_impact, query_imports, sync};

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
fn call_inside_deno_serve_resolves_to_imported_shared_function() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "functions/_shared/x.ts",
        "export function loadUser(id: string) { return id; }\n",
    );
    write(
        dir.path(),
        "functions/hello/index.ts",
        "import { loadUser } from \"../_shared/x.ts\";\n\nDeno.serve(async (req) => {\n  const u = loadUser(\"1\");\n  return new Response(u);\n});\n",
    );
    sync(dir.path(), None).unwrap();
    let report = query_calls(dir.path(), "loadUser").unwrap();
    assert_eq!(report.resolved.len(), 1);
    let hit = &report.resolved[0];
    assert_eq!(hit.file, "functions/hello/index.ts");
    assert_eq!(hit.line, 4);
    assert_eq!(hit.who, "Deno.serve#handler@L3 (in index.ts)");
    let text = render(&report);
    assert!(text.contains("functions/hello/index.ts:4 Deno.serve#handler@L3 (in index.ts)"));
    assert!(text.contains("resolved "));
    assert!(text.contains("coverage"));
}

#[test]
fn component_calls_hook_through_tsconfig_alias() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "tsconfig.json",
        "{\n  // comentário\n  \"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@/*\": [\"./src/*\"], }, },\n}\n",
    );
    write(
        dir.path(),
        "src/hooks/useX.ts",
        "export function useX() { return 1; }\n",
    );
    write(
        dir.path(),
        "src/components/Painel.tsx",
        "import { useX } from \"@/hooks/useX\";\n\nexport function Painel() {\n  useEffect(() => {\n    useX();\n  }, []);\n  return <div onClick={() => useX()}>ok</div>;\n}\n",
    );
    sync(dir.path(), None).unwrap();
    let report = query_calls(dir.path(), "useX").unwrap();
    let whos: Vec<&str> = report.resolved.iter().map(|h| h.who.as_str()).collect();
    assert!(whos.contains(&"useEffect#1@L4 (in Painel)"), "{whos:?}");
    assert!(whos.contains(&"onClick#handler@L7 (in Painel)"), "{whos:?}");
    assert_eq!(report.resolved.len(), 2);
}

#[test]
fn reexport_through_index_is_followed_and_renders_edge_is_typed() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "lib/impl.tsx",
        "export function work() { return 1; }\nexport function Card() { return <b/>; }\n",
    );
    write(
        dir.path(),
        "lib/index.ts",
        "export { work } from \"./impl\";\nexport * from \"./impl\";\n",
    );
    write(
        dir.path(),
        "app/main.tsx",
        "import { work, Card } from \"../lib\";\n\nexport function Main() {\n  work();\n  return <Card />;\n}\n",
    );
    sync(dir.path(), None).unwrap();
    let calls = query_calls(dir.path(), "work").unwrap();
    assert_eq!(calls.resolved.len(), 1);
    assert_eq!(calls.resolved[0].file, "app/main.tsx");
    let renders = query_calls(dir.path(), "Card").unwrap();
    assert_eq!(renders.resolved.len(), 1);
    assert_eq!(renders.resolved[0].edge, "renders");
    let importers = query_imports(dir.path(), "lib/impl.tsx").unwrap();
    assert!(importers
        .resolved
        .iter()
        .any(|h| h.file == "lib/index.ts" && h.edge == "re_exports"));
}

#[test]
fn namespace_import_member_resolves() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "util.ts",
        "export function slug(s: string) { return s; }\n",
    );
    write(
        dir.path(),
        "a.ts",
        "import * as ns from \"./util\";\nexport function run() { return ns.slug(\"x\"); }\n",
    );
    sync(dir.path(), None).unwrap();
    let report = query_calls(dir.path(), "slug").unwrap();
    assert_eq!(report.resolved.len(), 1);
    assert_eq!(report.resolved[0].who, "run");
}

#[test]
fn computed_member_call_is_listed_as_unresolved_never_a_bare_zero() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "handlers.ts",
        "export function go() { return 1; }\nexport function dispatch(obj: any, k: string) {\n  return obj[k]();\n}\nexport function other(obj: any) { return obj.go(); }\n",
    );
    sync(dir.path(), None).unwrap();
    let report = query_calls(dir.path(), "go").unwrap();
    assert!(report.resolved.is_empty());
    assert!(report
        .unresolved
        .iter()
        .any(|m| m.name == "obj.go" && m.reason == "unknown-binding"));
    let dynamic = query_callees(dir.path(), "dispatch").unwrap();
    assert!(dynamic
        .unresolved
        .iter()
        .any(|m| m.name == "obj[k]" && m.reason == "computed-member"));
    let text = render(&dynamic);
    assert!(text.contains("computed-member"));
    assert!(text.contains("unresolved 1"));
    assert!(text.contains("coverage 0.0%"));
    let text = render(&report);
    assert!(text.contains("unknown-binding"));
}

#[test]
fn external_packages_and_dynamic_imports_have_reasons() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "a.ts",
        "import { createClient } from \"npm:@supabase/supabase-js\";\nimport { serve } from \"https://deno.land/std/http/server.ts\";\nexport async function run() {\n  createClient();\n  serve(() => 1);\n  await import(\"./b\");\n}\n",
    );
    sync(dir.path(), None).unwrap();
    let report = query_callees(dir.path(), "run").unwrap();
    let reasons: Vec<(&str, &str)> = report
        .unresolved
        .iter()
        .map(|m| (m.name.as_str(), m.reason.as_str()))
        .collect();
    assert!(
        reasons.contains(&("createClient", "external")),
        "{reasons:?}"
    );
    assert!(reasons.contains(&("serve", "external")), "{reasons:?}");
    assert!(reasons.contains(&("import", "dynamic")), "{reasons:?}");
}

#[test]
fn impact_walks_two_levels_through_callbacks() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "core.ts",
        "export function leaf() { return 1; }\n",
    );
    write(
        dir.path(),
        "mid.ts",
        "import { leaf } from \"./core\";\nexport function middle() { return leaf(); }\n",
    );
    write(
        dir.path(),
        "top.ts",
        "import { middle } from \"./mid\";\nDeno.serve(async () => {\n  middle();\n});\nexport function direct() { return middle(); }\n",
    );
    sync(dir.path(), None).unwrap();
    let one = query_impact(dir.path(), "leaf", 1).unwrap();
    assert_eq!(one.resolved.len(), 1);
    let two = query_impact(dir.path(), "leaf", 2).unwrap();
    let levels: Vec<(usize, &str)> = two
        .resolved
        .iter()
        .map(|h| (h.level, h.who.as_str()))
        .collect();
    assert!(levels.contains(&(1, "middle")), "{levels:?}");
    assert!(
        levels.contains(&(2, "Deno.serve#handler@L2 (in top.ts)")),
        "{levels:?}"
    );
    assert!(levels.contains(&(2, "direct")), "{levels:?}");
    let text = render(&two);
    assert!(text.contains("level 2"));
}

#[test]
fn incremental_sync_resolves_dependents_of_changed_files() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "a.ts",
        "import { f } from \"./b\";\nexport function g() { return f(); }\n",
    );
    write(
        dir.path(),
        "b.ts",
        "export function other() { return 1; }\n",
    );
    sync(dir.path(), None).unwrap();
    let before = query_calls(dir.path(), "f").unwrap();
    assert!(before.resolved.is_empty());
    assert!(before
        .unresolved
        .iter()
        .any(|m| m.reason == "unknown-binding"));
    write(dir.path(), "b.ts", "export function f() { return 2; }\n");
    let report = sync(dir.path(), None).unwrap();
    assert_eq!(report.indexed, 1);
    assert_eq!(report.resolved, 2);
    let after = query_calls(dir.path(), "f").unwrap();
    assert_eq!(after.resolved.len(), 1);
    assert_eq!(after.resolved[0].file, "a.ts");
}

#[test]
fn removing_a_file_turns_its_callers_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "a.ts",
        "import { f } from \"./b\";\nexport function g() { return f(); }\n",
    );
    write(dir.path(), "b.ts", "export function f() { return 2; }\n");
    sync(dir.path(), None).unwrap();
    assert_eq!(query_calls(dir.path(), "f").unwrap().resolved.len(), 1);
    fs::remove_file(dir.path().join("b.ts")).unwrap();
    sync(dir.path(), None).unwrap();
    let after = query_calls(dir.path(), "f").unwrap();
    assert!(after.resolved.is_empty());
    assert_eq!(after.unresolved.len(), 1);
}

/// Supabase-like corpus: 120 Edge Functions + 17 `_shared` files (137 files, ~18k lines).
fn build_corpus(dir: &Path) -> usize {
    let filler = |count: usize| -> String {
        let mut s = String::new();
        for k in 0..count {
            s.push_str(&format!(
                "function local{k}(a: number) {{\n  const x = a + {k};\n"
            ));
            if k > 0 {
                s.push_str(&format!(
                    "  if (x > 10) {{\n    return local{}(x);\n  }}\n",
                    k - 1
                ));
            } else {
                s.push_str("  if (x > 10) {\n    return x;\n  }\n");
            }
            s.push_str("  return x;\n}\n\n");
        }
        s
    };
    write(
        dir,
        "functions/_shared/http.ts",
        &format!(
            "export const corsHeaders = {{}};\nexport function jsonResponse(body: unknown) {{\n  return new Response(JSON.stringify(body));\n}}\n\n{}",
            filler(18)
        ),
    );
    write(
        dir,
        "functions/_shared/auth.ts",
        &format!(
            "export function validateToken(t: string) {{ return t.length > 3; }}\n\nexport async function requireAuth(req: Request) {{\n  const t = req.headers.get(\"authorization\") ?? \"\";\n  if (!validateToken(t)) throw new Error(\"no\");\n  return t;\n}}\n\nexport function getUser(t: string) {{ return {{ id: t }}; }}\n\n{}",
            filler(16)
        ),
    );
    write(
        dir,
        "functions/_shared/db.ts",
        &format!(
            "export function query(table: string) {{ return [table]; }}\n\n{}",
            filler(18)
        ),
    );
    for u in 0..14 {
        write(dir, &format!("functions/_shared/u{u}.ts"), &filler(19));
    }
    let mut total = 0;
    for i in 0..120 {
        let mut s = String::from(
            "import { jsonResponse } from \"../_shared/http.ts\";\nimport { requireAuth, getUser } from \"../_shared/auth.ts\";\nimport * as db from \"../_shared/db.ts\";\nimport { createClient } from \"npm:@supabase/supabase-js@2\";\n\n",
        );
        if i % 7 == 0 {
            s.push_str("// nota: requireAuth(req) vem do _shared\n");
        }
        s.push_str(&filler(12));
        s.push_str("Deno.serve(async (req) => {\n  const user = await requireAuth(req);\n");
        if i % 2 == 0 {
            s.push_str("  const profile = getUser(user);\n");
        }
        s.push_str("  const rows = db.query(\"t\");\n  const client = createClient(\"u\", \"k\");\n  [1, 2].forEach((n) => {\n    jsonResponse(n);\n  });\n  return jsonResponse({ rows, client, v: local11(1) });\n});\n");
        write(dir, &format!("functions/f{i}/index.ts"), &s);
        total += 1;
    }
    total + 17
}

fn grep_lines(dir: &Path, needle: &str) -> usize {
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != ".elo") {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "ts") {
                let text = fs::read_to_string(&path).unwrap();
                count += text.lines().filter(|l| l.contains(needle)).count();
            }
        }
    }
    count
}

#[test]
fn corpus_callers_match_grep_and_timings() {
    use std::time::Instant;
    let dir = tempfile::tempdir().unwrap();
    let files = build_corpus(dir.path());
    assert_eq!(files, 137);
    let started = Instant::now();
    let report = sync(dir.path(), None).unwrap();
    let sync_ms = started.elapsed().as_millis();
    assert_eq!(report.indexed, 137);
    let again = Instant::now();
    sync(dir.path(), None).unwrap();
    let noop_ms = again.elapsed().as_millis();
    println!("sync 137 arquivos: {sync_ms} ms · sync sem mudanças: {noop_ms} ms");
    // (função, chamadores esperados pelo gerador)
    let expected = [
        ("requireAuth", 120),
        ("getUser", 60),
        ("jsonResponse", 240),
        ("query", 120),
        ("validateToken", 1),
    ];
    for (name, callers) in expected {
        let started = Instant::now();
        let found = query_calls(dir.path(), name).unwrap();
        let query_ms = started.elapsed().as_millis();
        let grep = grep_lines(dir.path(), &format!("{name}("));
        println!(
            "| {name} | {} | {grep} | {query_ms} ms |",
            found.resolved.len()
        );
        assert_eq!(found.resolved.len(), callers, "{name}");
        assert!(
            grep > found.resolved.len(),
            "{name}: grep conta definição e comentário"
        );
    }
}
