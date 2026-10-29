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

/// A tool's answer: human-readable text plus the same facts as data.
/// Older clients read `content`; newer ones (MCP 2025-03-26+) read
/// `structuredContent` against `outputSchema`.
pub struct ToolOutput {
    pub text: String,
    pub structured: Value,
}

/// Used when the client does not name a protocol version.
pub const PROTOCOL_FALLBACK: &str = "2024-11-05";

pub const TOOL_NAMES: &[&str] = &[
    "search_symbols",
    "get_definition",
    "get_callers",
    "get_references",
    "call_tree",
    "callee_tree",
    "get_hierarchy",
    "trace_path",
    "profile",
    "external_calls",
    "skim_source",
    "argument_provenance",
    "list_files",
    "list_dependencies",
    "list_unresolved_calls",
    "list_unlisted_files",
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
            yantrik::Config::for_root(&root).map(yantrik::Client::new)
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
            Ok(out) => json!({
                "content": [{ "type": "text", "text": out.text }],
                "structuredContent": out.structured,
                "isError": false
            }),
            // Tool failures are results with isError, not protocol errors.
            Err(e) => json!({
                "content": [{ "type": "text", "text": format!("error: {e:#}") }],
                "structuredContent": { "error": format!("{e:#}") },
                "isError": true
            }),
        })
    }

    fn run_tool(&self, name: &str, args: &Value) -> Result<ToolOutput> {
        if self.refresh {
            let stats = crate::index(&self.root)?;
            if stats.parsed > 0 {
                eprintln!(
                    "codeindex: refreshed {} file(s), {} symbols, {} edges",
                    stats.parsed, stats.symbols, stats.edges
                );
                if let Some(client) = &self.semantic {
                    // A semantic hiccup (embedder down, instance restarting)
                    // must not break lexical answers: the SQLite index is
                    // local and fresh regardless.
                    match self.store().and_then(|store| yantrik::sync(&store, client)) {
                        Ok(synced) => eprintln!(
                            "codeindex: semantic sync — {} pushed, {} present, {} forgotten",
                            synced.pushed, synced.existing, synced.forgotten
                        ),
                        Err(e) => eprintln!("codeindex: semantic sync skipped — {e:#}"),
                    }
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
                    let mut text = format!(
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
                        text.push_str(&format!(
                            "  {} {:>7} {} {}:{}  {}\n",
                            score, h.kind, h.name, h.file, h.line, signature
                        ));
                    }
                    let structured = json!({
                        "query": query,
                        "mode": "semantic",
                        "hits": hits.iter().map(|h| json!({
                            "source": h.source,
                            "score": h.score,
                            "kind": h.kind,
                            "name": h.name,
                            "file": h.file,
                            "line": h.line,
                            "signature": h.signature,
                        })).collect::<Vec<_>>(),
                    });
                    return Ok(ToolOutput { text, structured });
                }
                let kind = sarg("kind");
                let path = sarg("path");
                let hits = store.search_symbols(&query, kind.as_deref(), path.as_deref())?;
                let shown = hits.len().min(limit);
                let mut text = format!("{} match(es) for \"{query}\":\n", hits.len());
                for s in hits.iter().take(limit) {
                    text.push_str(&format!(
                        "  {:>7} {} {}:{}\n",
                        s.kind, s.name, s.file, s.line
                    ));
                }
                if shown < hits.len() {
                    text.push_str(&format!("  … {} more\n", hits.len() - shown));
                }
                let structured = json!({
                    "query": query,
                    "mode": "lexical",
                    "matches": hits.iter().take(limit).map(|s| json!({
                        "kind": s.kind, "name": s.name, "file": s.file, "line": s.line,
                    })).collect::<Vec<_>>(),
                    "truncated": shown < hits.len(),
                });
                ToolOutput { text, structured }
            }
            "get_definition" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let defs = store.definitions(&name)?;
                let text = if defs.is_empty() {
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
                };
                let structured = json!({
                    "name": name,
                    "definitions": defs.iter().map(|d| json!({
                        "kind": d.kind, "name": d.name, "file": d.file, "line": d.line,
                    })).collect::<Vec<_>>(),
                });
                ToolOutput { text, structured }
            }
            "get_callers" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let refs = store.callers(&name)?;
                let mut text = format!("{} call site(s) of \"{name}\":\n", refs.len());
                for r in &refs {
                    match (&r.target, &r.external_crate, &r.reason) {
                        (Some((f, l)), _, _) => text.push_str(&format!(
                            "  {}:{} ({} → {}:{})\n",
                            r.file, r.line, r.tier, f, l
                        )),
                        (None, Some(crate_name), _) => text.push_str(&format!(
                            "  {}:{} (external: {})\n",
                            r.file, r.line, crate_name
                        )),
                        (None, None, Some(reason)) => text.push_str(&format!(
                            "  {}:{} ({}, {})\n",
                            r.file, r.line, r.tier, reason
                        )),
                        _ => text.push_str(&format!("  {}:{} ({})\n", r.file, r.line, r.tier)),
                    }
                }
                let structured = json!({
                    "name": name,
                    "callers": refs.iter().map(|r| json!({
                        "file": r.file,
                        "line": r.line,
                        "tier": r.tier,
                        "target": r.target.as_ref().map(|(f, l)| json!({"file": f, "line": l})),
                        "reason": r.reason,
                        "external_crate": r.external_crate,
                    })).collect::<Vec<_>>(),
                });
                ToolOutput { text, structured }
            }
            "get_references" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let limit = narg("limit", 100);
                let refs = query::references(&store, &self.root, &name, limit)?;
                let mut text = format!(
                    "{} reference(s) to \"{name}\" (lexical occurrences in indexed files):\n",
                    refs.len()
                );
                for r in &refs {
                    match &r.target {
                        Some((f, l)) => text.push_str(&format!(
                            "  {}:{}  {} → {}:{}\n",
                            r.file, r.line, r.kind, f, l
                        )),
                        None => text.push_str(&format!("  {}:{}  {}\n", r.file, r.line, r.kind)),
                    }
                }
                let structured = json!({
                    "name": name,
                    "references": refs.iter().map(|r| json!({
                        "file": r.file,
                        "line": r.line,
                        "kind": r.kind,
                        "target": r.target.as_ref().map(|(f, l)| json!({"file": f, "line": l})),
                    })).collect::<Vec<_>>(),
                });
                ToolOutput { text, structured }
            }
            "call_tree" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let depth = narg("depth", 3);
                let tree = query::call_tree(&store, &name, depth)?;
                let text = if tree.is_empty() {
                    format!("no symbol named \"{name}\"\n")
                } else {
                    let mut out = format!("callers of \"{name}\" (resolved edges only):\n");
                    render_tree(&mut out, &tree, 1);
                    out
                };
                let structured = json!({
                    "name": name,
                    "roots": tree.iter().map(node_value).collect::<Vec<_>>(),
                });
                ToolOutput { text, structured }
            }
            "callee_tree" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let depth = narg("depth", 3);
                let tree = query::callee_tree(&store, &name, depth)?;
                let text = if tree.is_empty() {
                    format!("no symbol named \"{name}\"\n")
                } else {
                    let mut out = format!("callees of \"{name}\" (resolved edges only):\n");
                    render_callees(&mut out, &tree, 1);
                    out
                };
                let structured = json!({
                    "name": name,
                    "roots": tree.iter().map(callee_value).collect::<Vec<_>>(),
                });
                ToolOutput { text, structured }
            }
            "get_hierarchy" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let h = query::hierarchy(&store, &name)?;
                let mut text = format!(
                    "{}: {}\n",
                    h.name,
                    if h.kinds.is_empty() {
                        "unknown".to_string()
                    } else {
                        h.kinds.join(", ")
                    }
                );
                if !h.traits.is_empty() {
                    text.push_str(&format!("  implements: {}\n", h.traits.join(", ")));
                }
                if !h.implementors.is_empty() {
                    text.push_str(&format!(
                        "  implemented by: {}\n",
                        h.implementors.join(", ")
                    ));
                }
                if !h.supertraits.is_empty() {
                    text.push_str(&format!("  supertraits: {}\n", h.supertraits.join(", ")));
                }
                let structured = json!({
                    "name": h.name,
                    "kinds": h.kinds,
                    "traits": h.traits,
                    "implementors": h.implementors,
                    "supertraits": h.supertraits,
                });
                ToolOutput { text, structured }
            }
            "trace_path" => {
                let from = sarg("from").ok_or_else(|| anyhow::anyhow!("missing `from`"))?;
                let to = sarg("to").ok_or_else(|| anyhow::anyhow!("missing `to`"))?;
                let steps = query::trace_path(&store, &from, &to)?;
                let text = match &steps {
                    None => format!("no resolved call chain from \"{from}\" to \"{to}\"\n"),
                    Some(steps) => {
                        let mut out = format!(
                            "call chain \"{from}\" → \"{to}\" ({} hop(s)):\n",
                            steps.len().saturating_sub(1)
                        );
                        for step in steps {
                            match &step.via {
                                Some((f, l)) => out.push_str(&format!(
                                    "  → {}:{}  (called at {}:{})\n",
                                    step.file, step.line, f, l
                                )),
                                None => out.push_str(&format!("  {}:{}\n", step.file, step.line)),
                            }
                        }
                        out
                    }
                };
                let structured = json!({
                    "from": from,
                    "to": to,
                    "path": steps.as_ref().map(|steps| steps.iter().map(|s| json!({
                        "file": s.file,
                        "line": s.line,
                        "via": s.via.as_ref().map(|(f, l)| json!({"file": f, "line": l})),
                    })).collect::<Vec<_>>()),
                });
                ToolOutput { text, structured }
            }
            "profile" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let p = query::profile(&store, &self.root, &name)?;
                let mut text = format!("{} ({} definition(s)):\n", p.name, p.definitions.len());
                for d in &p.definitions {
                    text.push_str(&format!("  {:>7} {}:{}\n", d.kind, d.file, d.line));
                }
                text.push_str(&format!(
                    "  callers: {} resolved, {} candidate, {} unresolved\n",
                    p.callers.0, p.callers.1, p.callers.2
                ));
                if !p.callees.is_empty() {
                    let list: Vec<String> =
                        p.callees.iter().map(|(f, l)| format!("{f}:{l}")).collect();
                    text.push_str(&format!("  calls: {}\n", list.join(", ")));
                }
                let kinds: Vec<String> = p
                    .references
                    .1
                    .iter()
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect();
                text.push_str(&format!(
                    "  references: {} ({})\n",
                    p.references.0,
                    kinds.join(", ")
                ));
                if !p.hierarchy.traits.is_empty() {
                    text.push_str(&format!(
                        "  implements: {}\n",
                        p.hierarchy.traits.join(", ")
                    ));
                }
                if !p.hierarchy.implementors.is_empty() {
                    text.push_str(&format!(
                        "  implemented by: {}\n",
                        p.hierarchy.implementors.join(", ")
                    ));
                }
                let structured = json!({
                    "name": p.name,
                    "definitions": p.definitions.iter().map(|d| json!({
                        "kind": d.kind, "name": d.name, "file": d.file, "line": d.line,
                    })).collect::<Vec<_>>(),
                    "callers": { "resolved": p.callers.0, "candidate": p.callers.1, "unresolved": p.callers.2 },
                    "callees": p.callees.iter().map(|(f, l)| json!({"file": f, "line": l})).collect::<Vec<_>>(),
                    "references": { "total": p.references.0, "by_kind": p.references.1 },
                    "hierarchy": {
                        "kinds": p.hierarchy.kinds,
                        "traits": p.hierarchy.traits,
                        "implementors": p.hierarchy.implementors,
                        "supertraits": p.hierarchy.supertraits,
                    },
                });
                ToolOutput { text, structured }
            }
            "external_calls" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let e = query::external_calls(&store, &name)?;
                let mut text = format!("external crates used by {}:\n", e.name);
                for (c, n) in &e.calls {
                    text.push_str(&format!("  calls   {c} ({n})\n"));
                }
                for (c, n) in &e.imports {
                    text.push_str(&format!("  imports {c} ({n})\n"));
                }
                if e.calls.is_empty() && e.imports.is_empty() {
                    text.push_str("  none\n");
                }
                let structured = json!({
                    "name": e.name,
                    "calls": e.calls.iter().map(|(c, n)| json!({"crate": c, "count": n})).collect::<Vec<_>>(),
                    "imports": e.imports.iter().map(|(c, n)| json!({"crate": c, "count": n})).collect::<Vec<_>>(),
                });
                ToolOutput { text, structured }
            }
            "skim_source" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let file = sarg("file");
                let max_items = narg("max_items", 40);
                match query::skim(&store, &name, file.as_deref(), max_items)? {
                    Some(skim) => {
                        let mut text = format!(
                            "{} {} at {}:{}-{}\n  {}\n",
                            skim.kind, skim.name, skim.file, skim.start, skim.end, skim.signature
                        );
                        for i in &skim.items {
                            text.push_str(&format!(
                                "  {:>7} {}:{}  {}\n",
                                i.kind, i.name, i.line, i.signature
                            ));
                        }
                        if skim.items.is_empty() {
                            text.push_str("  (no nested items)\n");
                        }
                        let structured = json!({
                            "name": skim.name,
                            "kind": skim.kind,
                            "file": skim.file,
                            "start": skim.start,
                            "end": skim.end,
                            "signature": skim.signature,
                            "items": skim.items.iter().map(|i| json!({
                                "kind": i.kind, "name": i.name, "line": i.line, "signature": i.signature,
                            })).collect::<Vec<_>>(),
                        });
                        ToolOutput { text, structured }
                    }
                    None => ToolOutput {
                        text: format!("no symbol named \"{name}\"\n"),
                        structured: json!({ "name": name, "found": false }),
                    },
                }
            }
            "argument_provenance" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let max_sites = narg("max_sites", 20);
                match query::argument_provenance(&store, &self.root, &name, max_sites)? {
                    Some(p) => {
                        let mut text = format!(
                            "{} (params: {})\n",
                            p.name,
                            if p.params.is_empty() {
                                "—".to_string()
                            } else {
                                p.params.join(", ")
                            }
                        );
                        for c in &p.call_sites {
                            text.push_str(&format!(
                                "  {}:{} ({}) args: [{}]\n",
                                c.file,
                                c.line,
                                c.tier,
                                c.args.join(", ")
                            ));
                        }
                        if p.call_sites.is_empty() {
                            text.push_str("  no resolved call sites\n");
                        }
                        let structured = json!({
                            "name": p.name,
                            "params": p.params,
                            "call_sites": p.call_sites.iter().map(|c| json!({
                                "file": c.file, "line": c.line, "tier": c.tier, "args": c.args,
                            })).collect::<Vec<_>>(),
                        });
                        ToolOutput { text, structured }
                    }
                    None => ToolOutput {
                        text: format!("no fn named \"{name}\"\n"),
                        structured: json!({ "name": name, "found": false }),
                    },
                }
            }
            "list_files" => {
                let files = query::list_files(&store, &self.root)?;
                let mut text = format!("{} indexed file(s):\n", files.len());
                for f in &files {
                    text.push_str(&format!(
                        "  {:>4} symbols {:>5} lines  {}\n",
                        f.symbols, f.lines, f.path
                    ));
                }
                let structured = json!({
                    "files": files.iter().map(|f| json!({
                        "path": f.path, "symbols": f.symbols, "lines": f.lines,
                    })).collect::<Vec<_>>(),
                });
                ToolOutput { text, structured }
            }
            "list_dependencies" => {
                let deps = query::list_dependencies(&store)?;
                let mut text = format!("external crates ({}):\n", deps.len());
                for d in &deps {
                    text.push_str(&format!(
                        "  {:20} calls:{} imports:{}\n",
                        d.crate_name, d.calls, d.imports
                    ));
                }
                let structured = json!({
                    "dependencies": deps.iter().map(|d| json!({
                        "crate": d.crate_name, "calls": d.calls, "imports": d.imports,
                    })).collect::<Vec<_>>(),
                });
                ToolOutput { text, structured }
            }
            "list_unresolved_calls" => {
                let file = sarg("file");
                let limit = narg("limit", 50);
                let rows = store.unresolved_calls(file.as_deref(), limit)?;
                let mut text = format!("{} unresolved call(s):\n", rows.len());
                for r in &rows {
                    text.push_str(&format!(
                        "  {}:{} {} — {}\n",
                        r.file, r.line, r.callee, r.reason
                    ));
                }
                let structured = json!({
                    "rows": rows.iter().map(|r| json!({
                        "file": r.file, "line": r.line, "callee": r.callee, "reason": r.reason,
                    })).collect::<Vec<_>>(),
                });
                ToolOutput { text, structured }
            }
            "list_unlisted_files" => {
                let unlisted = crate::unlisted_files(&self.root);
                let mut text = format!("{} entrie(s) outside the index:\n", unlisted.len());
                for u in &unlisted {
                    text.push_str(&format!("  {} — {}\n", u.path.display(), u.reason));
                }
                let structured = json!({
                    "unlisted": unlisted.iter().map(|u| json!({
                        "path": u.path.display().to_string(), "reason": u.reason,
                    })).collect::<Vec<_>>(),
                });
                ToolOutput { text, structured }
            }
            "blast_radius" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let affected = query::blast_radius(&store, &name)?;
                let mut text = format!(
                    "{} transitive call site(s) of \"{name}\":\n",
                    affected.len()
                );
                for a in &affected {
                    text.push_str(&format!("  d{} {}:{}\n", a.depth, a.file, a.line));
                }
                let structured = json!({
                    "name": name,
                    "affected": affected.iter().map(|a| json!({
                        "file": a.file, "line": a.line, "depth": a.depth,
                    })).collect::<Vec<_>>(),
                });
                ToolOutput { text, structured }
            }
            "get_symbol" => {
                let name = sarg("name").ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let file = sarg("file");
                let max_lines = narg("max_lines", 120);
                match query::symbol_source(&store, &self.root, &name, file.as_deref(), max_lines)? {
                    Some(src) => {
                        let text = format!(
                            "{} {} at {}:{}-{}:\n{}",
                            src.kind, src.name, src.file, src.start, src.end, src.text
                        );
                        let structured = json!({
                            "kind": src.kind,
                            "name": src.name,
                            "file": src.file,
                            "start": src.start,
                            "end": src.end,
                            "text": src.text,
                        });
                        ToolOutput { text, structured }
                    }
                    None => ToolOutput {
                        text: format!("no symbol named \"{name}\"\n"),
                        structured: json!({ "name": name, "found": false }),
                    },
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
                    "embeddings": store.embedding_count()?,
                    "external": store.external_edge_count()?,
                });
                value["refresh_on_call"] = json!(self.refresh);
                if let Some(client) = &self.semantic {
                    let cfg = client.config();
                    value["semantic"] = json!({
                        "url": cfg.url,
                        "namespace": cfg.namespace,
                        "domain": cfg.domain,
                        "embed_url": cfg.embed_url,
                        "embed_model": cfg.embed_model,
                    });
                }
                let mut text = serde_json::to_string_pretty(&value)?;
                text.push('\n');
                ToolOutput {
                    text,
                    structured: value,
                }
            }
            other => unreachable!("TOOL_NAMES checked before dispatch: {other}"),
        })
    }
}

fn callee_value(node: &query::CalleeNode) -> Value {
    json!({
        "file": node.file,
        "line": node.line,
        "call_line": node.call_line,
        "children": node.children.iter().map(callee_value).collect::<Vec<_>>(),
    })
}

fn render_callees(out: &mut String, nodes: &[query::CalleeNode], indent: usize) {
    for node in nodes {
        out.push_str(&format!(
            "{}{}:{} (called at parent:{})\n",
            "  ".repeat(indent),
            node.file,
            node.line,
            node.call_line
        ));
        render_callees(out, &node.children, indent + 1);
    }
}

fn node_value(node: &CallNode) -> Value {
    json!({
        "file": node.file,
        "line": node.line,
        "children": node.children.iter().map(node_value).collect::<Vec<_>>(),
    })
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

fn tool(name: &str, description: &str, properties: Value, required: Value, output: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required
        },
        "outputSchema": output
    })
}

/// Schema of one `{kind, name, file, line}` record.
fn symbol_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": { "type": "string" },
            "name": { "type": "string" },
            "file": { "type": "string" },
            "line": { "type": "integer" }
        },
        "required": ["kind", "name", "file", "line"]
    })
}

fn array_of(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

fn tree_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file": { "type": "string" },
            "line": { "type": "integer" },
            "children": { "type": "array" }
        },
        "required": ["file", "line", "children"]
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
            json!(["query"]),
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "mode": { "type": "string", "enum": ["lexical", "semantic"] },
                    "matches": array_of(symbol_schema()),
                    "hits": array_of(json!({
                        "type": "object",
                        "properties": {
                            "source": { "type": "string" },
                            "score": { "type": "number" },
                            "kind": { "type": "string" },
                            "name": { "type": "string" },
                            "file": { "type": "string" },
                            "line": { "type": "integer" },
                            "signature": { "type": "string" }
                        }
                    })),
                    "truncated": { "type": "boolean" }
                },
                "required": ["query", "mode"]
            })
        ),
        tool(
            "get_definition",
            "Exact-name definitions: file, line and kind. A name may exist in several modules; all are listed until the resolver disambiguates.",
            json!({ "name": { "type": "string", "description": "Exact symbol name" } }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "definitions": array_of(symbol_schema())
                },
                "required": ["name", "definitions"]
            })
        ),
        tool(
            "get_callers",
            "Call sites of this name with their resolution tier (resolved → target, candidate/unresolved with a reason). Never pretends a guess is a fact.",
            json!({ "name": { "type": "string", "description": "Exact callee name" } }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "callers": array_of(json!({
                        "type": "object",
                        "properties": {
                            "file": { "type": "string" },
                            "line": { "type": "integer" },
                            "tier": { "type": "string", "enum": ["resolved", "candidate", "unresolved"] },
                            "target": { "type": ["object", "null"] },
                            "reason": { "type": ["string", "null"] }
                        },
                        "required": ["file", "line", "tier"]
                    }))
                },
                "required": ["name", "callers"]
            })
        ),
        tool(
            "get_references",
            "Every lexical identifier occurrence of the name in the indexed files: definitions, calls (with resolved targets) and plain mentions (type positions, use paths, patterns).",
            json!({
                "name": { "type": "string", "description": "Exact identifier" },
                "limit": { "type": "integer", "minimum": 1, "description": "Max results (default 100)" }
            }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "references": array_of(json!({
                        "type": "object",
                        "properties": {
                            "file": { "type": "string" },
                            "line": { "type": "integer" },
                            "kind": { "type": "string", "enum": ["definition", "call", "mention"] },
                            "target": { "type": ["object", "null"] }
                        },
                        "required": ["file", "line", "kind"]
                    }))
                },
                "required": ["name", "references"]
            })
        ),
        tool(
            "call_tree",
            "Who calls this symbol, transitively upward. Follows resolved edges only; depth-limited.",
            json!({
                "name": { "type": "string", "description": "Exact symbol name" },
                "depth": { "type": "integer", "minimum": 1, "description": "Max caller depth (default 3)" }
            }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "roots": array_of(tree_schema())
                },
                "required": ["name", "roots"]
            })
        ),
        tool(
            "callee_tree",
            "What this symbol calls, transitively downward. Follows resolved edges only; depth-limited.",
            json!({
                "name": { "type": "string", "description": "Exact symbol name" },
                "depth": { "type": "integer", "minimum": 1, "description": "Max callee depth (default 3)" }
            }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "roots": array_of(tree_schema())
                },
                "required": ["name", "roots"]
            })
        ),
        tool(
            "get_hierarchy",
            "Type/trait hierarchy: traits a type implements, types implementing a trait, and a trait's supertraits.",
            json!({ "name": { "type": "string", "description": "Exact type or trait name" } }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "kinds": { "type": "array", "items": { "type": "string" } },
                    "traits": { "type": "array", "items": { "type": "string" } },
                    "implementors": { "type": "array", "items": { "type": "string" } },
                    "supertraits": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["name", "kinds"]
            })
        ),
        tool(
            "trace_path",
            "Call chain connecting two symbols, through resolved edges only. Shortest known chain, not a proof of uniqueness.",
            json!({
                "from": { "type": "string", "description": "Starting symbol name" },
                "to": { "type": "string", "description": "Target symbol name" }
            }),
            json!(["from", "to"]),
            json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string" },
                    "to": { "type": "string" },
                    "path": { "type": ["array", "null"], "items": tree_schema() }
                },
                "required": ["from", "to"]
            })
        ),
        tool(
            "profile",
            "Everything about one symbol in a single call: definitions, caller tiers, resolved callees, lexical references, hierarchy.",
            json!({ "name": { "type": "string", "description": "Exact symbol name" } }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "definitions": array_of(symbol_schema()),
                    "callers": { "type": "object" },
                    "callees": array_of(json!({
                        "type": "object",
                        "properties": {
                            "file": { "type": "string" },
                            "line": { "type": "integer" }
                        }
                    })),
                    "references": { "type": "object" },
                    "hierarchy": { "type": "object" }
                },
                "required": ["name", "definitions", "callers", "references", "hierarchy"]
            })
        ),
        tool(
            "external_calls",
            "External crates a symbol leans on: calls into them from its body and imports in its file. Labelled by the resolver, not guessed.",
            json!({ "name": { "type": "string", "description": "Exact symbol name" } }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "calls": array_of(json!({
                        "type": "object",
                        "properties": { "crate": { "type": "string" }, "count": { "type": "integer" } },
                        "required": ["crate", "count"]
                    })),
                    "imports": array_of(json!({
                        "type": "object",
                        "properties": { "crate": { "type": "string" }, "count": { "type": "integer" } },
                        "required": ["crate", "count"]
                    }))
                },
                "required": ["name", "calls", "imports"]
            })
        ),
        tool(
            "skim_source",
            "A body's outline: the declaration plus the items nested inside it, so you can tell which lines are worth reading.",
            json!({
                "name": { "type": "string", "description": "Exact symbol name" },
                "file": { "type": "string", "description": "Restrict to this file (exact or suffix)" },
                "max_items": { "type": "integer", "minimum": 1, "description": "Max nested items (default 40)" }
            }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "kind": { "type": "string" },
                    "file": { "type": "string" },
                    "start": { "type": "integer" },
                    "end": { "type": "integer" },
                    "signature": { "type": "string" },
                    "items": array_of(json!({
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string" },
                            "name": { "type": "string" },
                            "line": { "type": "integer" },
                            "signature": { "type": "string" }
                        },
                        "required": ["kind", "name", "line"]
                    }))
                },
                "required": ["name"]
            })
        ),
        tool(
            "argument_provenance",
            "A function's parameters and the raw source text of the arguments passed at every call site. Lexical evidence — what the code says, not what the values are.",
            json!({
                "name": { "type": "string", "description": "Exact fn name" },
                "max_sites": { "type": "integer", "minimum": 1, "description": "Max call sites (default 20)" }
            }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "params": { "type": "array", "items": { "type": "string" } },
                    "call_sites": array_of(json!({
                        "type": "object",
                        "properties": {
                            "file": { "type": "string" },
                            "line": { "type": "integer" },
                            "tier": { "type": "string" },
                            "args": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["file", "line", "tier", "args"]
                    }))
                },
                "required": ["name", "params", "call_sites"]
            })
        ),
        tool(
            "list_files",
            "Directory map: every indexed file with its symbol count and the deepest indexed line.",
            json!({}),
            json!([]),
            json!({
                "type": "object",
                "properties": {
                    "files": array_of(json!({
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "symbols": { "type": "integer" },
                            "lines": { "type": "integer" }
                        },
                        "required": ["path", "symbols", "lines"]
                    }))
                },
                "required": ["files"]
            })
        ),
        tool(
            "list_dependencies",
            "External crates the repository leans on, ranked by usage (call edges and imports).",
            json!({}),
            json!([]),
            json!({
                "type": "object",
                "properties": {
                    "dependencies": array_of(json!({
                        "type": "object",
                        "properties": {
                            "crate": { "type": "string" },
                            "calls": { "type": "integer" },
                            "imports": { "type": "integer" }
                        },
                        "required": ["crate", "calls", "imports"]
                    }))
                },
                "required": ["dependencies"]
            })
        ),
        tool(
            "list_unresolved_calls",
            "Calls the resolver could not settle, each with its reason — nothing is silently guessed.",
            json!({
                "file": { "type": "string", "description": "Scope to one file" },
                "limit": { "type": "integer", "minimum": 1, "description": "Max rows (default 50)" }
            }),
            json!([]),
            json!({
                "type": "object",
                "properties": {
                    "rows": array_of(json!({
                        "type": "object",
                        "properties": {
                            "file": { "type": "string" },
                            "line": { "type": "integer" },
                            "callee": { "type": "string" },
                            "reason": { "type": "string" }
                        },
                        "required": ["file", "line", "callee", "reason"]
                    }))
                },
                "required": ["rows"]
            })
        ),
        tool(
            "list_unlisted_files",
            "Files and directories the index does not cover, each with the reason (non-Rust, symlink, excluded).",
            json!({}),
            json!([]),
            json!({
                "type": "object",
                "properties": {
                    "unlisted": array_of(json!({
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "reason": { "type": "string" }
                        },
                        "required": ["path", "reason"]
                    }))
                },
                "required": ["unlisted"]
            })
        ),
        tool(
            "blast_radius",
            "Everything that transitively calls this symbol through resolved edges — what a change here can reach.",
            json!({ "name": { "type": "string", "description": "Exact symbol name" } }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "affected": array_of(json!({
                        "type": "object",
                        "properties": {
                            "file": { "type": "string" },
                            "line": { "type": "integer" },
                            "depth": { "type": "integer" }
                        },
                        "required": ["file", "line", "depth"]
                    }))
                },
                "required": ["name", "affected"]
            })
        ),
        tool(
            "get_symbol",
            "Source of a symbol: declaration through its closing brace (fns) or the declaration line, read from the file.",
            json!({
                "name": { "type": "string", "description": "Exact symbol name" },
                "file": { "type": "string", "description": "Restrict to this file (exact or suffix)" },
                "max_lines": { "type": "integer", "minimum": 1, "description": "Line cap (default 120)" }
            }),
            json!(["name"]),
            json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string" },
                    "name": { "type": "string" },
                    "file": { "type": "string" },
                    "start": { "type": "integer" },
                    "end": { "type": "integer" },
                    "text": { "type": "string" },
                    "found": { "type": "boolean" }
                },
                "required": ["name"]
            })
        ),
        tool(
            "stats",
            "Index statistics: files, symbols by kind, edges by resolution tier and the reason breakdown.",
            json!({}),
            json!([]),
            json!({
                "type": "object",
                "properties": {
                    "root": { "type": "string" },
                    "db": { "type": "string" },
                    "schema_version": { "type": "string" },
                    "extractor_version": { "type": "string" },
                    "files": { "type": "integer" },
                    "symbols": { "type": "integer" },
                    "symbols_by_kind": { "type": "object" },
                    "edges": { "type": "integer" },
                    "resolved": { "type": "integer" },
                    "candidate": { "type": "integer" },
                    "unresolved": { "type": "integer" },
                    "edge_reasons": { "type": "object" },
                    "embeddings": { "type": "integer" },
                    "refresh_on_call": { "type": "boolean" },
                    "semantic": { "type": "object" }
                },
                "required": ["files", "symbols", "edges", "resolved", "candidate", "unresolved"]
            })
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
        match server
            .store()
            .and_then(|store| yantrik::sync(&store, client))
        {
            Ok(synced) => eprintln!(
                "codeindex: semantic sync at startup — {} pushed, {} present, {} forgotten",
                synced.pushed, synced.existing, synced.forgotten
            ),
            Err(e) => eprintln!(
                "codeindex: semantic sync skipped at startup — {e:#}; \
                 lexical tools keep working, semantic search will report the error"
            ),
        }
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
    fn tools_list_exposes_exactly_the_nineteen_tools() {
        let (_d, server) = fixture();
        let resp = call(&server, 2, "tools/list", json!({}));
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOL_NAMES);
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object");
            assert_eq!(t["outputSchema"]["type"], "object");
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
    fn tool_results_carry_structured_content() {
        let (_d, server) = fixture();
        let resp = call(
            &server,
            9,
            "tools/call",
            json!({ "name": "get_definition", "arguments": { "name": "helper" } }),
        );
        let structured = &resp["result"]["structuredContent"];
        assert_eq!(structured["name"], "helper");
        assert_eq!(structured["definitions"][0]["file"], "lib.rs");
        assert_eq!(structured["definitions"][0]["line"], 1);
        assert_eq!(resp["result"]["isError"], false);
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
    fn inventory_tools_answer() {
        let (_d, server) = fixture();
        for (tool, args, needle) in [
            ("list_files", json!({}), "lib.rs"),
            ("list_dependencies", json!({}), "external crates"),
            ("list_unresolved_calls", json!({}), "unresolved call"),
            ("list_unlisted_files", json!({}), "outside the index"),
        ] {
            let text = tool_text(&server, tool, args);
            assert!(text.contains(needle), "{tool}: {text}");
        }
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
