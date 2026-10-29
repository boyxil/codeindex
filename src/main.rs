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
    /// What this symbol calls, transitively (resolved edges only).
    Callees {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact symbol name.
        name: String,
        /// Maximum callee depth.
        #[arg(long, default_value_t = 3)]
        depth: usize,
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
    /// Type/trait hierarchy: implemented traits, implementors, supertraits.
    Hierarchy {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact type or trait name.
        name: String,
    },
    /// Call chain between two symbols (resolved edges only).
    Trace {
        /// Indexed repository root.
        path: PathBuf,
        /// Starting symbol name.
        from: String,
        /// Target symbol name.
        to: String,
    },
    /// External crates a symbol leans on: calls into them and imports.
    External {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact symbol name.
        name: String,
    },
    /// A symbol's outline: declaration plus the items inside its body.
    Skim {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact symbol name.
        name: String,
        /// Restrict to this file (exact or suffix).
        #[arg(long)]
        file: Option<String>,
        /// Maximum nested items.
        #[arg(long, default_value_t = 40)]
        max_items: usize,
    },
    /// Parameters of a symbol and the raw arguments passed at every
    /// call site (lexical, no data-flow analysis).
    Provenance {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact symbol name.
        name: String,
        /// Maximum call sites.
        #[arg(long, default_value_t = 20)]
        max_sites: usize,
    },
    /// Everything about one symbol in one answer.
    Profile {
        /// Indexed repository root.
        path: PathBuf,
        /// Exact symbol name.
        name: String,
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

fn callee_json(n: &query::CalleeNode) -> Value {
    json!({
        "file": n.file,
        "line": n.line,
        "call_line": n.call_line,
        "children": n.children.iter().map(callee_json).collect::<Vec<_>>(),
    })
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
                        match (&r.target, &r.external_crate, &r.reason) {
                            (Some((f, l)), _, _) => {
                                println!("  {}:{} ({}) → {}:{}", r.file, r.line, r.tier, f, l)
                            }
                            (None, Some(crate_name), _) => {
                                println!("  {}:{} (external: {})", r.file, r.line, crate_name)
                            }
                            (None, None, Some(reason)) => {
                                println!("  {}:{} ({}, {})", r.file, r.line, r.tier, reason)
                            }
                            _ => println!("  {}:{} ({})", r.file, r.line, r.tier),
                        }
                    }
                },
            );
        }
        Command::Callees { path, name, depth } => {
            let store = open(&path)?;
            let tree = query::callee_tree(&store, &name, depth)?;
            emit(
                json_mode,
                json!({
                    "name": name,
                    "roots": tree.iter().map(callee_json).collect::<Vec<_>>(),
                }),
                || {
                    if tree.is_empty() {
                        println!("no symbol named \"{name}\"");
                    } else {
                        println!("callees of \"{name}\" (resolved edges only):");
                        print_callees(&tree, 1);
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
        Command::Hierarchy { path, name } => {
            let store = open(&path)?;
            let h = query::hierarchy(&store, &name)?;
            emit(
                json_mode,
                json!({
                    "name": h.name,
                    "kinds": h.kinds,
                    "traits": h.traits,
                    "implementors": h.implementors,
                    "supertraits": h.supertraits,
                }),
                || {
                    println!(
                        "{}: {}",
                        h.name,
                        if h.kinds.is_empty() {
                            "unknown".to_string()
                        } else {
                            h.kinds.join(", ")
                        }
                    );
                    if !h.traits.is_empty() {
                        println!("  implements: {}", h.traits.join(", "));
                    }
                    if !h.implementors.is_empty() {
                        println!("  implemented by: {}", h.implementors.join(", "));
                    }
                    if !h.supertraits.is_empty() {
                        println!("  supertraits: {}", h.supertraits.join(", "));
                    }
                },
            );
        }
        Command::Trace { path, from, to } => {
            let store = open(&path)?;
            let steps = query::trace_path(&store, &from, &to)?;
            emit(
                json_mode,
                json!({
                    "from": from,
                    "to": to,
                    "path": steps.as_ref().map(|steps| steps.iter().map(|s| json!({
                        "file": s.file,
                        "line": s.line,
                        "via": s.via.as_ref().map(|(f, l)| json!({"file": f, "line": l})),
                    })).collect::<Vec<_>>()),
                }),
                || match &steps {
                    None => println!("no resolved call chain from \"{from}\" to \"{to}\"",),
                    Some(steps) => {
                        println!(
                            "call chain \"{from}\" → \"{to}\" ({} hop(s)):",
                            steps.len().saturating_sub(1)
                        );
                        for (i, step) in steps.iter().enumerate() {
                            match &step.via {
                                Some((f, l)) => println!(
                                    "  → {}:{}  (called at {}:{})",
                                    step.file, step.line, f, l
                                ),
                                None => println!("  {}:{}", step.file, step.line),
                            }
                            let _ = i;
                        }
                    }
                },
            );
        }
        Command::External { path, name } => {
            let store = open(&path)?;
            let e = query::external_calls(&store, &name)?;
            emit(
                json_mode,
                json!({
                    "name": e.name,
                    "calls": e.calls.iter().map(|(c, n)| json!({"crate": c, "count": n})).collect::<Vec<_>>(),
                    "imports": e.imports.iter().map(|(c, n)| json!({"crate": c, "count": n})).collect::<Vec<_>>(),
                }),
                || {
                    println!("external crates used by {}:", e.name);
                    for (c, n) in &e.calls {
                        println!("  calls   {c} ({n})");
                    }
                    for (c, n) in &e.imports {
                        println!("  imports {c} ({n})");
                    }
                    if e.calls.is_empty() && e.imports.is_empty() {
                        println!("  none");
                    }
                },
            );
        }
        Command::Skim {
            path,
            name,
            file,
            max_items,
        } => {
            let store = open(&path)?;
            match query::skim(&store, &name, file.as_deref(), max_items)? {
                Some(skim) => emit(
                    json_mode,
                    json!({
                        "name": skim.name,
                        "kind": skim.kind,
                        "file": skim.file,
                        "start": skim.start,
                        "end": skim.end,
                        "signature": skim.signature,
                        "items": skim.items.iter().map(|i| json!({
                            "kind": i.kind, "name": i.name, "line": i.line, "signature": i.signature,
                        })).collect::<Vec<_>>(),
                    }),
                    || {
                        println!(
                            "{} {} at {}:{}-{}",
                            skim.kind, skim.name, skim.file, skim.start, skim.end
                        );
                        println!("  {}", skim.signature);
                        for i in &skim.items {
                            println!("  {:>7} {}:{}  {}", i.kind, i.name, i.line, i.signature);
                        }
                        if skim.items.is_empty() {
                            println!("  (no nested items)");
                        }
                    },
                ),
                None => emit(
                    json_mode,
                    json!({ "error": format!("no symbol named \"{name}\"") }),
                    || println!("no symbol named \"{name}\""),
                ),
            }
        }
        Command::Provenance {
            path,
            name,
            max_sites,
        } => {
            let store = open(&path)?;
            match query::argument_provenance(&store, &path, &name, max_sites)? {
                Some(p) => emit(
                    json_mode,
                    json!({
                        "name": p.name,
                        "params": p.params,
                        "call_sites": p.call_sites.iter().map(|c| json!({
                            "file": c.file, "line": c.line, "tier": c.tier, "args": c.args,
                        })).collect::<Vec<_>>(),
                    }),
                    || {
                        println!(
                            "{} (params: {})",
                            p.name,
                            if p.params.is_empty() {
                                "—".to_string()
                            } else {
                                p.params.join(", ")
                            }
                        );
                        for c in &p.call_sites {
                            println!(
                                "  {}:{} ({}) args: [{}]",
                                c.file,
                                c.line,
                                c.tier,
                                c.args.join(", ")
                            );
                        }
                        if p.call_sites.is_empty() {
                            println!("  no resolved call sites");
                        }
                    },
                ),
                None => emit(
                    json_mode,
                    json!({ "error": format!("no fn named \"{name}\"") }),
                    || println!("no fn named \"{name}\""),
                ),
            }
        }
        Command::Profile { path, name } => {
            let store = open(&path)?;
            let p = query::profile(&store, &path, &name)?;
            emit(
                json_mode,
                json!({
                    "name": p.name,
                    "definitions": p.definitions.iter().map(symbol_json).collect::<Vec<_>>(),
                    "callers": { "resolved": p.callers.0, "candidate": p.callers.1, "unresolved": p.callers.2 },
                    "callees": p.callees.iter().map(|(f, l)| json!({"file": f, "line": l})).collect::<Vec<_>>(),
                    "references": { "total": p.references.0, "by_kind": p.references.1 },
                    "hierarchy": { "kinds": p.hierarchy.kinds, "traits": p.hierarchy.traits,
                                   "implementors": p.hierarchy.implementors, "supertraits": p.hierarchy.supertraits },
                }),
                || {
                    println!("{} ({} definition(s))", p.name, p.definitions.len());
                    for d in &p.definitions {
                        println!("  {:>7} {}:{}", d.kind, d.file, d.line);
                    }
                    println!(
                        "  callers: {} resolved, {} candidate, {} unresolved",
                        p.callers.0, p.callers.1, p.callers.2
                    );
                    if !p.callees.is_empty() {
                        let list: Vec<String> =
                            p.callees.iter().map(|(f, l)| format!("{f}:{l}")).collect();
                        println!("  calls: {}", list.join(", "));
                    }
                    let kinds: Vec<String> = p
                        .references
                        .1
                        .iter()
                        .map(|(k, v)| format!("{k} {v}"))
                        .collect();
                    println!("  references: {} ({})", p.references.0, kinds.join(", "));
                    if !p.hierarchy.traits.is_empty() {
                        println!("  implements: {}", p.hierarchy.traits.join(", "));
                    }
                    if !p.hierarchy.implementors.is_empty() {
                        println!("  implemented by: {}", p.hierarchy.implementors.join(", "));
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
                    "embeddings": store.embedding_count()?,
                    "external": store.external_edge_count()?,
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

fn print_callees(nodes: &[query::CalleeNode], indent: usize) {
    for node in nodes {
        println!(
            "{}{}:{} (called at parent:{})",
            "  ".repeat(indent),
            node.file,
            node.line,
            node.call_line
        );
        print_callees(&node.children, indent + 1);
    }
}

fn print_tree(nodes: &[CallNode], indent: usize) {
    for node in nodes {
        println!("{}{}:{}", "  ".repeat(indent), node.file, node.line);
        print_tree(&node.children, indent + 1);
    }
}
