//! codeindex library: parse Rust sources into a symbol inventory.
//!
//! Entirely self-contained: a hand-written lexer (see [`lexer`]) feeds a
//! token-level item scanner. No tree-sitter, no external grammars — the
//! whole pipeline is ours to extend. The cost is honest imprecision:
//! name resolution is heuristic until the M3 resolver lands, which is
//! why edges (M2+) will carry resolved/candidate tiers rather than
//! pretending to certainty.
//!
//! M0 scope: walk + lex + extract item declarations.
//! M1 adds SQLite storage and incremental rescans;
//! M2 adds queries and lexical call-site candidates;
//! M3 adds the in-house name resolver (use-tree + module tree).

pub mod lexer;
pub mod store;

use anyhow::Context;
use lexer::{lex, text, Tok};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One extracted symbol.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    /// fn | struct | enum | trait | impl | mod | union
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
    pub fn mod_count(&self) -> usize {
        self.count("mod")
    }
    pub fn union_count(&self) -> usize {
        self.count("union")
    }
}

/// Item keywords we index, with the extraction rule per keyword.
const ITEM_KEYWORDS: &[(&str, ItemKind)] = &[
    ("fn", ItemKind::Named),
    ("struct", ItemKind::Named),
    ("enum", ItemKind::Named),
    ("trait", ItemKind::Named),
    ("mod", ItemKind::Named),
    ("union", ItemKind::Named),
    ("impl", ItemKind::Impl),
];

#[derive(Clone, Copy)]
enum ItemKind {
    /// `keyword Name …` — the name is the next Ident.
    Named,
    /// `impl … { }` — the name is the last Ident before the brace
    /// (the implemented type; the trait, if any, precedes `for`).
    Impl,
}

/// Walk `root` for *.rs files (skipping target/, .git/, .codeindex/),
/// lex each file, extract item declarations.
pub fn scan(root: &Path) -> anyhow::Result<Inventory> {
    let mut inv = Inventory::default();
    for file in rust_files(root) {
        let src = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        inv.files += 1;
        extract_items(&lex(&src), &src, &file, &mut inv.symbols);
    }
    Ok(inv)
}

/// Counters of one `index` run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub files_seen: usize,
    /// New or changed since the last run — re-lexed and re-stored.
    pub parsed: usize,
    /// Content hash matched the stored one — parse skipped.
    pub unchanged: usize,
    /// Indexed before, gone from disk now.
    pub removed: usize,
    /// Total symbol rows in the store after the run.
    pub symbols: usize,
}

/// Build or update the SQLite index at `<root>/.codeindex/index.db`.
///
/// Incrementality: a file is re-lexed only when its SHA-256 differs from
/// the stored hash. mtime is recorded for humans but never trusted for
/// decisions — a copy that preserves mtime while changing bytes must
/// still be caught. Files present in the store but absent on disk are
/// dropped. Paths are stored relative to the root so the index survives
/// moving the checkout.
pub fn index(root: &Path) -> anyhow::Result<IndexStats> {
    let db = root.join(".codeindex").join("index.db");
    let store = store::Store::open(&db)?;
    let files = rust_files(root);
    let mut stats = IndexStats {
        files_seen: files.len(),
        ..Default::default()
    };
    let known = store.file_hashes()?;
    let mut seen: HashSet<String> = HashSet::with_capacity(files.len());

    for file in files {
        let src = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let hash = sha256_hex(src.as_bytes());
        let rel = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .to_string_lossy()
            .to_string();
        seen.insert(rel.clone());
        if known.get(&rel).is_some_and(|h| h == &hash) {
            stats.unchanged += 1;
            continue;
        }
        let mtime = std::fs::metadata(&file)?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut symbols = Vec::new();
        extract_items(&lex(&src), &src, &file, &mut symbols);
        store.replace_file(&rel, &hash, mtime, &symbols)?;
        stats.parsed += 1;
    }

    for path in known.keys() {
        if !seen.contains(path) {
            store.remove_file(path)?;
            stats.removed += 1;
        }
    }
    stats.symbols = store.symbol_count()?;
    Ok(stats)
}

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Collect *.rs files without following skipped directories.
fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if name != "target" && name != ".git" && name != ".codeindex" {
                    walk(&path, out);
                }
            } else if path.extension().is_some_and(|x| x == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Token-level item extraction. A keyword opens an item when it is not
/// in method-call position (preceded by `.`); a `fn` inside a type like
/// `Box<fn(u8) -> u8>` is skipped because no Ident follows the keyword.
fn extract_items(tokens: &[lexer::Token], src: &str, file: &Path, out: &mut Vec<Symbol>) {
    let mut i = 0usize;
    while i < tokens.len() {
        let tok = &tokens[i];
        if tok.kind != Tok::Ident {
            i += 1;
            continue;
        }
        let word = text(src, tok);
        let Some((kw, kind)) = ITEM_KEYWORDS.iter().find(|(kw, _)| *kw == word) else {
            i += 1;
            continue;
        };
        // Method-call position guard: `.fn()` cannot occur (reserved),
        // but a path segment `Foo::fn` cannot either — the real hazard is
        // a field access like `x.union`, so skip after `.`.
        if i > 0 && tokens[i - 1].kind == Tok::Punct
            && text(src, &tokens[i - 1]) == "."
        {
            i += 1;
            continue;
        }

        match kind {
            ItemKind::Named => {
                let Some(next) = tokens.get(i + 1) else {
                    i += 1;
                    continue;
                };
                if next.kind != Tok::Ident {
                    i += 1; // e.g. `fn(` in a fn-pointer type — not an item
                    continue;
                }
                out.push(Symbol {
                    name: text(src, next).to_string(),
                    kind: kw,
                    file: file.to_path_buf(),
                    line: tok.line,
                });
            }
            ItemKind::Impl => {
                // Scan to the opening brace at angle/paren depth 0; the
                // implemented type is the last Ident before it.
                let mut depth = 0i32;
                let mut last_ident: Option<&lexer::Token> = None;
                let mut j = i + 1;
                let mut closed = false;
                while j < tokens.len() {
                    let t = &tokens[j];
                    match t.kind {
                        Tok::Punct => match text(src, t) {
                            "<" => depth += 1,
                            ">" => depth -= 1,
                            "(" => depth += 1,
                            ")" => depth -= 1,
                            "{" if depth <= 0 => {
                                closed = true;
                                break;
                            }
                            _ => {}
                        },
                        Tok::Ident => last_ident = Some(t),
                        _ => {}
                    }
                    j += 1;
                }
                if closed {
                    if let Some(t) = last_ident {
                        out.push(Symbol {
                            name: text(src, t).to_string(),
                            kind: "impl",
                            file: file.to_path_buf(),
                            line: tok.line,
                        });
                    }
                    i = j; // resume inside the impl body
                    continue;
                }
            }
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbols(src: &str) -> Vec<(String, &'static str)> {
        let toks = lex(src);
        let mut out = Vec::new();
        extract_items(&toks, src, Path::new("t.rs"), &mut out);
        out.into_iter().map(|s| (s.name, s.kind)).collect()
    }

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
        // `impl Trait for Type` names the TYPE.
        let imp = inv.symbols.iter().find(|s| s.kind == "impl").unwrap();
        assert_eq!(imp.name, "Beta");
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

    #[test]
    fn fn_pointers_in_types_are_not_items() {
        let syms = symbols("let f: Box<fn(u8) -> u8> = None;\nfn real() {}\n");
        assert_eq!(syms, vec![("real".into(), "fn")]);
    }

    #[test]
    fn generics_and_where_clauses_keep_the_name() {
        let syms = symbols(
            "fn generic<T: Clone>(t: T) -> T where T: Send { t }\nstruct Pair<A, B> { a: A, b: B }\n",
        );
        assert_eq!(
            syms,
            vec![("generic".into(), "fn"), ("Pair".into(), "struct")]
        );
    }

    #[test]
    fn mod_union_impl_types_are_indexed() {
        let syms = symbols(
            "mod inner;\nunion U { a: u32 }\nimpl std::fmt::Display for Wrapper {}\nimpl Config {}\n",
        );
        assert!(syms.contains(&("inner".into(), "mod")));
        assert!(syms.contains(&("U".into(), "union")));
        // `impl std::fmt::Display for Wrapper` — the type is Wrapper; the
        // impl body scan must survive the `::` path segments.
        assert!(syms.contains(&("Wrapper".into(), "impl")));
        assert!(syms.contains(&("Config".into(), "impl")));
    }

    #[test]
    fn keyword_after_dot_is_a_field_not_an_item() {
        let syms = symbols("let x = parser.union;\nfn ok() {}\n");
        assert_eq!(syms, vec![("ok".into(), "fn")]);
    }

    #[test]
    fn items_inside_fn_bodies_are_indexed_too() {
        let syms = symbols("fn outer() {\n    fn helper() {}\n    struct Inner;\n}\n");
        let names: Vec<&str> = syms.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["outer", "helper", "Inner"]);
    }

    fn write(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn index_reparses_only_changed_files() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rs"), "fn one() {}\n");
        write(&dir.path().join("b.rs"), "fn two() {}\n");

        let s = index(dir.path()).unwrap();
        assert_eq!(s, IndexStats { files_seen: 2, parsed: 2, unchanged: 0, removed: 0, symbols: 2 });

        // Nothing changed: the second run must skip parsing entirely.
        let s = index(dir.path()).unwrap();
        assert_eq!(s, IndexStats { files_seen: 2, parsed: 0, unchanged: 2, removed: 0, symbols: 2 });

        // One file edited: only it is re-parsed; its new symbol lands.
        write(&dir.path().join("b.rs"), "fn two() {}\nfn three() {}\n");
        let s = index(dir.path()).unwrap();
        assert_eq!(s, IndexStats { files_seen: 2, parsed: 1, unchanged: 1, removed: 0, symbols: 3 });

        let store = store::Store::open(&dir.path().join(".codeindex/index.db")).unwrap();
        let all = store.symbols().unwrap();
        let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["one", "two", "three"]);
    }

    #[test]
    fn index_drops_files_deleted_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone.rs");
        write(&gone, "fn vanished() {}\n");
        write(&dir.path().join("kept.rs"), "fn kept() {}\n");
        index(dir.path()).unwrap();

        std::fs::remove_file(&gone).unwrap();
        let s = index(dir.path()).unwrap();
        assert_eq!(s, IndexStats { files_seen: 1, parsed: 0, unchanged: 1, removed: 1, symbols: 1 });

        let store = store::Store::open(&dir.path().join(".codeindex/index.db")).unwrap();
        let all = store.symbols().unwrap();
        let files: Vec<&str> = all.iter().map(|s| s.file.as_str()).collect();
        assert_eq!(files, vec!["kept.rs"]);
    }

    #[test]
    fn index_stores_root_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        write(&dir.path().join("src/lib.rs"), "fn inside() {}\n");
        index(dir.path()).unwrap();

        let store = store::Store::open(&dir.path().join(".codeindex/index.db")).unwrap();
        let all = store.symbols().unwrap();
        let sym = &all[0];
        assert_eq!(sym.file, "src/lib.rs");
        assert_eq!(sym.line, 1);
    }
}
