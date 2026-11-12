use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{json, Value};

const EXPECTED_INSTRUCTIONS: &str = "Use elo_recall to search memories before working; elo_context to read a scope or topic; elo_add to record notes and decisions as an agent; elo_explain to understand an item; elo_map to query the previously indexed code map.";

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_elo"))
        .arg("init")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    dir
}

struct Session {
    child: Child,
    input: Option<ChildStdin>,
    output: Receiver<String>,
}

impl Session {
    fn new(cwd: &Path, root: Option<&Path>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_elo"));
        command.arg("mcp").current_dir(cwd);
        if let Some(root) = root {
            command.arg("--root").arg(root);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, output) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output,
        }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.input.as_mut().unwrap(), "{line}").unwrap();
    }

    fn receive(&self) -> Value {
        serde_json::from_str(&self.output.recv_timeout(Duration::from_secs(10)).unwrap()).unwrap()
    }

    fn request(&mut self, id: Value, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string());
        let result = self.receive();
        assert_eq!(result["jsonrpc"], "2.0");
        assert_eq!(result["id"], id);
        result
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            json!(1),
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        )
    }

    fn close(&mut self) {
        self.input.take();
        assert!(matches!(
            self.output.recv_timeout(Duration::from_secs(10)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(self.child.wait().unwrap().success());
        let mut stderr = String::new();
        std::io::Read::read_to_string(self.child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
        assert!(stderr.is_empty(), "{stderr}");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn content(response: &Value) -> &str {
    assert_eq!(response["result"]["isError"], false, "{response}");
    assert_eq!(response["result"]["content"][0]["type"], "text");
    response["result"]["content"][0]["text"].as_str().unwrap()
}

#[test]
fn handshake_tools_and_persistent_agent_memory() {
    let dir = workspace();
    let elsewhere = tempfile::tempdir().unwrap();
    let mut session = Session::new(elsewhere.path(), Some(dir.path()));
    for version in [
        "2025-11-25",
        "2025-06-18",
        "2025-03-26",
        "2024-11-05",
        "unknown",
    ] {
        let response = session.request(json!("init"), "initialize", json!({"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"test","version":"1"}}));
        assert_eq!(
            response["result"]["protocolVersion"],
            if version == "unknown" {
                "2025-11-25"
            } else {
                version
            }
        );
        assert_eq!(response["result"]["capabilities"], json!({"tools":{}}));
        assert_eq!(
            response["result"]["serverInfo"],
            json!({"name":"elo","version":env!("CARGO_PKG_VERSION")})
        );
        assert_eq!(response["result"]["instructions"], EXPECTED_INSTRUCTIONS);
    }
    let list = session.request(json!(2), "tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 8);
    for name in [
        "elo_recall",
        "elo_context",
        "elo_add",
        "elo_explain",
        "elo_map",
        "elo_feedback",
        "elo_maintain_catalog",
        "elo_maintain_propose",
    ] {
        let tool = tools.iter().find(|t| t["name"] == name).unwrap();
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(tool["inputSchema"]["required"].is_array());
    }
    let add = tools.iter().find(|t| t["name"] == "elo_add").unwrap();
    let kinds = add["inputSchema"]["properties"]["kind"]["enum"]
        .as_array()
        .unwrap();
    assert!(!kinds.contains(&json!("law")));
    assert!(kinds.contains(&json!("note")));
    let memory = "Nebulosa: preservar decisões de arquitetura";
    let added = session.tool(
        "elo_add",
        json!({"text":memory,"keywords":["nebulosa"],"paths":["src/**"]}),
    );
    let id = content(&added).to_owned();
    assert!(!id.is_empty());
    let recalled = session.tool("elo_recall", json!({"query":"nebulosa","limit":5}));
    assert!(content(&recalled).contains(memory));
    let explained = session.tool("elo_explain", json!({"id":id}));
    assert!(content(&explained).contains(&id));
    assert!(content(&explained).contains("src/**"));
    let cli = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(["explain", &id])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(cli.status.success());
    assert_eq!(content(&explained).as_bytes(), cli.stdout);
    let context = session.tool("elo_context", json!({"target":"global"}));
    let cli = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(["context", "global"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(cli.status.success());
    assert_eq!(content(&context).as_bytes(), cli.stdout);
    let db = rusqlite::Connection::open(dir.path().join(".elo/elo.db")).unwrap();
    let (origin, triggers): (String, String) = db
        .query_row(
            "SELECT origin, triggers FROM items WHERE id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(origin, "agent");
    let triggers: Value = serde_json::from_str(&triggers).unwrap();
    assert_eq!(triggers["paths"], json!(["src/**"]));
    assert_eq!(triggers["keywords"], json!(["nebulosa"]));
    // Outra conexão consegue escrever enquanto a sessão MCP está ociosa.
    db.execute_batch("BEGIN IMMEDIATE; ROLLBACK;").unwrap();
    for (args, expected) in [
        (json!({"text":" "}), "title and body must not be empty"),
        (
            json!({"text":"nota","scope":"invalid"}),
            "invalid scope \"invalid\" (use global, project:NAME or link:NAME)",
        ),
    ] {
        let response = session.tool("elo_add", args);
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert!(response.get("error").is_none());
        assert_eq!(response["result"]["content"][0]["text"], expected);
    }
    assert_eq!(
        session.tool("elo_explain", json!({"id":"missing"}))["result"]["isError"],
        true
    );
    let unknown = session.tool("unknown", json!({}));
    assert_eq!(
        unknown["error"],
        json!({"code":-32602,"message":"Unknown tool"})
    );
    assert!(unknown.get("result").is_none());
    let default_note = session.tool("elo_add", json!({"text":"Memória padrão sem gatilhos"}));
    assert!(content(&default_note).starts_with("note-"));
    session.close();
    let mut next = Session::new(dir.path(), None);
    assert!(content(&next.tool("elo_recall", json!({"query":"nebulosa"}))).contains(memory));
    next.close();
}

#[test]
fn protocol_errors_notifications_and_eof() {
    let dir = workspace();
    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let mut session = Session::new(&nested, None);
    for notification in [
        json!({}),
        json!({"method":"ping"}),
        json!({"jsonrpc":"1.0","method":3}),
        json!({"jsonrpc":"2.0","method":"tools/call","params":[]}),
    ] {
        session.send(&notification.to_string());
    }
    session.send("");
    session.send("  \t");
    session.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    session.send(r#"{"jsonrpc":"2.0","method":"anything"}"#);
    session.send(r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"elo_add","arguments":{"text":"notificationphantom"}}}"#);
    assert_eq!(
        session.request(json!(7), "ping", json!({}))["result"],
        json!({})
    );
    assert!(
        !content(&session.tool("elo_recall", json!({"query":"notificationphantom"})))
            .contains("notificationphantom")
    );
    assert_eq!(
        session.request(json!(8), "unknown", json!({}))["error"]["code"],
        -32601
    );
    for (line, code) in [
        ("broken", -32700),
        ("[]", -32600),
        ("null", -32600),
        (r#"{"jsonrpc":"1.0","id":null,"method":"ping"}"#, -32600),
        (r#"{"jsonrpc":"2.0","id":{},"method":"ping"}"#, -32600),
    ] {
        session.send(line);
        let response = session.receive();
        assert_eq!(response["error"]["code"], code);
        assert_eq!(response["id"], Value::Null);
    }
    for (method, params) in [
        ("ping", json!([])),
        ("initialize", json!({})),
        ("tools/call", json!({"name":3})),
        ("tools/call", json!({"name":"elo_recall","arguments":null})),
    ] {
        assert_eq!(
            session.request(json!(9), method, params)["error"]["code"],
            -32602
        );
    }
    for (name, args) in [
        ("elo_recall", json!({})),
        ("elo_recall", json!({"query":false})),
        ("elo_recall", json!({"query":"x","limit":-1})),
        ("elo_recall", json!({"query":"x","limit":1.5})),
        ("elo_add", json!({"text":"x","paths":[3]})),
        ("elo_add", json!({"text":"x","keywords":"x"})),
        ("elo_add", json!({"text":"x","kind":"unknown"})),
        ("elo_add", json!({"text":"x","kind":"law"})),
        ("elo_add", json!({"text":"x","as_user":true})),
        ("elo_map", json!({"mode":"unknown","value":"x"})),
    ] {
        assert_eq!(session.tool(name, args)["error"]["code"], -32602);
    }
    // A última mensagem também é processada sem newline antes do EOF.
    write!(
        session.input.as_mut().unwrap(),
        "{}",
        json!({"jsonrpc":"2.0","id":null,"method":"ping"})
    )
    .unwrap();
    session.input.take();
    assert_eq!(
        session.receive(),
        json!({"jsonrpc":"2.0","id":null,"result":{}})
    );
    session.close();
    let mut empty = Session::new(dir.path(), None);
    empty.close();
}

#[test]
fn map_tools_match_cli() {
    let dir = workspace();
    std::fs::write(
        dir.path().join("main.ts"),
        "export function greet() { return 1; }\nexport function run() { return greet(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("caller.ts"),
        "import { greet } from './main';\nexport function invoke() { return greet(); }\n",
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(["map", "sync"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let mut session = Session::new(dir.path(), None);
    for (mode, value) in [
        ("file", "main.ts"),
        ("contains", "greet"),
        ("calls", "greet"),
        ("imports", "main.ts"),
        ("impact", "greet"),
    ] {
        let response = session.tool("elo_map", json!({"mode":mode,"value":value}));
        let cli = Command::new(env!("CARGO_BIN_EXE_elo"))
            .args(["map", "query", &format!("--{mode}"), value])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(cli.status.success(), "{cli:?}");
        assert_eq!(content(&response).as_bytes(), cli.stdout);
        assert!(content(&response).contains(if mode == "imports" {
            "caller.ts"
        } else {
            "greet"
        }));
    }
    session.close();
}

#[test]
fn missing_root_fails_only_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    for explicit in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_elo"));
        command
            .arg("mcp")
            .current_dir(dir.path())
            .stdin(Stdio::null());
        if explicit {
            command.arg("--root").arg(dir.path());
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        let stderr = String::from_utf8(result.stderr).unwrap();
        if explicit {
            assert_eq!(
                stderr,
                "error: no ELO store here (.elo/elo.db); run `elo init`\n"
            );
        } else {
            assert!(stderr.contains("No ELO root found"));
            assert!(stderr
                .contains("run `elo init` in your workspace root or use `elo mcp --root <root>`"));
        }
    }
}

#[test]
fn agent_feedback_tool_accepts_only_recent_deliveries() {
    let dir = workspace();
    let db = rusqlite::Connection::open(dir.path().join(".elo/elo.db")).unwrap();
    let insert_item = |id: &str| {
        db.execute(
            "INSERT INTO items (id, kind, scope, title, body, origin, triggers, created_at, updated_at) \
             VALUES (?1, 'rule', 'global', 't', 'b', 'agent', '{}', 0, 0)",
            [id],
        )
        .unwrap();
    };
    insert_item("a1");
    insert_item("b1");
    let inject = |session: &str, seq: i64, item: &str| {
        db.execute(
            "INSERT INTO injections (session_id, event_seq, item_id, mode, kind, scope, at) \
             VALUES (?1, ?2, ?3, 'full', 'rule', 'global', 0)",
            rusqlite::params![session, seq, item],
        )
        .unwrap();
    };
    inject("s1", 4, "a1");
    inject("s1", 50, "b1"); // "a1" leaves the 40-event window
    inject("s2", 3, "b1");

    let mut session = Session::new(dir.path(), None);
    let resp = session.tool(
        "elo_feedback",
        json!({"session_id":"s1","used":["b1"],"ignored":[]}),
    );
    assert_eq!(content(&resp), "accepted: b1; rejected: ");
    // Delivered in a different session does not count.
    let resp = session.tool(
        "elo_feedback",
        json!({"session_id":"s1","used":[],"ignored":["a1"]}),
    );
    assert_eq!(content(&resp), "accepted: ; rejected: a1");
    // The same id is delivered in s2: the report must name that session.
    let resp = session.tool("elo_feedback", json!({"session_id":"s2","used":["b1"]}));
    assert_eq!(content(&resp), "accepted: b1; rejected: ");
    // Omitting session_id is rejected by schema validation; an empty session_id rejects every id.
    let resp = session.tool("elo_feedback", json!({"used":["b1"],"ignored":["a1"]}));
    assert_eq!(resp["error"]["code"], -32602);
    let resp = session.tool(
        "elo_feedback",
        json!({"session_id":"","used":["b1"],"ignored":["a1"]}),
    );
    assert_eq!(content(&resp), "accepted: ; rejected: b1, a1");
    // More than twenty ids per list comes back as a tool error, not a crash.
    let ids: Vec<Value> = (0..21).map(|i| json!(format!("x{i}"))).collect();
    let resp = session.tool("elo_feedback", json!({"session_id":"s1","used":ids}));
    assert_eq!(resp["result"]["isError"], true, "{resp}");
    assert!(resp.get("error").is_none());
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("too many ids"),
        "{resp}"
    );
    session.close();
}

#[test]
fn end_to_end_hook_feedback_line_accepted_by_mcp() {
    let dir = workspace();
    // The line only asks about items already delivered in the session, so the store needs one.
    let added = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args([
            "add",
            "law",
            "--as-user",
            "--id",
            "law-base",
            "--scope",
            "global",
            "--title",
            "Base",
            "--body",
            "Always preserve user data.",
            "--triggers",
            "help",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(added.status.success(), "{added:?}");
    let run_hook = || -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_elo"))
            .args(["hook", "user-prompt-submit", "--live"])
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = json!({"session_id":"e2e-sess","cwd":dir.path(),"prompt":"help"});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    for _ in 1..=4 {
        run_hook();
    }
    let out5 = run_hook();
    let line = out5
        .lines()
        .find(|l| l.starts_with("Call elo_feedback("))
        .expect("feedback line on event 5");
    let sid = line
        .strip_prefix("Call elo_feedback(session_id:\"")
        .and_then(|s| s.split_once('"'))
        .map(|(s, _)| s)
        .expect("session_id in feedback line");
    let ids_part = line
        .rsplit_once("for these delivered ids: [")
        .and_then(|(_, rest)| rest.strip_suffix("]."))
        .expect("delivered ids list");
    let delivered: Vec<String> = ids_part
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(sid, "e2e-sess");
    assert_eq!(delivered, vec!["law-base".to_string()]);

    let mut mcp = Session::new(dir.path(), None);
    let resp = mcp.tool(
        "elo_feedback",
        json!({"session_id": sid, "used": delivered, "ignored": []}),
    );
    assert_eq!(content(&resp), "accepted: law-base; rejected: ");
    mcp.close();
}

fn protocol_meta(version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": {"name":"test-modern", "version":"1"},
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

#[test]
fn discovery_and_modern_tools_without_handshake() {
    let dir = workspace();
    let mut session = Session::new(dir.path(), None);
    // Ferramentas funcionam inclusive antes de discover.
    let meta = protocol_meta("2026-07-28");
    let list = session.request(json!(1), "tools/list", json!({"_meta":meta}));
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 8);
    assert_eq!(list["result"]["resultType"], "complete");
    let added = session.request(
        json!(2),
        "tools/call",
        json!({
            "_meta":meta,"name":"elo_add","arguments":{"text":"modernmemory"}
        }),
    );
    assert!(content(&added).starts_with("note-"));
    assert_eq!(added["result"]["resultType"], "complete");
    let recalled = session.request(
        json!(3),
        "tools/call",
        json!({
            "_meta":meta,"name":"elo_recall","arguments":{"query":"modernmemory"}
        }),
    );
    assert!(content(&recalled).contains("modernmemory"));
    for params in [json!({}), json!({"_meta":meta})] {
        let discovered = session.request(json!("discover"), "server/discover", params);
        assert_eq!(
            discovered["result"],
            json!({
                "supportedVersions":["2026-07-28","2025-11-25","2025-06-18","2025-03-26","2024-11-05"],
                "capabilities":{"tools":{}},
                "instructions":EXPECTED_INSTRUCTIONS,
                "resultType":"complete",
                "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"elo","version":env!("CARGO_PKG_VERSION")}}
            })
        );
        assert!(discovered["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("elo_recall"));
    }
    // Não há versão retida na conexão: sem metadados permanece tolerante.
    assert_eq!(
        session.request(json!(4), "ping", json!({}))["result"],
        json!({})
    );
    for version in ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"] {
        let response = session.request(
            json!(5),
            "tools/list",
            json!({"_meta":protocol_meta(version)}),
        );
        assert!(response["result"]["tools"].is_array());
        assert!(response["result"].get("resultType").is_none());
    }
    // Só a versão basta, conforme a tolerância solicitada.
    assert_eq!(
        session.request(
            json!(6),
            "ping",
            json!({"_meta":{
                "io.modelcontextprotocol/protocolVersion":"2026-07-28"
            }})
        )["result"]["resultType"],
        "complete"
    );
    session.close();
}

#[test]
fn unsupported_version_cannot_execute_tools_and_does_not_poison_session() {
    let dir = workspace();
    let mut session = Session::new(dir.path(), None);
    for method in [
        "server/discover",
        "tools/list",
        "tools/call",
        "ping",
        "unknown",
    ] {
        let response = session.request(
            json!("bad-version"),
            method,
            json!({
                "_meta":protocol_meta("1999-01-01"),
                "name":"elo_add", "arguments":{"text":"versionphantom"}
            }),
        );
        assert_eq!(response["error"]["code"], -32022);
        assert_eq!(
            response["error"]["data"],
            json!({"supported":[
                "2026-07-28","2025-11-25","2025-06-18","2025-03-26","2024-11-05"
            ]})
        );
        assert!(response.get("result").is_none());
    }
    assert!(
        !content(&session.tool("elo_recall", json!({"query":"versionphantom"})))
            .contains("versionphantom")
    );
    for meta in [
        json!(null),
        json!([]),
        json!({"io.modelcontextprotocol/protocolVersion":3}),
    ] {
        assert_eq!(
            session.request(json!(7), "tools/list", json!({"_meta":meta}))["error"]["code"],
            -32602
        );
    }
    assert_eq!(
        session.request(json!(8), "ping", json!({"_meta":{"custom":"value"}}))["result"],
        json!({})
    );
    let legacy = session.request(
        json!(9),
        "initialize",
        json!({"protocolVersion":"2026-07-28"}),
    );
    assert_eq!(legacy["result"]["protocolVersion"], "2025-11-25");
    assert!(legacy["result"].get("resultType").is_none());
    assert_eq!(
        session.request(
            json!(10),
            "ping",
            json!({"_meta":protocol_meta("2026-07-28")})
        )["result"]["resultType"],
        "complete"
    );
    session.close();
}

#[test]
fn map_paths_stay_inside_workspace() {
    let dir = workspace();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.ts"), "export function safe() {}\n").unwrap();
    let sync = Command::new(env!("CARGO_BIN_EXE_elo"))
        .args(["map", "sync"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(sync.status.success());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("absent"), dir.path().join("dangling"))
            .unwrap();
        std::os::unix::fs::symlink(dir.path().join("main.ts"), dir.path().join("inside.ts"))
            .unwrap();
    }
    let mut session = Session::new(dir.path(), None);
    for mode in ["file", "imports"] {
        let mut invalid = vec!["/etc/passwd", "../x", "nested/../../x", "nested/../main.ts"];
        #[cfg(unix)]
        invalid.extend(["escape/absent.ts", "dangling", "dangling/child.ts"]);
        for value in invalid {
            let response = session.tool("elo_map", json!({"mode":mode,"value":value}));
            assert_eq!(response["result"]["isError"], true, "{response}");
            assert!(response.get("error").is_none());
            assert!(response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Invalid map path:"));
        }
        for value in ["main.ts", "./main.ts", "deleted/absent.ts"] {
            content(&session.tool("elo_map", json!({"mode":mode,"value":value})));
        }
        #[cfg(unix)]
        content(&session.tool("elo_map", json!({"mode":mode,"value":"inside.ts"})));
    }
    // Symbol searches are not filesystem paths.
    for mode in ["contains", "calls", "impact"] {
        content(&session.tool("elo_map", json!({"mode":mode,"value":"../x"})));
    }
    session.close();
}
