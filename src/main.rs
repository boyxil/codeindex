//! codeindex — local code index for AI agents.
//! M0: walk a Rust tree, lex every file with the in-house lexer,
//! report the symbol inventory.
//! M1: incremental SQLite index at <path>/.codeindex/index.db.
//! M2: queries over the store — search, definition, callers.
//! M3: in-house name resolver — resolved edges, call_tree, blast_radius.
//! M4: MCP server over stdio (`serve`) with seven tools.

use anyhow::Result;
use clap::{Parser, Subcommand};
use codeindex::query::CallNode;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "codeindex", version, about = "Local code index for AI agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { path } => {
            let inv = codeindex::scan(&path)?;
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
                println!(
                    "  {:>7} {}:{}",
                    sym.kind,
                    sym.file.display(),
                    sym.line
                );
                println!("          {}", sym.name);
            }
            if inv.symbols.len() > 20 {
                println!("  … and {} more", inv.symbols.len() - 20);
            }
        }
        Command::Index { path, semantic } => {
            let stats = codeindex::index(&path)?;
            println!(
                "{} files seen: {} parsed, {} unchanged, {} removed — {} symbols, {} edges ({} resolved, {} candidate, {} unresolved)",
                stats.files_seen,
                stats.parsed,
                stats.unchanged,
                stats.removed,
                stats.symbols,
                stats.edges,
                stats.resolved,
                stats.candidate,
                stats.unresolved
            );
            if semantic {
                sync_semantic(&path)?;
            }
        }
        Command::Search {
            path,
            query,
            semantic,
        } => {
            if semantic {
                let client = yantrik_client()?;
                let hits = client.recall(&query, 20)?;
                println!("{} semantic hit(s) for \"{}\":", hits.len(), query);
                for h in &hits {
                    println!(
                        "  {:.2}  {:>7} {} {}:{}  {}",
                        h.score, h.kind, h.name, h.file, h.line, h.signature
                    );
                }
                return Ok(());
            }
            let store = open(&path)?;
            let hits = store.search_symbols(&query)?;
            println!("{} matches for \"{}\":", hits.len(), query);
            for s in &hits {
                println!("  {:>7} {} {}:{}", s.kind, s.name, s.file, s.line);
            }
        }
        Command::Definition { path, name } => {
            let store = open(&path)?;
            let defs = store.definitions(&name)?;
            match defs.len() {
                0 => println!("no symbol named \"{name}\""),
                1 => println!("1 definition:"),
                n => println!("{n} definitions (same name in several places):"),
            }
            for s in &defs {
                println!("  {:>7} {} {}:{}", s.kind, s.name, s.file, s.line);
            }
        }
        Command::Callers { path, name } => {
            let store = open(&path)?;
            let refs = store.callers(&name)?;
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
        }
        Command::CallTree { path, name, depth } => {
            let store = open(&path)?;
            let tree = codeindex::query::call_tree(&store, &name, depth)?;
            if tree.is_empty() {
                println!("no symbol named \"{name}\"");
            } else {
                println!("callers of \"{name}\" (resolved edges only):");
                print_tree(&tree, 1);
            }
        }
        Command::BlastRadius { path, name } => {
            let store = open(&path)?;
            let affected = codeindex::query::blast_radius(&store, &name)?;
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
        }
        Command::References { path, name, limit } => {
            let store = open(&path)?;
            let refs = codeindex::query::references(&store, &path, &name, limit)?;
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

fn sync_semantic(path: &Path) -> Result<()> {
    let store = open(path)?;
    let client = yantrik_client()?;
    let stats = codeindex::yantrik::sync(&store, &client)?;
    println!(
        "semantic sync: {} pushed, {} already present, {} forgotten (namespace {})",
        stats.pushed,
        stats.existing,
        stats.forgotten,
        client.config().namespace
    );
    Ok(())
}

fn print_tree(nodes: &[CallNode], indent: usize) {
    for node in nodes {
        println!("{}{}:{}", "  ".repeat(indent), node.file, node.line);
        print_tree(&node.children, indent + 1);
    }
}
