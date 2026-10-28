//! codeindex — local code index for AI agents.
//! M0: walk a Rust tree, lex every file with the in-house lexer,
//! report the symbol inventory.
//! M1: incremental SQLite index at <path>/.codeindex/index.db.
//! M2: queries over the store — search, definition, callers.
//! M3: in-house name resolver — resolved edges, call_tree, blast_radius.

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
    },
    /// Find symbols whose name contains the query (case-insensitive).
    Search {
        /// Indexed repository root.
        path: PathBuf,
        /// Substring of a symbol name.
        query: String,
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
        Command::Index { path } => {
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
        }
        Command::Search { path, query } => {
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
    }
    Ok(())
}

fn print_tree(nodes: &[CallNode], indent: usize) {
    for node in nodes {
        println!("{}{}:{}", "  ".repeat(indent), node.file, node.line);
        print_tree(&node.children, indent + 1);
    }
}
