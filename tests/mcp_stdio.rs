//! Live MCP session against the real binary: spawn `codeindex serve`,
//! speak newline-delimited JSON-RPC over its stdin/stdout, read the
//! answers. This is the M4 acceptance check — protocol, tools and the
//! resolved call graph exercised exactly as an MCP client would.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn stdio_session_speaks_mcp() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("lib.rs"),
        "fn helper() {}\nfn caller() {\n    helper();\n}\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_codeindex"))
        .args(["serve", dir.path().to_str().unwrap(), "--no-refresh"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codeindex serve");

    let mut stdin = child.stdin.take().unwrap();
    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_callers","arguments":{"name":"helper"}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"call_tree","arguments":{"name":"helper","depth":3}}}"#,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_references","arguments":{"name":"helper"}}}"#,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"stats"}}"#,
    ];
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    // EOF ends the loop; every answer is already in the pipe.
    drop(stdin);

    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "server exited with {:?}, stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let mut by_id: HashMap<u64, serde_json::Value> = HashMap::new();
    for line in BufReader::new(&out.stdout[..]).lines() {
        let line = line.unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad line {line:?}: {e}"));
        if let Some(id) = value.get("id").and_then(|i| i.as_u64()) {
            by_id.insert(id, value);
        }
    }

    // initialize
    let init = &by_id[&1];
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "codeindex");

    // tools/list: seven tools, no notification answer (no id 0).
    assert_eq!(by_id[&2]["result"]["tools"].as_array().unwrap().len(), 12);

    // Structured answers travel beside the text (MCP 2025-03-26+).
    assert!(by_id[&3]["result"]["structuredContent"].is_object());

    // get_callers: the call site resolves to the definition.
    let text = by_id[&3]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("lib.rs:3 (resolved → lib.rs:1)"),
        "callers text: {text}"
    );

    // call_tree: root definition then the caller site.
    let text = by_id[&4]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("lib.rs:1"), "tree: {text}");
    assert!(text.contains("lib.rs:3"), "tree: {text}");

    // get_references: definition + call classified.
    let text = by_id[&5]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("lib.rs:1  definition"), "refs: {text}");
    assert!(text.contains("lib.rs:3  call → lib.rs:1"), "refs: {text}");

    // stats: structured JSON with counts.
    let text = by_id[&6]["result"]["content"][0]["text"].as_str().unwrap();
    let stats: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(stats["files"], 1);
    assert_eq!(stats["symbols"], 2);
    assert!(stats["resolved"].as_u64().unwrap() >= 1);
}
