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

/// Downward call node: a callee definition reached from a call site.
#[derive(Debug, Clone, PartialEq)]
pub struct CalleeNode {
    pub file: String,
    /// Definition line of the callee.
    pub line: i64,
    /// Call site line inside the parent.
    pub call_line: i64,
    pub children: Vec<CalleeNode>,
}

/// What `name` calls: the mirror of [`call_tree`]. Resolved edges only;
/// cycles are collapsed by remembering which definitions were expanded.
pub fn callee_tree(store: &Store, name: &str, max_depth: usize) -> Result<Vec<CalleeNode>> {
    let edges = store.edges_with_resolution()?;
    let fns = store.function_spans()?;
    let mut visited: HashSet<Key> = HashSet::new();
    Ok(roots(store, name)?
        .into_iter()
        .map(|root| CalleeNode {
            file: root.0.clone(),
            line: root.1,
            call_line: root.1,
            children: callees_of(&edges, &fns, &root, 1, max_depth, &mut visited),
        })
        .collect())
}

fn callees_of(
    edges: &[crate::store::EdgeInfo],
    fns: &HashMap<String, Vec<(i64, i64)>>,
    def: &Key,
    depth: usize,
    max_depth: usize,
    visited: &mut HashSet<Key>,
) -> Vec<CalleeNode> {
    if depth > max_depth || !visited.insert(def.clone()) {
        return Vec::new();
    }
    let end = fns
        .get(&def.0)
        .and_then(|spans| spans.iter().find(|(s, _)| *s == def.1).map(|(_, e)| *e))
        .unwrap_or(def.1);
    let mut out = Vec::new();
    for edge in edges {
        if edge.file != def.0 || edge.line <= def.1 || edge.line > end {
            continue;
        }
        let Some((tf, tl)) = &edge.target else {
            continue;
        };
        let child_def = (tf.clone(), *tl);
        out.push(CalleeNode {
            file: tf.clone(),
            line: *tl,
            call_line: edge.line,
            children: callees_of(edges, fns, &child_def, depth + 1, max_depth, visited),
        });
    }
    out
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

/// Type/trait hierarchy, both directions: which traits a type
/// implements, and which types implement a trait.
#[derive(Debug, Clone, PartialEq)]
pub struct Hierarchy {
    pub name: String,
    /// Kinds found for this name (struct, enum, trait, …).
    pub kinds: Vec<String>,
    /// Traits implemented by this type (`impl Trait for Type`).
    pub traits: Vec<String>,
    /// Types implementing this trait.
    pub implementors: Vec<String>,
    /// Supertraits declared on this trait.
    pub supertraits: Vec<String>,
}

pub fn hierarchy(store: &Store, name: &str) -> Result<Hierarchy> {
    // Impl blocks are named after the type but they do not declare it;
    // the hierarchy speaks about the declaration itself.
    let mut kinds: Vec<String> = store
        .definitions(name)?
        .into_iter()
        .map(|d| d.kind)
        .filter(|k| k != "impl")
        .collect();
    kinds.sort();
    kinds.dedup();
    Ok(Hierarchy {
        name: name.to_string(),
        kinds,
        traits: store.trait_impls_of(name)?,
        implementors: store.implementors_of(name)?,
        supertraits: store.supertraits_of(name)?,
    })
}

/// One graph node on a traced path.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceStep {
    pub file: String,
    /// Definition line.
    pub line: i64,
    /// Call site inside the previous step (None for the start).
    pub via: Option<(String, i64)>,
}

/// A call chain from one symbol to another. Both directions are
/// searched breadth-first through resolved edges only; the first
/// meeting point wins, so the result is a shortest known chain, not a
/// proof that no other path exists.
pub fn trace_path(store: &Store, from: &str, to: &str) -> Result<Option<Vec<TraceStep>>> {
    let edges = store.edges_with_resolution()?;
    let fns = store.function_spans()?;

    // def -> [(call site, callee def)]
    let mut forward: HashMap<Key, Vec<(Key, Key)>> = HashMap::new();
    // def -> [(call site, caller def)]
    let mut backward: HashMap<Key, Vec<(Key, Key)>> = HashMap::new();
    for edge in &edges {
        let Some(target) = &edge.target else {
            continue;
        };
        let site = (edge.file.clone(), edge.line);
        let callee = (target.0.clone(), target.1);
        if let Some(caller) = enclosing(&fns, &site) {
            forward
                .entry(caller.clone())
                .or_default()
                .push((site.clone(), callee.clone()));
            backward.entry(callee).or_default().push((site, caller));
        }
    }

    let starts = roots(store, from)?;
    let goals = roots(store, to)?;
    if starts.is_empty() || goals.is_empty() {
        return Ok(None);
    }
    // Forward BFS: node -> (previous node, call site).
    let mut parent: HashMap<Key, Option<(Key, Key)>> = HashMap::new();
    let mut queue: VecDeque<Key> = VecDeque::new();
    for start in &starts {
        if parent.insert(start.clone(), None).is_none() {
            queue.push_back(start.clone());
        }
    }
    let mut meet: Option<Key> = None;
    while let Some(node) = queue.pop_front() {
        if goals.contains(&node) {
            meet = Some(node);
            break;
        }
        if parent.len() > 10_000 {
            break; // bounded: a huge graph must not stall the tool
        }
        for (site, next) in forward.get(&node).into_iter().flatten() {
            if !parent.contains_key(next) {
                parent.insert(next.clone(), Some((node.clone(), site.clone())));
                queue.push_back(next.clone());
            }
        }
    }
    let Some(meet) = meet else {
        return Ok(None);
    };

    // Walk back to the start; then reverse into path order.
    let mut steps: Vec<TraceStep> = Vec::new();
    let mut cursor = meet;
    loop {
        match parent.get(&cursor).cloned().flatten() {
            Some((prev, site)) => {
                steps.push(TraceStep {
                    file: cursor.0.clone(),
                    line: cursor.1,
                    via: Some(site),
                });
                cursor = prev;
            }
            None => {
                steps.push(TraceStep {
                    file: cursor.0.clone(),
                    line: cursor.1,
                    via: None,
                });
                break;
            }
        }
    }
    steps.reverse();
    Ok(Some(steps))
}

/// External crates a symbol leans on: calls into them from its body and
/// imports in its file. Both are labelled by the resolver, not guessed
/// here.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalUsage {
    pub name: String,
    /// (crate, call sites) inside the symbol's span.
    pub calls: Vec<(String, usize)>,
    /// (crate, import count) in the symbol's file.
    pub imports: Vec<(String, usize)>,
}

pub fn external_calls(store: &Store, name: &str) -> Result<ExternalUsage> {
    let defs = store.definitions(name)?;
    let mut calls: Vec<(String, usize)> = Vec::new();
    let mut imports: Vec<(String, usize)> = Vec::new();
    let merge = |into: &mut Vec<(String, usize)>, key: String, count: usize| match into
        .iter_mut()
        .find(|(k, _)| *k == key)
    {
        Some((_, c)) => *c += count,
        None => into.push((key, count)),
    };
    for def in &defs {
        let end = store_symbol_end(store, &def.file, def.line);
        for (crate_name, count) in store.external_calls_in_span(&def.file, def.line, end)? {
            merge(&mut calls, crate_name, count);
        }
        for (crate_name, count) in store.external_imports_of(&def.file)? {
            merge(&mut imports, crate_name, count);
        }
    }
    calls.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    imports.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(ExternalUsage {
        name: name.to_string(),
        calls,
        imports,
    })
}

/// One item in a body outline.
#[derive(Debug, Clone, PartialEq)]
pub struct SkimItem {
    pub kind: String,
    pub name: String,
    pub line: i64,
    pub signature: String,
}

/// A symbol's outline: its own declaration plus the items declared
/// inside its body — enough to decide which lines are worth reading.
#[derive(Debug, Clone, PartialEq)]
pub struct Skim {
    pub file: String,
    pub kind: String,
    pub name: String,
    pub start: i64,
    pub end: i64,
    pub signature: String,
    pub items: Vec<SkimItem>,
}

pub fn skim(
    store: &Store,
    name: &str,
    file: Option<&str>,
    max_items: usize,
) -> Result<Option<Skim>> {
    let defs = store.definitions(name)?;
    let Some(def) = defs
        .into_iter()
        .find(|d| file.is_none_or(|f| d.file == f || d.file.ends_with(f)))
    else {
        return Ok(None);
    };
    let end = store_symbol_end(store, &def.file, def.line);
    let signature = store
        .signature_at(&def.file, def.line, &def.name)?
        .unwrap_or_default();
    let items = store
        .symbols_in_span(&def.file, def.line, end)?
        .into_iter()
        .filter(|s| !(s.line == def.line && s.name == def.name))
        .take(max_items)
        .map(|s| SkimItem {
            signature: store
                .signature_at(&s.file, s.line, &s.name)
                .ok()
                .flatten()
                .unwrap_or_default(),
            kind: s.kind,
            name: s.name,
            line: s.line,
        })
        .collect();
    Ok(Some(Skim {
        file: def.file,
        kind: def.kind,
        name: def.name,
        start: def.line,
        end,
        signature,
        items,
    }))
}

/// One call site with the raw text of the arguments it passes.
#[derive(Debug, Clone, PartialEq)]
pub struct CallArgs {
    pub file: String,
    pub line: i64,
    pub tier: String,
    pub args: Vec<String>,
}

/// Lexical argument provenance: a function's parameters and, for every
/// call site that reaches it, the source text of what is passed. This is
/// what the code *says*, not what the values *are* — no data-flow
/// analysis, and it is labelled accordingly.
#[derive(Debug, Clone, PartialEq)]
pub struct Provenance {
    pub name: String,
    pub params: Vec<String>,
    pub call_sites: Vec<CallArgs>,
}

/// Parameter names of a fn declaration, read from the tokens. `::` in a
/// type path is two colons, so a name is an ident followed by exactly
/// one colon; `&self` has no colon and is skipped.
fn fn_params(tokens: &[crate::lexer::Token], src: &str, name_idx: usize) -> Vec<String> {
    let colon_after = |j: usize| -> bool {
        tokens.get(j + 1).is_some_and(|t| {
            t.kind == crate::lexer::Tok::Punct && crate::lexer::text(src, t) == ":"
        }) && !tokens.get(j + 2).is_some_and(|t| {
            t.kind == crate::lexer::Tok::Punct && crate::lexer::text(src, t) == ":"
        })
    };
    let mut angle = 0i32;
    let mut j = name_idx + 1;
    let mut open = None;
    while j < tokens.len() {
        let t = &tokens[j];
        if t.kind == crate::lexer::Tok::Punct {
            match crate::lexer::text(src, t) {
                "<" => angle += 1,
                ">" => angle = (angle - 1).max(0),
                "(" if angle == 0 => {
                    open = Some(j);
                    break;
                }
                _ => {}
            }
        }
        j += 1;
    }
    let Some(open) = open else {
        return Vec::new();
    };
    let Some(close) = crate::scopes::matching_paren(tokens, src, open) else {
        return Vec::new();
    };
    let mut params = Vec::new();
    let mut depth = 0i32;
    let mut k = open + 1;
    while k < close {
        let t = &tokens[k];
        if t.kind == crate::lexer::Tok::Punct {
            match crate::lexer::text(src, t) {
                "(" | "<" | "[" => depth += 1,
                ")" | ">" | "]" => depth = (depth - 1).max(0),
                _ => {}
            }
        } else if t.kind == crate::lexer::Tok::Ident && depth == 0 && colon_after(k) {
            params.push(crate::lexer::text(src, t).to_string());
        }
        k += 1;
    }
    params
}

pub fn argument_provenance(
    store: &Store,
    root: &std::path::Path,
    name: &str,
    max_sites: usize,
) -> Result<Option<Provenance>> {
    let _ = root;
    let defs = store.definitions(name)?;
    let Some(def) = defs.iter().find(|d| d.kind == "fn") else {
        return Ok(None);
    };
    let src = std::fs::read_to_string(root.join(&def.file))?;
    let tokens = crate::lexer::lex(&src);
    // The declaration: Ident(name) right after Ident("fn").
    let mut name_idx = None;
    for i in 1..tokens.len() {
        if tokens[i].kind == crate::lexer::Tok::Ident
            && crate::lexer::text(&src, &tokens[i]) == name
            && tokens[i - 1].kind == crate::lexer::Tok::Ident
            && crate::lexer::text(&src, &tokens[i - 1]) == "fn"
        {
            name_idx = Some(i);
            break;
        }
    }
    let params = name_idx
        .map(|i| fn_params(&tokens, &src, i))
        .unwrap_or_default();

    // Call sites whose resolved target is this definition.
    let mut call_sites = Vec::new();
    let mut sources: HashMap<String, (Vec<crate::lexer::Token>, String)> = HashMap::new();
    for r in store.callers(name)? {
        let Some((tf, tl)) = &r.target else {
            continue;
        };
        if tf != &def.file || tl != &def.line {
            continue;
        }
        if call_sites.len() >= max_sites {
            break;
        }
        let (tokens, src) = sources.entry(r.file.clone()).or_insert_with(|| {
            let src = std::fs::read_to_string(root.join(&r.file)).unwrap_or_default();
            (crate::lexer::lex(&src), src)
        });
        let args = call_args_at(tokens, src, name, r.line);
        call_sites.push(CallArgs {
            file: r.file.clone(),
            line: r.line,
            tier: r.tier,
            args,
        });
    }
    Ok(Some(Provenance {
        name: name.to_string(),
        params,
        call_sites,
    }))
}

/// Raw text of the arguments at the call site of `callee` on `line`.
fn call_args_at(tokens: &[crate::lexer::Token], src: &str, callee: &str, line: i64) -> Vec<String> {
    let is = |t: &crate::lexer::Token, w: &str| {
        t.kind == crate::lexer::Tok::Ident && crate::lexer::text(src, t) == w
    };
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i].line as i64 > line {
            break; // past the call site
        }
        // The edge records the callee token's line: match exactly there,
        // so an earlier same-name call does not shadow this one.
        if tokens[i].line as i64 == line
            && is(&tokens[i], callee)
            && (i == 0 || !is(&tokens[i.saturating_sub(1)], "fn"))
        {
            // First "(" within a small window (turbofish is a few tokens).
            let mut j = i + 1;
            let mut seen = 0usize;
            while j < tokens.len() && seen < 12 {
                if tokens[j].kind == crate::lexer::Tok::Punct
                    && crate::lexer::text(src, &tokens[j]) == "("
                {
                    break;
                }
                j += 1;
                seen += 1;
            }
            if let Some(close) = j
                .checked_add(1)
                .and_then(|k| tokens.get(k).map(|_| ()))
                .and_then(|_| crate::scopes::matching_paren(tokens, src, j))
            {
                // Split the inside by commas at depth 0.
                let mut args = Vec::new();
                let (mut depth, mut seg_start) = (0i32, j + 1);
                let mut k = j + 1;
                while k < close {
                    let t = &tokens[k];
                    if t.kind == crate::lexer::Tok::Punct {
                        match crate::lexer::text(src, t) {
                            "(" | "[" | "{" | "<" => depth += 1,
                            ")" | "]" | "}" => depth = (depth - 1).max(0),
                            ">" => depth = (depth - 1).max(0),
                            "," if depth == 0 => {
                                args.push(arg_text(src, tokens, seg_start, k));
                                seg_start = k + 1;
                            }
                            _ => {}
                        }
                    }
                    k += 1;
                }
                args.push(arg_text(src, tokens, seg_start, close));
                return args;
            }
        }
        i += 1;
    }
    Vec::new()
}

fn arg_text(src: &str, tokens: &[crate::lexer::Token], from: usize, to: usize) -> String {
    let first = tokens.get(from);
    let last = tokens.get(to.saturating_sub(1));
    let (Some(first), Some(last)) = (first, last) else {
        return String::new();
    };
    let text = &src[first.start..last.end];
    let trimmed = text.trim();
    if trimmed.chars().count() > 80 {
        format!("{}…", trimmed.chars().take(79).collect::<String>())
    } else {
        trimmed.to_string()
    }
}

/// Per-file overview for the directory map.
#[derive(Debug, Clone, PartialEq)]
pub struct FileOverview {
    pub path: String,
    pub symbols: usize,
    pub lines: i64,
    /// Git state of the file, when the repo is a git checkout:
    /// `modified`, `added`, `untracked`, …
    pub git: Option<String>,
}

fn git_annotations(root: &std::path::Path) -> HashMap<String, String> {
    let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output()
    else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let (status, path) = line.split_at(line.len().min(2));
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        let state = match status.trim() {
            "M" | "MM" | "AM" => "modified",
            "??" => "untracked",
            "A" | "M " | "A " => "staged",
            "D" => "deleted",
            _ => "changed",
        };
        map.insert(path.trim_start_matches('"').to_string(), state.to_string());
    }
    map
}

pub fn list_files(store: &Store, root: &std::path::Path) -> Result<Vec<FileOverview>> {
    let git = git_annotations(root);
    Ok(store
        .files_overview()?
        .into_iter()
        .map(|(path, symbols, lines)| FileOverview {
            git: git.get(&path).cloned(),
            path,
            symbols,
            lines,
        })
        .collect())
}

/// One external crate with its usage counts.
#[derive(Debug, Clone, PartialEq)]
pub struct Dependency {
    pub crate_name: String,
    pub calls: usize,
    pub imports: usize,
}

pub fn list_dependencies(store: &Store) -> Result<Vec<Dependency>> {
    Ok(store
        .dependency_counts()?
        .into_iter()
        .map(|(crate_name, calls, imports)| Dependency {
            crate_name,
            calls,
            imports,
        })
        .collect())
}

/// One unresolved call with its reason — the honesty ledger, row by row.
#[derive(Debug, Clone, PartialEq)]
pub struct UnresolvedCall {
    pub file: String,
    pub line: i64,
    pub callee: String,
    pub reason: String,
    pub external_crate: Option<String>,
}

pub fn list_unresolved(
    store: &Store,
    file: Option<&str>,
    limit: usize,
) -> Result<Vec<UnresolvedCall>> {
    Ok(store
        .unresolved_calls(file, limit)?
        .into_iter()
        .map(|row| UnresolvedCall {
            file: row.file,
            line: row.line,
            callee: row.callee,
            reason: row.reason,
            external_crate: row.external_crate,
        })
        .collect())
}

/// Everything about one symbol in a single answer: definitions, caller
/// tiers, resolved callees, lexical references and hierarchy.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    pub definitions: Vec<crate::store::IndexedSymbol>,
    pub callers: (usize, usize, usize),
    pub callees: Vec<Key>,
    pub references: (usize, std::collections::BTreeMap<&'static str, usize>),
    pub hierarchy: Hierarchy,
}

pub fn profile(store: &Store, root: &std::path::Path, name: &str) -> Result<Profile> {
    let definitions = store.definitions(name)?;
    let mut callers = (0usize, 0usize, 0usize);
    for r in store.callers(name)? {
        match r.tier.as_str() {
            crate::resolve::TIER_RESOLVED => callers.0 += 1,
            crate::resolve::TIER_CANDIDATE => callers.1 += 1,
            _ => callers.2 += 1,
        }
    }
    // Depth-1 resolved callees of the first fn definition.
    let edges = store.edges_with_resolution()?;
    let fns = store.function_spans()?;
    let mut callees: Vec<Key> = Vec::new();
    if let Some(def) = definitions.iter().find(|d| d.kind == "fn") {
        let end = fns
            .get(&def.file)
            .and_then(|spans| {
                spans
                    .iter()
                    .find(|(start, _)| *start == def.line)
                    .map(|(_, e)| *e)
            })
            .unwrap_or(def.line);
        for edge in &edges {
            if edge.file != def.file || edge.line <= def.line || edge.line > end {
                continue;
            }
            if let Some((f, l)) = &edge.target {
                let key = (f.clone(), *l);
                if !callees.contains(&key) {
                    callees.push(key);
                }
            }
        }
    }
    let refs = references(store, root, name, 10_000)?;
    let mut by_kind: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for r in &refs {
        *by_kind.entry(r.kind).or_default() += 1;
    }
    Ok(Profile {
        name: name.to_string(),
        definitions,
        callers,
        callees,
        references: (refs.len(), by_kind),
        hierarchy: hierarchy(store, name)?,
    })
}

/// One fuzzy path match.
#[derive(Debug, Clone, PartialEq)]
pub struct FileMatch {
    pub path: String,
    pub symbols: usize,
}

/// Fuzzy path search over the indexed files: subsequence match on the
/// normalized path (`-`/`_` equal, case-insensitive). Files whose
/// basename contains the query whole come first.
pub fn find_file(store: &Store, query: &str, limit: usize) -> Result<Vec<FileMatch>> {
    let norm = |s: &str| s.to_lowercase().replace('-', "_");
    let needle = norm(query);
    let mut exact = Vec::new();
    let mut contains = Vec::new();
    let mut fuzzy = Vec::new();
    for (path, symbols, _lines) in store.files_overview()? {
        let p = norm(&path);
        let base = p.rsplit('/').next().unwrap_or(&p).to_string();
        if base == needle {
            exact.push(FileMatch { path, symbols });
        } else if base.contains(&needle) || p.contains(&format!("/{needle}")) {
            contains.push(FileMatch { path, symbols });
        } else if crate::store::subsequence(&needle, &p) {
            fuzzy.push(FileMatch { path, symbols });
        }
    }
    exact.truncate(limit);
    contains.truncate(limit);
    fuzzy.truncate(limit);
    Ok(exact.into_iter().chain(contains).chain(fuzzy).collect())
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
    fn external_calls_reports_crates_and_imports() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "use serde::Serialize;\nfn f() {\n    std::mem::drop(1);\n    local_missing();\n}\n",
        )
        .unwrap();
        crate::index(dir.path()).unwrap();
        let store = Store::open(&dir.path().join(".codeindex/index.db")).unwrap();
        let usage = external_calls(&store, "f").unwrap();
        assert!(
            usage.calls.iter().any(|(c, _)| c == "std"),
            "{:?}",
            usage.calls
        );
        assert!(
            usage.imports.iter().any(|(c, _)| c == "serde"),
            "{:?}",
            usage.imports
        );
        // A plain unresolved call is not an external crate.
        assert!(!usage.calls.iter().any(|(c, _)| c == "local_missing"));
    }

    #[test]
    fn skim_lists_nested_items() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn outer() {\n    fn helper() {}\n    struct Inner;\n    let x = 1;\n}\n",
        )
        .unwrap();
        crate::index(dir.path()).unwrap();
        let store = Store::open(&dir.path().join(".codeindex/index.db")).unwrap();
        let skimmed = skim(&store, "outer", None, 10).unwrap().unwrap();
        let names: Vec<(&str, &str)> = skimmed
            .items
            .iter()
            .map(|i| (i.name.as_str(), i.kind.as_str()))
            .collect();
        assert_eq!(names, vec![("helper", "fn"), ("Inner", "struct")]);
        assert!(skimmed.signature.contains("fn outer()"));
    }

    #[test]
    fn provenance_lists_params_and_raw_arguments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn target(count: u8, label: &str) {}\nfn caller() {\n    target(3 + 1, \"hi\");\n    target(\n        7,\n        \"multi\"\n    );\n}\n",
        )
        .unwrap();
        crate::index(dir.path()).unwrap();
        let store = Store::open(&dir.path().join(".codeindex/index.db")).unwrap();
        let p = argument_provenance(&store, dir.path(), "target", 10)
            .unwrap()
            .unwrap();
        assert_eq!(p.params, vec!["count", "label"]);
        assert_eq!(p.call_sites.len(), 2);
        assert_eq!(p.call_sites[0].tier, "resolved");
        assert_eq!(p.call_sites[0].args, vec!["3 + 1", "\"hi\""]);
        assert_eq!(
            p.call_sites[1].args,
            vec!["7", "\"multi\""],
            "multi-line call must still split cleanly"
        );
    }

    #[test]
    fn hierarchy_reports_both_directions_and_supertraits() {
        let (_d, store) = setup(
            "trait Base {}\ntrait Shape: Base {\n    fn area(&self);\n}\nstruct Square;\nimpl Shape for Square {\n    fn area(&self) {}\n}\nimpl Square {\n    fn new() -> Self { Square }\n}\n",
        );
        let square = hierarchy(&store, "Square").unwrap();
        assert_eq!(square.kinds, vec!["struct"]);
        assert_eq!(square.traits, vec!["Shape"]);
        assert!(square.implementors.is_empty());
        let shape = hierarchy(&store, "Shape").unwrap();
        assert_eq!(shape.kinds, vec!["trait"]);
        assert_eq!(shape.implementors, vec!["Square"]);
        assert_eq!(shape.supertraits, vec!["Base"]);
        assert!(shape.traits.is_empty());
    }

    #[test]
    fn trace_path_links_symbols_through_resolved_edges() {
        let (_d, store) = setup(CHAIN);
        let steps = trace_path(&store, "top", "leaf").unwrap().unwrap();
        let defs: Vec<(&str, i64)> = steps.iter().map(|s| (s.file.as_str(), s.line)).collect();
        assert_eq!(defs, vec![("lib.rs", 5), ("lib.rs", 2), ("lib.rs", 1)]);
        // The middle hop records where it was called.
        assert_eq!(steps[1].via, Some(("lib.rs".into(), 6)));
        assert_eq!(steps[0].via, None);
        // No path the other way round.
        assert!(trace_path(&store, "leaf", "top").unwrap().is_none());
    }

    #[test]
    fn profile_answers_everything_at_once() {
        let (dir, store) = setup(CHAIN);
        let p = profile(&store, dir.path(), "mid").unwrap();
        assert_eq!(p.definitions.len(), 1);
        assert_eq!(p.definitions[0].line, 2);
        // mid is called once (from top) and resolves.
        assert_eq!(p.callers, (1, 0, 0));
        assert_eq!(p.callees, vec![("lib.rs".to_string(), 1)]);
        assert_eq!(p.references.0, 2); // definition + call
        assert_eq!(p.references.1.get("definition"), Some(&1));
        assert_eq!(p.references.1.get("call"), Some(&1));
    }

    #[test]
    fn inventory_tools_cover_files_deps_and_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn f() {\n    std::mem::drop(1);\n}\n",
        )
        .unwrap();
        crate::index(dir.path()).unwrap();
        let store = Store::open(&dir.path().join(".codeindex/index.db")).unwrap();

        let files = list_files(&store, dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].symbols, 1);

        let deps = list_dependencies(&store).unwrap();
        assert!(
            deps.iter().any(|d| d.crate_name == "std" && d.calls >= 1),
            "{deps:?}"
        );

        let unresolved = list_unresolved(&store, None, 50).unwrap();
        assert!(unresolved
            .iter()
            .any(|u| u.callee == "drop" && u.external_crate.as_deref() == Some("std")));
    }

    #[test]
    fn callee_tree_walks_downward() {
        let (_d, store) = setup(CHAIN);
        let tree = callee_tree(&store, "top", 3).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!((tree[0].file.as_str(), tree[0].line), ("lib.rs", 5));
        let mid = &tree[0].children;
        assert_eq!(mid.len(), 1);
        assert_eq!((mid[0].file.as_str(), mid[0].line), ("lib.rs", 2));
        assert_eq!(mid[0].call_line, 6);
        let leaf = &mid[0].children;
        assert_eq!(leaf.len(), 1);
        assert_eq!((leaf[0].file.as_str(), leaf[0].line), ("lib.rs", 1));
        assert_eq!(leaf[0].call_line, 3);
        // A leaf has nothing below it.
        assert!(leaf[0].children.is_empty());
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
