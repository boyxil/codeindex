//! codeindex — local code index for AI agents.
//! M0: walk a Rust tree, lex every file with the in-house lexer,
//! report the symbol inventory.
//! M1: incremental SQLite index at <path>/.codeindex/index.db.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
                "{} files seen: {} parsed, {} unchanged, {} removed — {} symbols total",
                stats.files_seen,
                stats.parsed,
                stats.unchanged,
                stats.removed,
                stats.symbols
            );
        }
    }
    Ok(())
}
