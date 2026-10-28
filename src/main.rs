//! codeindex — local code index for AI agents.
//! M0: walk a Rust tree, parse every file with tree-sitter, report symbols.

use anyhow::Context;
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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { path } => {
            let inv = codeindex::scan(&path)?;
            println!(
                "{} files parsed, {} symbols: {} fn, {} struct, {} enum, {} trait, {} impl",
                inv.files, inv.total(), inv.fn_count(), inv.struct_count(), inv.enum_count(),
                inv.trait_count(), inv.impl_count()
            );
            for sym in inv.symbols.iter().take(20) {
                println!(
                    "  {:>8} {}",
                    sym.kind,
                    format!("{}:{}", sym.file.display(), sym.line)
                );
                println!("           {}", sym.name);
            }
            if inv.symbols.len() > 20 {
                println!("  … and {} more", inv.symbols.len() - 20);
            }
        }
    }
    Ok(())
}
