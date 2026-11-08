use std::fs;
use std::path::Path;

use elo_ctx::map::{query_contains, query_file, raw_calls, sync, Kind};

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

#[test]
fn deno_serve_handler_entity_and_raw_call() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "server.ts",
        "Deno.serve(async (req) => {\n  foo();\n  return new Response(\"ok\");\n});\n",
    );
    let report = sync(dir.path(), None).unwrap();
    assert_eq!(report.indexed, 1);
    assert_eq!(report.unchanged, 0);
    let hits = query_contains(dir.path(), "Deno.serve#handler").unwrap();
    assert!(hits
        .hits
        .iter()
        .any(|e| e.name == "Deno.serve#handler@L1" && e.kind == Kind::Callback && e.line == 1));
    let calls = raw_calls(dir.path(), "Deno.serve#handler").unwrap();
    assert!(calls.iter().any(|(name, _)| name == "foo"));
}

#[test]
fn react_component_and_effect_callback() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/Botao.tsx",
        "export function Botao() {\n  useEffect(() => {\n    carregar();\n  });\n  return <button>ok</button>;\n}\n",
    );
    sync(dir.path(), None).unwrap();
    let entities = query_file(dir.path(), "src/Botao.tsx").unwrap();
    assert!(entities
        .hits
        .iter()
        .any(|e| e.name == "Botao" && e.kind == Kind::Component));
    assert!(entities
        .hits
        .iter()
        .any(|e| e.name == "useEffect#1@L2" && e.kind == Kind::Callback));
    let inner = raw_calls(dir.path(), "useEffect#1").unwrap();
    assert!(inner.iter().any(|(name, _)| name == "carregar"));
    let outer = raw_calls(dir.path(), "Botao").unwrap();
    assert!(outer.iter().any(|(name, _)| name == "useEffect"));
}

#[test]
fn sync_is_incremental() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.ts", "export function a() { return 1; }\n");
    write(dir.path(), "b.ts", "export function b() { return 2; }\n");
    let first = sync(dir.path(), None).unwrap();
    assert_eq!(first.indexed, 2);
    let second = sync(dir.path(), None).unwrap();
    assert_eq!(second.indexed, 0);
    assert_eq!(second.unchanged, 2);
    write(dir.path(), "b.ts", "export function b() { return 3; }\n");
    let third = sync(dir.path(), None).unwrap();
    assert_eq!(third.indexed, 1);
    assert_eq!(third.unchanged, 1);
    fs::remove_file(dir.path().join("a.ts")).unwrap();
    let fourth = sync(dir.path(), None).unwrap();
    assert_eq!(fourth.removed, 1);
    assert_eq!(fourth.unchanged, 1);
    assert!(query_file(dir.path(), "a.ts").unwrap().hits.is_empty());
}
