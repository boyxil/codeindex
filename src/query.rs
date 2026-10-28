//! Call-graph queries over resolved edges: `call_tree` (who calls this,
//! transitively upward) and `blast_radius` (what breaks if this
//! changes). Only `resolved` edges are followed — candidates and
//! unresolved edges are named as such in `callers`, never silently woven
//! into the graph.
//!
//! Climbing upward from a call site requires the enclosing function:
//! a call in `b.rs:12` belongs to the fn whose body spans line 12, and
//! that fn's definition is the next graph node. Without it the chain
//! would stop at the first hop.

use crate::store::Store;
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};

/// One call site (or, at the root, one definition).
#[derive(Debug, Clone, PartialEq)]
pub struct CallNode {
    pub file: String,
    pub line: i64,
    pub children: Vec<CallNode>,
}

/// One transitively affected call site, with its distance from the
/// symbol being changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Affected {
    pub file: String,
    pub line: i64,
    pub depth: usize,
}

type Key = (String, i64);
type Incoming = HashMap<Key, Vec<Key>>;

fn incoming(store: &Store) -> Result<Incoming> {
    let mut map: HashMap<Key, Vec<Key>> = HashMap::new();
    for e in store.edges_with_resolution()? {
        if let Some(target) = &e.target {
            map.entry(target.clone())
                .or_default()
                .push((e.file.clone(), e.line));
        }
    }
    Ok(map)
}

fn roots(store: &Store, name: &str) -> Result<Vec<Key>> {
    Ok(store
        .definitions(name)?
        .into_iter()
        .map(|d| (d.file, d.line))
        .collect())
}

/// Innermost fn whose body contains `line` — its definition key.
fn enclosing(fns: &HashMap<String, Vec<(i64, i64)>>, site: &Key) -> Option<Key> {
    let spans = fns.get(&site.0)?;
    spans
        .iter()
        .filter(|(start, end)| *start < site.1 && site.1 <= *end)
        .max_by_key(|(start, _)| *start)
        .map(|(start, _)| (site.0.clone(), *start))
}

/// Callers tree for `name`, upward, at most `max_depth` levels.
///
/// A definition may have several roots (same name in different modules);
/// cycles are collapsed — a function already expanded is not expanded
/// again.
pub fn call_tree(store: &Store, name: &str, max_depth: usize) -> Result<Vec<CallNode>> {
    let incoming = incoming(store)?;
    let fns = store.function_spans()?;
    let mut visited: HashSet<Key> = HashSet::new();
    let mut out = Vec::new();
    for root in roots(store, name)? {
        let mut node = CallNode {
            file: root.0.clone(),
            line: root.1,
            children: Vec::new(),
        };
        if visited.insert(root.clone()) && max_depth > 0 {
            add_callers(
                &incoming,
                &fns,
                &root,
                0,
                max_depth,
                &mut visited,
                &mut node.children,
            );
        }
        out.push(node);
    }
    Ok(out)
}

fn add_callers(
    incoming: &Incoming,
    fns: &HashMap<String, Vec<(i64, i64)>>,
    def: &Key,
    depth: usize,
    max_depth: usize,
    visited: &mut HashSet<Key>,
    out: &mut Vec<CallNode>,
) {
    if depth >= max_depth {
        return;
    }
    for site in incoming.get(def).into_iter().flatten() {
        let mut child = CallNode {
            file: site.0.clone(),
            line: site.1,
            children: Vec::new(),
        };
        if let Some(caller_def) = enclosing(fns, site) {
            if visited.insert(caller_def.clone()) {
                add_callers(
                    incoming,
                    fns,
                    &caller_def,
                    depth + 1,
                    max_depth,
                    visited,
                    &mut child.children,
                );
            }
        }
        out.push(child);
    }
}

/// All call sites transitively depending on `name` through resolved
/// edges, breadth-first so `depth` is the shortest distance.
pub fn blast_radius(store: &Store, name: &str) -> Result<Vec<Affected>> {
    let incoming = incoming(store)?;
    let fns = store.function_spans()?;
    let mut seen: HashSet<Key> = HashSet::new();
    let mut queue: VecDeque<(Key, usize)> = VecDeque::new();
    for root in roots(store, name)? {
        if seen.insert(root.clone()) {
            queue.push_back((root, 0));
        }
    }
    let mut out = Vec::new();
    while let Some((def, depth)) = queue.pop_front() {
        for site in incoming.get(&def).into_iter().flatten() {
            // Every distinct call site matters: a function calling the
            // symbol twice contributes two affected places, even though
            // the function itself is expanded only once (`seen`).
            out.push(Affected {
                file: site.0.clone(),
                line: site.1,
                depth: depth + 1,
            });
            if let Some(caller_def) = enclosing(&fns, site) {
                if seen.insert(caller_def.clone()) {
                    queue.push_back((caller_def, depth + 1));
                }
            }
        }
    }
    out.sort_by(|a, b| (a.depth, &a.file, a.line).cmp(&(b.depth, &b.file, b.line)));
    Ok(out)
}

/// One lexical occurrence of a name.
#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    pub file: String,
    pub line: i64,
    /// `definition` | `call` | `mention`
    pub kind: &'static str,
    /// Resolved target for calls that have one.
    pub target: Option<(String, i64)>,
}

/// Every lexical identifier occurrence of `name` in the indexed files,
/// classified against the symbols and edges we already know.
///
/// This reads the files at query time: the index supplies the file set,
/// and the lexer is the only honest way to see type positions, `use`
/// paths and patterns — those are not edges yet. Non-call occurrences
/// are reported as `mention`, never as a resolved fact.
pub fn references(
    store: &Store,
    root: &std::path::Path,
    name: &str,
    limit: usize,
) -> Result<Vec<Reference>> {
    let defs_list: Vec<Key> = store
        .definitions(name)?
        .into_iter()
        .map(|d| (d.file, d.line))
        .collect();
    let defs: HashSet<Key> = defs_list.iter().cloned().collect();
    let calls: HashMap<Key, Option<(String, i64)>> = store
        .callers(name)?
        .into_iter()
        .map(|c| ((c.file, c.line), c.target))
        .collect();
    let parts: Vec<&str> = name.split("::").collect();
    let matches_at = |tokens: &[crate::lexer::Token], src: &str, i: usize| -> bool {
        // A qualified name (`Enum::Variant`) is not one token: match the
        // whole `ident (:: ident)*` sequence.
        let mut j = i;
        for (k, part) in parts.iter().enumerate() {
            if k > 0 {
                let is_colon = |t: &crate::lexer::Token| {
                    t.kind == crate::lexer::Tok::Punct && crate::lexer::text(src, t) == ":"
                };
                if !tokens.get(j).is_some_and(is_colon) || !tokens.get(j + 1).is_some_and(is_colon)
                {
                    return false;
                }
                j += 2;
            }
            match tokens.get(j) {
                Some(t)
                    if t.kind == crate::lexer::Tok::Ident
                        && crate::lexer::text(src, t) == *part => {}
                _ => return false,
            }
            j += 1;
        }
        true
    };
    let mut seen: HashSet<Key> = HashSet::new();
    let mut out = Vec::new();
    for path in store.file_paths()? {
        let Ok(src) = std::fs::read_to_string(root.join(&path)) else {
            continue;
        };
        let tokens = crate::lexer::lex(&src);
        for (i, tok) in tokens.iter().enumerate() {
            if tok.kind != crate::lexer::Tok::Ident
                || !matches_at(&tokens, &src, i)
                || !seen.insert((path.clone(), tok.line as i64))
            {
                continue;
            }
            let key = (path.clone(), tok.line as i64);
            let (kind, target) = if defs.contains(&key) {
                ("definition", None)
            } else if let Some(target) = calls.get(&key) {
                ("call", target.clone())
            } else {
                ("mention", None)
            };
            out.push(Reference {
                file: path.clone(),
                line: tok.line as i64,
                kind,
                target,
            });
        }
    }
    // A qualified definition is written short at its own site
    // (`Ident,` inside `enum Tok`), so the token scan above cannot see
    // it — the index already knows where it is.
    for key in &defs_list {
        if seen.insert(key.clone()) {
            out.push(Reference {
                file: key.0.clone(),
                line: key.1,
                kind: "definition",
                target: None,
            });
        }
    }
    out.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    out.truncate(limit);
    Ok(out)
}

/// A symbol's source slice (declaration line through the closing brace
/// for fns, the declaration line otherwise).
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolSource {
    pub file: String,
    pub kind: String,
    pub name: String,
    pub start: i64,
    pub end: i64,
    pub text: String,
}

/// Source of the symbol named `name`, optionally restricted to a file.
/// Several same-name definitions without a file filter pick the first
/// and say so (`more`).
pub fn symbol_source(
    store: &Store,
    root: &std::path::Path,
    name: &str,
    file: Option<&str>,
    max_lines: usize,
) -> Result<Option<SymbolSource>> {
    let defs: Vec<_> = store
        .definitions(name)?
        .into_iter()
        .filter(|d| file.is_none_or(|f| d.file == f || d.file.ends_with(f)))
        .collect();
    let Some(def) = defs.first() else {
        return Ok(None);
    };
    let src = std::fs::read_to_string(root.join(&def.file))?;
    let lines: Vec<&str> = src.lines().collect();
    let start = def.line.max(1) as usize;
    // The symbol's own span (fn body) bounds the slice; `max_lines` only
    // caps pathological long bodies.
    let span_end = store_symbol_end(store, &def.file, def.line).max(def.line) as usize;
    let end = span_end
        .min(start + max_lines.saturating_sub(1))
        .min(lines.len());
    let text = lines
        .get(start - 1..end)
        .map(|slice| slice.join("\n"))
        .unwrap_or_default();
    Ok(Some(SymbolSource {
        file: def.file.clone(),
        kind: def.kind.clone(),
        name: def.name.clone(),
        start: start as i64,
        end: end as i64,
        text,
    }))
}

/// A symbol touched by a diff range.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangedSymbol {
    pub file: String,
    pub line: i64,
    pub kind: String,
    pub name: String,
}

/// Structural impact of a diff: which symbols the changed lines belong
/// to, and who calls those symbols transitively (resolved edges only).
#[derive(Debug, Clone, PartialEq)]
pub struct Impact {
    pub changed: Vec<ChangedSymbol>,
    pub affected: Vec<Affected>,
}

/// Parse `git diff --unified=0` output into (file, start, end) new-side
/// line ranges — the lines the change produced.
fn diff_ranges(root: &std::path::Path, rev: &str) -> Result<Vec<(String, i64, i64)>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--unified=0", rev, "--", "*.rs"])
        .output()
        .context("running git diff")?;
    if !output.status.success() {
        bail!(
            "git diff {rev} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut ranges = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.trim_start_matches("b/").trim();
            current = if path == "/dev/null" {
                None
            } else {
                Some(path.to_string())
            };
            continue;
        }
        if let (Some(file), Some(hunk)) = (&current, line.strip_prefix("@@ ")) {
            // @@ -old_start,old_len +new_start,new_len @@
            let Some(plus) = hunk.split_whitespace().find(|p| p.starts_with('+')) else {
                continue;
            };
            let mut parts = plus[1..].split(',');
            let start: i64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let len: i64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            if start > 0 {
                ranges.push((file.clone(), start, start + len.max(1) - 1));
            }
        }
    }
    Ok(ranges)
}

/// What a change since `rev` touches, and what that reaches.
pub fn impact(store: &Store, root: &std::path::Path, rev: &str) -> Result<Impact> {
    let ranges = diff_ranges(root, rev)?;
    let mut changed: Vec<ChangedSymbol> = Vec::new();
    for row in store.semantic_rows()? {
        let touch = ranges.iter().any(|(file, start, end)| {
            // Only fn spans are tracked; other symbols contribute their
            // declaration line.
            let sym_end = store_symbol_end(store, &row.file, row.line);
            file == &row.file && *start <= sym_end && row.line <= *end
        });
        if touch
            && !changed
                .iter()
                .any(|c| c.file == row.file && c.line == row.line)
        {
            changed.push(ChangedSymbol {
                file: row.file.clone(),
                line: row.line,
                kind: row.kind.clone(),
                name: row.name.clone(),
            });
        }
    }
    let mut affected = Vec::new();
    for symbol in &changed {
        for hit in blast_radius(store, &symbol.name)? {
            if !affected
                .iter()
                .any(|a: &Affected| a.file == hit.file && a.line == hit.line)
            {
                affected.push(hit);
            }
        }
    }
    affected.sort_by(|a, b| (a.depth, &a.file, a.line).cmp(&(b.depth, &b.file, b.line)));
    changed.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    Ok(Impact { changed, affected })
}

/// End line of the symbol in `file` whose start is `line`: fn spans come
/// from the index, everything else is a single line.
fn store_symbol_end(store: &Store, file: &str, line: i64) -> i64 {
    store
        .function_spans()
        .ok()
        .and_then(|fns| fns.get(file).cloned())
        .and_then(|spans| {
            spans
                .iter()
                .find(|(start, _)| *start == line)
                .map(|(_, end)| *end)
        })
        .unwrap_or(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn setup(src: &str) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), src).unwrap();
        crate::index(dir.path()).unwrap();
        let store = Store::open(&dir.path().join(".codeindex/index.db")).unwrap();
        (dir, store)
    }

    const CHAIN: &str = "fn leaf() {}\nfn mid() {\n    leaf();\n}\nfn top() {\n    mid();\n}\n";

    #[test]
    fn call_tree_walks_callers_upward() {
        let (_d, store) = setup(CHAIN);
        let tree = call_tree(&store, "leaf", 3).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!((tree[0].file.as_str(), tree[0].line), ("lib.rs", 1));
        let mid = &tree[0].children;
        assert_eq!(mid.len(), 1);
        assert_eq!((mid[0].file.as_str(), mid[0].line), ("lib.rs", 3));
        let top = &mid[0].children;
        assert_eq!(top.len(), 1);
        assert_eq!((top[0].file.as_str(), top[0].line), ("lib.rs", 6));
        // Depth limit stops expansion.
        let shallow = call_tree(&store, "leaf", 1).unwrap();
        assert!(shallow[0].children[0].children.is_empty());
    }

    #[test]
    fn blast_radius_lists_transitive_dependents_with_depth() {
        let (_d, store) = setup(CHAIN);
        let radius = blast_radius(&store, "leaf").unwrap();
        assert_eq!(
            radius,
            vec![
                Affected {
                    file: "lib.rs".into(),
                    line: 3,
                    depth: 1
                },
                Affected {
                    file: "lib.rs".into(),
                    line: 6,
                    depth: 2
                },
            ]
        );
    }

    #[test]
    fn recursion_does_not_loop() {
        let (_d, store) = setup("fn a() {\n    b();\n}\nfn b() {\n    a();\n}\n");
        let tree = call_tree(&store, "a", 5).unwrap();
        // a → (b's call site) b → ... visited collapses the cycle.
        assert_eq!(tree[0].children.len(), 1);
        let radius = blast_radius(&store, "a").unwrap();
        assert!(radius.len() <= 2);
    }

    #[test]
    fn symbol_source_slices_the_body() {
        let (dir, store) = setup(CHAIN);
        let src = symbol_source(&store, dir.path(), "mid", None, 50)
            .unwrap()
            .unwrap();
        assert_eq!((src.start, src.end), (2, 4));
        assert!(src.text.contains("fn mid()"), "{}", src.text);
        assert!(src.text.contains("leaf();"), "{}", src.text);
        assert!(symbol_source(&store, dir.path(), "nope", None, 50)
            .unwrap()
            .is_none());
    }

    #[test]
    fn references_understand_qualified_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "enum Tok {\n    Ident,\n}\nfn f(t: Tok) -> bool {\n    matches!(t, Tok::Ident)\n}\n",
        )
        .unwrap();
        crate::index(dir.path()).unwrap();
        let store = Store::open(&dir.path().join(".codeindex/index.db")).unwrap();
        let refs = references(&store, dir.path(), "Tok::Ident", 10).unwrap();
        let kinds: Vec<(&str, i64)> = refs.iter().map(|r| (r.kind, r.line)).collect();
        assert_eq!(kinds, vec![("definition", 2), ("mention", 5)]);
        // A plain name still matches its plain occurrences.
        let refs = references(&store, dir.path(), "Tok", 10).unwrap();
        assert!(refs.iter().any(|r| r.line == 1));
    }

    #[test]
    fn impact_maps_a_diff_to_symbols_and_their_callers() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn leaf() {}\nfn mid() {\n    leaf();\n}\n",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "initial"]);
        // Change leaf's body only.
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn leaf() {\n    let x = 1;\n}\nfn mid() {\n    leaf();\n}\n",
        )
        .unwrap();
        crate::index(dir.path()).unwrap();
        let store = Store::open(&dir.path().join(".codeindex/index.db")).unwrap();
        let impact = impact(&store, dir.path(), "HEAD").unwrap();
        let changed: Vec<&str> = impact.changed.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(changed, vec!["leaf"], "{:?}", impact.changed);
        assert!(
            impact
                .affected
                .iter()
                .any(|a| a.file == "lib.rs" && a.line == 5),
            "mid's call site must be affected: {:?}",
            impact.affected
        );
    }

    #[test]
    fn every_call_site_in_one_function_is_reported() {
        // Two calls to the same symbol in one function are two affected
        // places; the function is expanded upward only once.
        let (_d, store) = setup("fn leaf() {}\nfn mid() {\n    leaf();\n    leaf();\n}\n");
        let radius = blast_radius(&store, "leaf").unwrap();
        assert_eq!(radius.len(), 2, "{radius:#?}");
        assert!(radius.iter().all(|a| a.depth == 1));
    }

    #[test]
    fn unresolved_and_candidate_edges_are_not_followed() {
        // `s.len()` is a method call (candidate) — must not appear.
        let (_d, store) = setup("fn f(s: &str) {\n    s.len();\n}\n");
        assert!(blast_radius(&store, "f").unwrap().is_empty());
        assert!(call_tree(&store, "f", 3).unwrap()[0].children.is_empty());
    }
}
