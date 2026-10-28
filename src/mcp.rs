//! MCP (Model Context Protocol) server over stdio: newline-delimited
//! JSON-RPC 2.0. Seven tools over the local index.
//!
//! stdout carries protocol messages only — every log line goes to
//! stderr, or clients break. Tool results state the resolution tier
//! verbatim: an agent reading `candidate (method-call)` knows the answer
//! is a guess, and that is the point.

use crate::query::{self, CallNode};
use crate::store::{self, Store};
use crate::yantrik;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Used when the client does not name a protocol version.
pub const PROTOCOL_FALLBACK: &str = "2024-11-05";

pub const TOOL_NAMES: &[&str] = &[
    "search_symbols",
    "get_definition",
    "get_callers",
    "get_references",
    "call_tree",
    "blast_radius",
    "get_symbol",
    "stats",
];

pub struct McpServer {
    root: PathBuf,
    db_path: PathBuf,
    /// Incremental re-index before each tool call: answers must reflect
    /// the checkout, and hashing a small tree is cheap. `--no-refresh`
    /// turns it off for huge repos or read-only settings.
    refresh: bool,
    /// Semantic layer (`--semantic` + CODEINDEX_YANTRIK_URL): a
    /// dedicated YantrikDB instance, never the agent memory one.
    semantic: Option<yantrik::Client>,
}

impl McpServer {
    pub fn new(root: PathBuf, refresh: bool, semantic: bool) -> McpServer {
        let db_path = root.join(".codeindex").join("index.db");
        let semantic = if semantic {
            yantrik::Config::from_env().map(yantrik::Client::new)
        } else {
            None
        };
        McpServer {
            root,
            db_path,
            refresh,
            semantic,
        }
    }

    fn store(&self) -> Result<Store> {
        Store::open(&self.db_path)
    }

    /// One JSON-RPC message in, at most one response out (notifications
    /// get `None`).
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(|m| m.as_str()) else {
            return id.map(|id| rpc_error(id, -32600, "invalid request".to_string()));
        };
        let id = id?; // notification: never answered
        let result = match method {
            "initialize" => Ok(self.initialize(msg)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.tools_call(msg.get("params").cloned().unwrap_or(json!({}))),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => rpc_error(id, code, message),
        })
    }

    fn initialize(&self, msg: &Value) -> Value {
        let requested = msg
            .pointer("/params/protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(PROTOCOL_FALLBACK);
        json!({
            "protocolVersion": requested,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "codeindex", "version": env!("CARGO_PKG_VERSION") }
        })
    }

    fn tools_call(&self, params: Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !TOOL_NAMES.contains(&name.as_str()) {
            return Err((-32602, format!("unknown tool: {name}")));
        }
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        Ok(match self.run_tool(&name, &args) {
            Ok(text) => json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false
            }),
            // Tool failures are results with isError, not protocol errors.
            Err(e) => json!({
                "content": [{ "type": "text", "text": format!("error: {e:#}") }],
                "isError": true
            }),
        })
    }

    fn run_tool(&self, name: &str, args: &Value) -> Result<String> {
        if self.refresh {
            let stats = crate::index(&self.root)?;
            if stats.parsed > 0 {
                eprintln!(
                    "codeindex: refreshed {} file(s), {} symbols, {} edges",
                    stats.parsed, stats.symbols, stats.edges
                );
                if let Some(client) = &self.semantic {
                    let store = self.store()?;
                    let synced = yantrik::sync(&store, client)?;
                    eprintln!(
                        "codeindex: semantic sync — {} pushed, {} present, {} forgotten",
                        synced.pushed, synced.existing, synced.forgotten
                    );
                }
            }
        }
        let store = self.store()?;
        let sarg = |key: &str| args.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let narg = |key: &str, default: usize| {
            args.get(key)
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(default)
        };
        Ok(match name {
            "search_symbols" => {
                let query = sarg("query").ok_or_else(|| anyhow::anyhow!("missing `query`"))?;
                let limit = narg("limit", 50);
                let mode = args
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("lexical");
                if mode == "semantic" {
                    let Some(client) = &self.semantic else {
                        anyhow::bail!(
                            "semantic mode needs the server started with --semantic \
                             and CODEINDEX_YANTRIK_URL set"
                        );
                    };
                    let hits = yantrik::hybrid_search(&store, client, &query, limit)?;
                    let mut out = format!(
                        "{} semantic hit(s) for \"{query}\" (namespace {}):\n",
                        hits.len(),
                        client.config().namespace
                    );
                    for h in &hits {
                        let score = if h.source == "lexical" {
                            "lex  ".to_string()
                        } else {
                            format!("{:.2} ", h.score)
                        };
                        let signature = if h.signature.is_empty() {
                            "(name match)".to_string()
                        } else {
                            h.signature.clone()
                        };
                        out.push_str(&format!(
                            "  {} {:>7} {} {}:{}  {}\n",
                            score, h.kind, h.name, h.file, h.line, signature
                        ));
                    }
                    return Ok(out);
                }
                let kind = sarg("kind");
                let path = sarg("path");
                let hits = store.search_symbols(&query, kind.as_deref(), path.as_deref())?;
                let shown = hits.len().min(limit);
                let mut out = format!("{} match(es) for \"{query}\":\n", hits.len());
                for s in hits.iter().take(limit) {
                    out.push_str(&format!(
                        "  {:>7} {} {}:{}\n",
                        s.kind, s.name, s.file, s.line
                    ));
                }
                if shown < hits.len() {
                    out.push_str(&format!("  … {} more\n", hits.len() - shown));
                }
                out
            }
            "get_definition" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let defs = store.definitions(&name)?;
                if defs.is_empty() {
                    format!("no symbol named \"{name}\"\n")
                } else {
                    let mut out = format!("{} definition(s) of \"{name}\":\n", defs.len());
                    for d in &defs {
                        out.push_str(&format!(
                            "  {:>7} {}:{}  ({})\n",
                            d.kind, d.file, d.line, d.kind
                        ));
                    }
                    out
                }
            }
            "get_callers" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let refs = store.callers(&name)?;
                let mut out = format!("{} call site(s) of \"{name}\":\n", refs.len());
                for r in &refs {
                    match (&r.target, &r.reason) {
                        (Some((f, l)), _) => out.push_str(&format!(
                            "  {}:{} ({} → {}:{})\n",
                            r.file, r.line, r.tier, f, l
                        )),
                        (None, Some(reason)) => out.push_str(&format!(
                            "  {}:{} ({}, {})\n",
                            r.file, r.line, r.tier, reason
                        )),
                        _ => out.push_str(&format!("  {}:{} ({})\n", r.file, r.line, r.tier)),
                    }
                }
                out
            }
            "get_references" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let limit = narg("limit", 100);
                let refs = query::references(&store, &self.root, &name, limit)?;
                let mut out = format!(
                    "{} reference(s) to \"{name}\" (lexical occurrences in indexed files):\n",
                    refs.len()
                );
                for r in &refs {
                    match &r.target {
                        Some((f, l)) => out.push_str(&format!(
                            "  {}:{}  {} → {}:{}\n",
                            r.file, r.line, r.kind, f, l
                        )),
                        None => out.push_str(&format!("  {}:{}  {}\n", r.file, r.line, r.kind)),
                    }
                }
                out
            }
            "call_tree" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let depth = narg("depth", 3);
                let tree = query::call_tree(&store, &name, depth)?;
                if tree.is_empty() {
                    format!("no symbol named \"{name}\"\n")
                } else {
                    let mut out = format!("callers of \"{name}\" (resolved edges only):\n");
                    render_tree(&mut out, &tree, 1);
                    out
                }
            }
            "blast_radius" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let affected = query::blast_radius(&store, &name)?;
                let mut out = format!(
                    "{} transitive call site(s) of \"{name}\":\n",
                    affected.len()
                );
                for a in &affected {
                    out.push_str(&format!("  d{} {}:{}\n", a.depth, a.file, a.line));
                }
                out
            }
            "get_symbol" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let file = sarg("file");
                let max_lines = narg("max_lines", 120);
                match query::symbol_source(&store, &self.root, &name, file.as_deref(), max_lines)? {
                    Some(src) => format!(
                        "{} {} at {}:{}-{}:\n{}",
                        src.kind, src.name, src.file, src.start, src.end, src.text
                    ),
                    None => format!("no symbol named \"{name}\"\n"),
                }
            }
            "stats" => {
                let (resolved, candidate, unresolved) = store.edge_tier_counts()?;
                let mut value = json!({
                    "root": self.root.display().to_string(),
                    "db": self.db_path.display().to_string(),
                    "schema_version": store::SCHEMA_VERSION,
                    "extractor_version": store::EXTRACTOR_VERSION,
                    "files": store.file_count()?,
                    "symbols": store.symbol_count()?,
                    "symbols_by_kind": dict(&store.symbol_kind_counts()?),
                    "edges": resolved + candidate + unresolved,
                    "resolved": resolved,
                    "candidate": candidate,
                    "unresolved": unresolved,
                    "edge_reasons": dict(&store.edge_reason_counts()?),
                });
                value["refresh_on_call"] = json!(self.refresh);
                if let Some(client) = &self.semantic {
                    let cfg = client.config();
                    value["semantic"] = json!({
                        "url": cfg.url,
                        "namespace": cfg.namespace,
                        "domain": cfg.domain,
                    });
                }
                let mut text = serde_json::to_string_pretty(&value)?;
                text.push('\n');
                text
            }
            other => unreachable!("TOOL_NAMES checked before dispatch: {other}"),
        })
    }
}

fn dict(counts: &[(String, usize)]) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in counts {
        map.insert(k.clone(), json!(v));
    }
    Value::Object(map)
}

fn render_tree(out: &mut String, nodes: &[CallNode], indent: usize) {
    for node in nodes {
        out.push_str(&format!(
            "{}{}:{}\n",
            "  ".repeat(indent),
            node.file,
            node.line
        ));
        render_tree(out, &node.children, indent + 1);
    }
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool(name: &str, description: &str, properties: Value, required: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required
        }
    })
}

fn tools() -> Value {
    json!([
        tool(
            "search_symbols",
            "Find indexed symbols whose name contains the query (case-insensitive): fns, structs, enums, traits, impls, mods.",
            json!({
                "query": { "type": "string", "description": "Substring of a symbol name, or a natural-language description in semantic mode" },
                "limit": { "type": "integer", "minimum": 1, "description": "Max results (default 50)" },
                "mode": { "type": "string", "enum": ["lexical", "semantic"], "description": "lexical = name substring (default); semantic = recall from the dedicated YantrikDB code instance (needs --semantic)" },
                "kind": { "type": "string", "description": "Exact kind filter: fn, struct, enum, trait, impl, mod, union" },
                "path": { "type": "string", "description": "Only symbols whose file path contains this substring" }
            }),
            json!(["query"])
        ),
        tool(
            "get_definition",
            "Exact-name definitions: file, line and kind. A name may exist in several modules; all are listed until the resolver disambiguates.",
            json!({ "name": { "type": "string", "description": "Exact symbol name" } }),
            json!(["name"])
        ),
        tool(
            "get_callers",
            "Call sites of this name with their resolution tier (resolved → target, candidate/unresolved with a reason). Never pretends a guess is a fact.",
            json!({ "name": { "type": "string", "description": "Exact callee name" } }),
            json!(["name"])
        ),
        tool(
            "get_references",
            "Every lexical identifier occurrence of the name in the indexed files: definitions, calls (with resolved targets) and plain mentions (type positions, use paths, patterns).",
            json!({
                "name": { "type": "string", "description": "Exact identifier" },
                "limit": { "type": "integer", "minimum": 1, "description": "Max results (default 100)" }
            }),
            json!(["name"])
        ),
        tool(
            "call_tree",
            "Who calls this symbol, transitively upward. Follows resolved edges only; depth-limited.",
            json!({
                "name": { "type": "string", "description": "Exact symbol name" },
                "depth": { "type": "integer", "minimum": 1, "description": "Max caller depth (default 3)" }
            }),
            json!(["name"])
        ),
        tool(
            "blast_radius",
            "Everything that transitively calls this symbol through resolved edges — what a change here can reach.",
            json!({ "name": { "type": "string", "description": "Exact symbol name" } }),
            json!(["name"])
        ),
        tool(
            "get_symbol",
            "Source of a symbol: declaration through its closing brace (fns) or the declaration line, read from the file.",
            json!({
                "name": { "type": "string", "description": "Exact symbol name" },
                "file": { "type": "string", "description": "Restrict to this file (exact or suffix)" },
                "max_lines": { "type": "integer", "minimum": 1, "description": "Line cap (default 120)" }
            }),
            json!(["name"])
        ),
        tool(
            "stats",
            "Index statistics: files, symbols by kind, edges by resolution tier and the reason breakdown.",
            json!({}),
            json!([])
        )
    ])
}

/// Serve MCP over stdin/stdout until EOF. Indexes the root once at
/// startup so the first answer is about real data.
pub fn run_stdio(root: PathBuf, refresh: bool, semantic: bool) -> Result<()> {
    let server = McpServer::new(root, refresh, semantic);
    let stats = crate::index(&server.root)?;
    eprintln!(
        "codeindex MCP: root={}, {} file(s), {} symbols, {} edges; per-call refresh: {}; semantic: {}",
        server.root.display(),
        stats.files_seen,
        stats.symbols,
        stats.edges,
        if server.refresh { "on" } else { "off" },
        match &server.semantic {
            Some(c) => format!("{} (namespace {})", c.config().url, c.config().namespace),
            None => "off".to_string(),
        }
    );
    if let Some(client) = &server.semantic {
        let store = server.store()?;
        let synced = yantrik::sync(&store, client)?;
        eprintln!(
            "codeindex: semantic sync at startup — {} pushed, {} present, {} forgotten",
            synced.pushed, synced.existing, synced.forgotten
        );
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let resp = rpc_error(Value::Null, -32700, format!("parse error: {e}"));
                writeln!(stdout, "{resp}")?;
                stdout.flush()?;
                continue;
            }
        };
        if let Some(resp) = server.handle(&msg) {
            writeln!(stdout, "{resp}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, McpServer) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn helper() {}\nfn caller() {\n    helper();\n    let x = helper;\n    x();\n}\n",
        )
        .unwrap();
        crate::index(dir.path()).unwrap();
        let server = McpServer::new(dir.path().to_path_buf(), false, false);
        (dir, server)
    }

    fn call(server: &McpServer, id: u64, method: &str, params: Value) -> Value {
        server
            .handle(&json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params
            }))
            .unwrap()
    }

    fn tool_text(server: &McpServer, tool: &str, args: Value) -> String {
        let resp = call(
            server,
            9,
            "tools/call",
            json!({ "name": tool, "arguments": args }),
        );
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text: {resp}"))
            .to_string()
    }

    #[test]
    fn initialize_echoes_the_requested_protocol_version() {
        let (_d, server) = fixture();
        let resp = call(
            &server,
            1,
            "initialize",
            json!({ "protocolVersion": "2025-03-26" }),
        );
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "codeindex");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_exposes_exactly_the_eight_tools() {
        let (_d, server) = fixture();
        let resp = call(&server, 2, "tools/list", json!({}));
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOL_NAMES);
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn search_and_definition_answer_from_the_index() {
        let (_d, server) = fixture();
        let text = tool_text(&server, "search_symbols", json!({ "query": "hel" }));
        assert!(text.contains("helper"), "{text}");
        let text = tool_text(&server, "get_definition", json!({ "name": "helper" }));
        assert!(text.contains("lib.rs:1"), "{text}");
    }

    #[test]
    fn callers_and_references_classify_honestly() {
        let (_d, server) = fixture();
        let text = tool_text(&server, "get_callers", json!({ "name": "helper" }));
        // `helper()` resolves; `x()` is a plain call to a value, unresolved.
        assert!(text.contains("lib.rs:3 (resolved → lib.rs:1)"), "{text}");
        let text = tool_text(&server, "get_references", json!({ "name": "helper" }));
        assert!(text.contains("lib.rs:1  definition"), "{text}");
        assert!(text.contains("lib.rs:3  call → lib.rs:1"), "{text}");
        assert!(text.contains("lib.rs:4  mention"), "{text}");
    }

    #[test]
    fn get_symbol_returns_the_body() {
        let (_d, server) = fixture();
        let text = tool_text(&server, "get_symbol", json!({ "name": "caller" }));
        assert!(text.contains("fn caller()"), "{text}");
        assert!(text.contains("helper();"), "{text}");
    }

    #[test]
    fn call_tree_and_blast_radius_walk_resolved_edges() {
        let (_d, server) = fixture();
        let text = tool_text(&server, "call_tree", json!({ "name": "helper" }));
        assert!(text.contains("lib.rs:1"), "{text}");
        assert!(text.contains("lib.rs:3"), "{text}");
        let text = tool_text(&server, "blast_radius", json!({ "name": "helper" }));
        assert!(text.contains("d1 lib.rs:3"), "{text}");
    }

    #[test]
    fn stats_reports_tiers_and_reasons() {
        let (_d, server) = fixture();
        let text = tool_text(&server, "stats", json!({}));
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["files"], 1);
        assert_eq!(v["symbols"], 2);
        assert!(v["resolved"].as_u64().unwrap() >= 1);
        assert!(v["edge_reasons"].is_object());
    }

    #[test]
    fn notifications_get_no_response() {
        let (_d, server) = fixture();
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(server.handle(&note).is_none());
    }

    #[test]
    fn unknown_method_and_unknown_tool_are_protocol_errors() {
        let (_d, server) = fixture();
        let resp = call(&server, 3, "bogus/method", json!({}));
        assert_eq!(resp["error"]["code"], -32601);
        let resp = call(
            &server,
            4,
            "tools/call",
            json!({ "name": "no_such_tool", "arguments": {} }),
        );
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn tool_failures_are_results_with_is_error() {
        let (_d, server) = fixture();
        let resp = call(
            &server,
            5,
            "tools/call",
            json!({ "name": "get_definition", "arguments": {} }),
        );
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing"));
    }
}
