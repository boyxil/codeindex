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
use anyhow::Result;
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
            add_callers(&incoming, &fns, &root, 0, max_depth, &mut visited, &mut node.children);
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
            let mut record = false;
            if let Some(caller_def) = enclosing(&fns, site) {
                record = seen.insert(caller_def.clone());
                if record {
                    queue.push_back((caller_def, depth + 1));
                }
            }
            if record {
                out.push(Affected {
                    file: site.0.clone(),
                    line: site.1,
                    depth: depth + 1,
                });
            }
        }
    }
    out.sort_by(|a, b| (a.depth, &a.file, a.line).cmp(&(b.depth, &b.file, b.line)));
    Ok(out)
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
    fn unresolved_and_candidate_edges_are_not_followed() {
        // `s.len()` is a method call (candidate) — must not appear.
        let (_d, store) = setup("fn f(s: &str) {\n    s.len();\n}\n");
        assert!(blast_radius(&store, "f").unwrap().is_empty());
        assert!(call_tree(&store, "f", 3).unwrap()[0].children.is_empty());
    }
}
