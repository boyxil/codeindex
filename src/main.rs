//! codeindex — local code index for AI agents.
//! M0: walk a Rust tree, lex every file with the in-house lexer,
//! report the symbol inventory.
//! M1: incremental SQLite index at <path>/.codeindex/index.db.
//! M2: queries over the store — search, definition, callers.
//! M3: in-house name resolver — resolved edges, call_tree, blast_radius.
//! M4: MCP server over stdio (`serve`).
//! M5: semantic layer on a dedicated YantrikDB instance.

use anyhow::Result;
use clap::{Parser, Subcommand};
use codeindex::query::{self, CallNode};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "codeindex", version, about = "Local code index for AI agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Emit machine-readable JSON instead of human text.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Parse a Rust tree and report the symbol inventory.
    Scan {
        /// Repository root (walks *.rs under it, skips target/ and .git/).
        path: PathBuf,
    },
    /// Build or incrementally update the SQLite index at
    /// <path>/.codeindex/index.db.
    Index {
        /// Repository root.
        path: PathBuf,
        /// Also sync symbols into the dedicated YantrikDB code instance
        /// (CODEINDEX_YANTRIK_URL) for semantic search.
        #[arg(long)]
        semantic: bool,
    },
    /// Find symbols whose name contains the query (case-insensitive).
    Search {
        /// Indexed repository root.
        path: PathBuf,
        /// Substring of a symbol name.
        query: String,
        /// Only this exact kind: fn, struct, enum, trait, impl, mod, union.
        #[arg(long)]
        kind: Option<String>,
        /// Only symbols whose file path contains this substring.
        #[arg(long)]
        path_filter: Option<String>,
        /// Use semantic recall from the YantrikDB code instance
        /// (CODEINDEX_YANTRIK_URL) instead of substring matching.
        #[arg(long)]
        semantic: bool,
    },
    /// Show where a symbol with this exact name is defined.
    Definition {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact symbol name.
        name: String,
    },
    /// Print a symbol's source (declaration through closing brace for fns).
    Symbol {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact symbol name.
        name: String,
        /// Restrict to this file (exact path or suffix).
        #[arg(long)]
        file: Option<String>,
        /// Line cap.
        #[arg(long, default_value_t = 120)]
        max_lines: usize,
    },
    /// List call sites invoking this name, with their resolution tier.
    Callers {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact callee name.
        name: String,
    },
    /// Show who calls this symbol, transitively (resolved edges only).
    CallTree {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact symbol name.
        name: String,
        /// Maximum caller depth.
        #[arg(long, default_value_t = 3)]
        depth: usize,
    },
    /// List everything that transitively calls this symbol — what a
    /// change here can reach.
    BlastRadius {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact symbol name.
        name: String,
    },
    /// Every lexical occurrence of a name in the indexed files, with
    /// definition/call/mention classification.
    References {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact identifier.
        name: String,
        /// Maximum results.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Structural impact of a diff: changed symbols and what calls them.
    Impact {
        /// Indexed repository root.
        path: PathBuf,
        /// Revision to diff the working tree against (e.g. HEAD~1).
        #[arg(long)]
        since: String,
    },
    /// Index statistics: files, symbols by kind, edges by tier and reason.
    Stats {
        /// Indexed repository root.
        path: PathBuf,
    },
    /// Serve the index over MCP (stdio, JSON-RPC 2.0).
    Serve {
        /// Repository root, indexed at startup.
        path: PathBuf,
        /// Skip the incremental re-index before each tool call.
        #[arg(long)]
        no_refresh: bool,
        /// Keep the YantrikDB code instance in sync and allow semantic
        /// search (CODEINDEX_YANTRIK_URL).
        #[arg(long)]
        semantic: bool,
    },
}

fn open(path: &Path) -> Result<codeindex::store::Store> {
    codeindex::store::Store::open(&path.join(".codeindex/index.db"))
}

fn emit(json_mode: bool, value: Value, text: impl FnOnce()) {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(&value).expect("json"));
    } else {
        text();
    }
}

fn symbol_json(s: &codeindex::store::IndexedSymbol) -> Value {
    json!({ "kind": s.kind, "name": s.name, "file": s.file, "line": s.line })
}

fn node_json(n: &CallNode) -> Value {
    json!({
        "file": n.file,
        "line": n.line,
        "children": n.children.iter().map(node_json).collect::<Vec<_>>(),
    })
}

#[cfg(unix)]
fn silence_sigpipe() {
    // A closed pipe (`codeindex search … | head`) must end the program
    // quietly instead of panicking inside println!.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn silence_sigpipe() {}

fn main() -> Result<()> {
    silence_sigpipe();
    let cli = Cli::parse();
    let json_mode = cli.json;
    match cli.command {
        Command::Scan { path } => {
            let inv = codeindex::scan(&path)?;
            emit(
                json_mode,
                json!({
                    "files": inv.files,
                    "skipped": inv.skipped,
                    "symbols": inv.symbols.iter().map(|s| json!({
                        "kind": s.kind,
                        "name": s.name,
                        "file": s.file.display().to_string(),
                        "line": s.line,
                        "end_line": s.end_line,
                    })).collect::<Vec<_>>(),
                }),
                || {
                    println!(
                        "{} files parsed, {} symbols: {} fn, {} struct, {} enum, {} trait, {} impl, {} mod, {} union",
                        inv.files,
                        inv.total(),
                        inv.fn_count(),
                        inv.struct_count(),
                        inv.enum_count(),
                        inv.trait_count(),
                        inv.impl_count(),
                        inv.mod_count(),
                        inv.union_count()
                    );
                    for sym in inv.symbols.iter().take(20) {
                        println!("  {:>7} {}:{}", sym.kind, sym.file.display(), sym.line);
                        println!("          {}", sym.name);
                    }
                    if inv.symbols.len() > 20 {
                        println!("  … and {} more", inv.symbols.len() - 20);
                    }
                    if inv.skipped > 0 {
                        println!("  {} file(s) skipped: unreadable or not UTF-8", inv.skipped);
                    }
                },
            );
        }
        Command::Index { path, semantic } => {
            let stats = codeindex::index(&path)?;
            emit(
                json_mode,
                json!({
                    "files_seen": stats.files_seen,
                    "parsed": stats.parsed,
                    "unchanged": stats.unchanged,
                    "removed": stats.removed,
                    "skipped": stats.skipped,
                    "symbols": stats.symbols,
                    "edges": stats.edges,
                    "resolved": stats.resolved,
                    "candidate": stats.candidate,
                    "unresolved": stats.unresolved,
                }),
                || {
                    println!(
                        "{} files seen: {} parsed, {} unchanged, {} removed{} — {} symbols, {} edges ({} resolved, {} candidate, {} unresolved)",
                        stats.files_seen,
                        stats.parsed,
                        stats.unchanged,
                        stats.removed,
                        if stats.skipped > 0 {
                            format!(", {} skipped", stats.skipped)
                        } else {
                            String::new()
                        },
                        stats.symbols,
                        stats.edges,
                        stats.resolved,
                        stats.candidate,
                        stats.unresolved
                    );
                },
            );
            if semantic {
                sync_semantic(&path, json_mode)?;
            }
        }
        Command::Search {
            path,
            query,
            kind,
            path_filter,
            semantic,
        } => {
            if semantic {
                let store = open(&path)?;
                let client = yantrik_client()?;
                let hits = codeindex::yantrik::hybrid_search(&store, &client, &query, 20)?;
                emit(
                    json_mode,
                    json!({
                        "query": query,
                        "mode": "semantic",
                        "hits": hits.iter().map(|h| json!({
                            "score": h.score,
                            "kind": h.kind,
                            "name": h.name,
                            "file": h.file,
                            "line": h.line,
                            "signature": h.signature,
                            "source": h.source,
                        })).collect::<Vec<_>>(),
                    }),
                    || {
                        println!("{} semantic hit(s) for \"{}\":", hits.len(), query);
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
                            println!(
                                "  {} {:>7} {} {}:{}  {}",
                                score, h.kind, h.name, h.file, h.line, signature
                            );
                        }
                    },
                );
                return Ok(());
            }
            let store = open(&path)?;
            let hits = store.search_symbols(&query, kind.as_deref(), path_filter.as_deref())?;
            emit(
                json_mode,
                json!({
                    "query": query,
                    "mode": "lexical",
                    "matches": hits.iter().map(symbol_json).collect::<Vec<_>>(),
                }),
                || {
                    println!("{} matches for \"{}\":", hits.len(), query);
                    for s in &hits {
                        println!("  {:>7} {} {}:{}", s.kind, s.name, s.file, s.line);
                    }
                },
            );
        }
        Command::Definition { path, name } => {
            let store = open(&path)?;
            let defs = store.definitions(&name)?;
            emit(
                json_mode,
                json!({
                    "name": name,
                    "definitions": defs.iter().map(symbol_json).collect::<Vec<_>>(),
                }),
                || {
                    match defs.len() {
                        0 => println!("no symbol named \"{name}\""),
                        1 => println!("1 definition:"),
                        n => println!("{n} definitions (same name in several places):"),
                    }
                    for s in &defs {
                        println!("  {:>7} {} {}:{}", s.kind, s.name, s.file, s.line);
                    }
                },
            );
        }
        Command::Symbol {
            path,
            name,
            file,
            max_lines,
        } => {
            let store = open(&path)?;
            match query::symbol_source(&store, &path, &name, file.as_deref(), max_lines)? {
                Some(src) => emit(
                    json_mode,
                    json!({
                        "kind": src.kind,
                        "name": src.name,
                        "file": src.file,
                        "start": src.start,
                        "end": src.end,
                        "text": src.text,
                    }),
                    || {
                        println!(
                            "{} {} at {}:{}-{}",
                            src.kind, src.name, src.file, src.start, src.end
                        );
                        println!("{}", src.text);
                    },
                ),
                None => emit(
                    json_mode,
                    json!({ "error": format!("no symbol named \"{name}\"") }),
                    || println!("no symbol named \"{name}\""),
                ),
            }
        }
        Command::Callers { path, name } => {
            let store = open(&path)?;
            let refs = store.callers(&name)?;
            emit(
                json_mode,
                json!({
                    "name": name,
                    "callers": refs.iter().map(|r| json!({
                        "file": r.file,
                        "line": r.line,
                        "tier": r.tier,
                        "target": r.target.as_ref().map(|(f, l)| json!({"file": f, "line": l})),
                        "reason": r.reason,
                    })).collect::<Vec<_>>(),
                }),
                || {
                    println!("{} call site(s) of \"{name}\":", refs.len());
                    for r in &refs {
                        match (&r.target, &r.reason) {
                            (Some((f, l)), _) => {
                                println!("  {}:{} ({}) → {}:{}", r.file, r.line, r.tier, f, l)
                            }
                            (None, Some(reason)) => {
                                println!("  {}:{} ({}, {})", r.file, r.line, r.tier, reason)
                            }
                            _ => println!("  {}:{} ({})", r.file, r.line, r.tier),
                        }
                    }
                },
            );
        }
        Command::CallTree { path, name, depth } => {
            let store = open(&path)?;
            let tree = query::call_tree(&store, &name, depth)?;
            emit(
                json_mode,
                json!({
                    "name": name,
                    "roots": tree.iter().map(node_json).collect::<Vec<_>>(),
                }),
                || {
                    if tree.is_empty() {
                        println!("no symbol named \"{name}\"");
                    } else {
                        println!("callers of \"{name}\" (resolved edges only):");
                        print_tree(&tree, 1);
                    }
                },
            );
        }
        Command::BlastRadius { path, name } => {
            let store = open(&path)?;
            let affected = query::blast_radius(&store, &name)?;
            emit(
                json_mode,
                json!({
                    "name": name,
                    "affected": affected.iter().map(|a| json!({
                        "file": a.file, "line": a.line, "depth": a.depth,
                    })).collect::<Vec<_>>(),
                }),
                || {
                    let files: std::collections::HashSet<&str> =
                        affected.iter().map(|a| a.file.as_str()).collect();
                    println!(
                        "{} transitive call site(s) of \"{name}\" across {} file(s):",
                        affected.len(),
                        files.len()
                    );
                    for a in &affected {
                        println!("  d{} {}:{}", a.depth, a.file, a.line);
                    }
                },
            );
        }
        Command::References { path, name, limit } => {
            let store = open(&path)?;
            let refs = query::references(&store, &path, &name, limit)?;
            emit(
                json_mode,
                json!({
                    "name": name,
                    "references": refs.iter().map(|r| json!({
                        "file": r.file,
                        "line": r.line,
                        "kind": r.kind,
                        "target": r.target.as_ref().map(|(f, l)| json!({"file": f, "line": l})),
                    })).collect::<Vec<_>>(),
                }),
                || {
                    println!(
                        "{} reference(s) to \"{name}\" (lexical occurrences in indexed files):",
                        refs.len()
                    );
                    for r in &refs {
                        match &r.target {
                            Some((f, l)) => {
                                println!("  {}:{}  {} → {}:{}", r.file, r.line, r.kind, f, l)
                            }
                            None => println!("  {}:{}  {}", r.file, r.line, r.kind),
                        }
                    }
                },
            );
        }
        Command::Impact { path, since } => {
            let store = open(&path)?;
            let impact = query::impact(&store, &path, &since)?;
            emit(
                json_mode,
                json!({
                    "since": since,
                    "changed": impact.changed.iter().map(|c| json!({
                        "file": c.file, "line": c.line, "kind": c.kind, "name": c.name,
                    })).collect::<Vec<_>>(),
                    "affected": impact.affected.iter().map(|a| json!({
                        "file": a.file, "line": a.line, "depth": a.depth,
                    })).collect::<Vec<_>>(),
                }),
                || {
                    println!("changed symbols since {since} ({}):", impact.changed.len());
                    for c in &impact.changed {
                        println!("  {:>7} {} {}:{}", c.kind, c.name, c.file, c.line);
                    }
                    println!(
                        "\ntransitively affected call sites ({}):",
                        impact.affected.len()
                    );
                    for a in &impact.affected {
                        println!("  d{} {}:{}", a.depth, a.file, a.line);
                    }
                },
            );
        }
        Command::Stats { path } => {
            let store = open(&path)?;
            let (resolved, candidate, unresolved) = store.edge_tier_counts()?;
            let mut kinds = serde_json::Map::new();
            for (kind, count) in store.symbol_kind_counts()? {
                kinds.insert(kind, json!(count));
            }
            let mut reasons = serde_json::Map::new();
            for (reason, count) in store.edge_reason_counts()? {
                reasons.insert(reason, json!(count));
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "files": store.file_count()?,
                    "symbols": store.symbol_count()?,
                    "symbols_by_kind": kinds,
                    "edges": resolved + candidate + unresolved,
                    "resolved": resolved,
                    "candidate": candidate,
                    "unresolved": unresolved,
                    "edge_reasons": reasons,
                    "schema_version": codeindex::store::SCHEMA_VERSION,
                    "extractor_version": codeindex::store::EXTRACTOR_VERSION,
                }))?
            );
        }
        Command::Serve {
            path,
            no_refresh,
            semantic,
        } => {
            codeindex::mcp::run_stdio(path, !no_refresh, semantic)?;
        }
    }
    Ok(())
}

fn yantrik_client() -> Result<codeindex::yantrik::Client> {
    let cfg = codeindex::yantrik::Config::from_env().ok_or_else(|| {
        anyhow::anyhow!(
            "semantic layer not configured: set CODEINDEX_YANTRIK_URL (and CODEINDEX_YANTRIK_TOKEN)"
        )
    })?;
    Ok(codeindex::yantrik::Client::new(cfg))
}

fn sync_semantic(path: &Path, json_mode: bool) -> Result<()> {
    let store = open(path)?;
    let client = yantrik_client()?;
    let stats = codeindex::yantrik::sync(&store, &client)?;
    emit(
        json_mode,
        json!({
            "pushed": stats.pushed,
            "existing": stats.existing,
            "forgotten": stats.forgotten,
            "namespace": client.config().namespace,
        }),
        || {
            println!(
                "semantic sync: {} pushed, {} already present, {} forgotten (namespace {})",
                stats.pushed,
                stats.existing,
                stats.forgotten,
                client.config().namespace
            )
        },
    );
    Ok(())
}

fn print_tree(nodes: &[CallNode], indent: usize) {
    for node in nodes {
        println!("{}{}:{}", "  ".repeat(indent), node.file, node.line);
        print_tree(&node.children, indent + 1);
    }
}
