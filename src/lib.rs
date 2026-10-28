//! codeindex library: parse Rust sources into a symbol inventory.
//! M0 scope: walk + parse + extract. Storage (SQLite) lands in M1,
//! name resolution (stack-graphs) in M3.

use anyhow::Context;
use streaming_iterator::StreamingIterator;
use std::path::{Path, PathBuf};

/// One extracted symbol.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    /// fn | struct | enum | trait | impl
    pub kind: &'static str,
    pub file: PathBuf,
    /// 1-based line of the item's start.
    pub line: usize,
}

/// Aggregate result of one scan.
#[derive(Debug, Default)]
pub struct Inventory {
    pub files: usize,
    pub symbols: Vec<Symbol>,
}

impl Inventory {
    pub fn total(&self) -> usize {
        self.symbols.len()
    }
    pub fn count(&self, kind: &str) -> usize {
        self.symbols.iter().filter(|s| s.kind == kind).count()
    }
    pub fn fn_count(&self) -> usize {
        self.count("fn")
    }
    pub fn struct_count(&self) -> usize {
        self.count("struct")
    }
    pub fn enum_count(&self) -> usize {
        self.count("enum")
    }
    pub fn trait_count(&self) -> usize {
        self.count("trait")
    }
    pub fn impl_count(&self) -> usize {
        self.count("impl")
    }
}

/// tree-sitter query: every item kind we index, name captured as `name`.
/// One pattern per impl — a trait arm too would match `impl Trait for
/// Type` twice (the query language has no comments, hence the docs
/// live here).
const ITEMS_QUERY: &str = r#"
(function_item name: (identifier) @name)
(struct_item    name: (type_identifier) @name)
(enum_item      name: (type_identifier) @name)
(trait_item     name: (type_identifier) @name)
(impl_item      type: (type_identifier) @name)
"#;

/// Walk `root` for *.rs files (skipping target/, .git/, .codeindex/),
/// parse each with tree-sitter, collect symbols.
pub fn scan(root: &Path) -> anyhow::Result<Inventory> {
    let mut language = tree_sitter::Language::new(tree_sitter_rust::LANGUAGE.into());
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&mut language)
        .context("loading Rust grammar")?;
    let query = tree_sitter::Query::new(&language, ITEMS_QUERY).context("items query")?;

    let mut inv = Inventory::default();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(e.depth() > 0
                && (name == "target" || name == ".git" || name == ".codeindex"))
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "rs"))
    {
        let file = entry.path();
        let src = std::fs::read(file)
            .with_context(|| format!("reading {}", file.display()))?;
        let tree = parser
            .parse(&src, None)
            .context(format!("parsing {}", file.display()))?;
        inv.files += 1;

        let mut cursor = tree_sitter::QueryCursor::new();
        // tree-sitter 0.24 exposes matches as a StreamingIterator: the C
        // cursor state mutates per step, so `while let Some(m) = it.next()`
        // is the sanctioned shape (Item = &mut QueryMatch).
        let mut matches = cursor.matches(&query, tree.root_node(), src.as_slice());
        while let Some(m) = matches.next() {
            let Some(name_node) = m.captures.first() else {
                continue;
            };
            let name = name_node
                .node
                .utf8_text(&src)
                .unwrap_or_default()
                .to_string();
            let kind = match m.pattern_index {
                0 => "fn",
                1 => "struct",
                2 => "enum",
                3 => "trait",
                _ => "impl",
            };
            inv.symbols.push(Symbol {
                name,
                kind,
                file: file.to_path_buf(),
                line: name_node.node.start_position().row + 1,
            });
        }
    }
    Ok(inv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_extracts_symbols_from_a_sample_tree() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(
            &file,
            "pub fn alpha() {}\nstruct Beta;\nenum Gamma { A }\ntrait Delta {}\nimpl Delta for Beta {}\n",
        )
        .unwrap();
        let inv = scan(dir.path()).unwrap();
        assert_eq!(inv.files, 1);
        assert_eq!(inv.total(), 5);
        assert_eq!(inv.fn_count(), 1);
        assert_eq!(inv.struct_count(), 1);
        assert_eq!(inv.enum_count(), 1);
        assert_eq!(inv.trait_count(), 1);
        assert_eq!(inv.impl_count(), 1);
        assert_eq!(inv.symbols[0].name, "alpha");
        assert_eq!(inv.symbols[0].line, 1);
    }

    #[test]
    fn scan_skips_target_and_non_rs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target").join("gen.rs"), "fn nope() {}").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "fn also_nope() {}").unwrap();
        std::fs::write(dir.path().join("real.rs"), "fn yes() {}").unwrap();
        let inv = scan(dir.path()).unwrap();
        assert_eq!(inv.files, 1);
        assert_eq!(inv.total(), 1);
        assert_eq!(inv.symbols[0].name, "yes");
    }
}
