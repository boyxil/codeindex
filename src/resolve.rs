//! In-house name resolver: module tree + `use` paths + impl/trait
//! awareness. This is the project's core know-how — and the place where
//! the honesty rule is enforced hardest.
//!
//! An edge is promoted to `resolved` only when the evidence is exact: a
//! unique definition reachable through the module tree, a `use` binding
//! or an impl block of an in-scope type. Everything else keeps a weaker
//! tier and a machine-readable reason:
//!
//! - method calls (`x.foo()`) need receiver type inference we do not
//!   have → `candidate` / `method-call`
//! - glob imports (`use m::*`) may bind the name, but without visibility
//!   checks that is a guess → `candidate` / `glob-import`
//! - ambiguous names → `candidate` / `ambiguous`
//! - external crates, unknown qualifiers, missing items → `unresolved`
//!
//! Deliberate approximations, all documented at their site: the module
//! tree comes from file layout (not from `mod` declarations), trait
//! methods are not callable through `Trait::method`, and cross-crate
//! workspace calls are external.

use crate::scopes::{ImplSpan, ModuleSpan, TraitSpan, UseDecl};
use std::collections::{HashMap, HashSet};

pub const TIER_RESOLVED: &str = "resolved";
pub const TIER_CANDIDATE: &str = "candidate";
pub const TIER_UNRESOLVED: &str = "unresolved";

pub const R_METHOD: &str = "method-call";
pub const R_GLOB: &str = "glob-import";
pub const R_AMBIGUOUS: &str = "ambiguous";
pub const R_NOT_IN_SCOPE: &str = "not-found-in-scope";
pub const R_QUALIFIER: &str = "qualifier-not-found";
pub const R_NOT_FN: &str = "not-a-function";
pub const R_NO_METHOD: &str = "no-such-fn-in-impl";
pub const R_NOT_IN_MODULE: &str = "not-found-in-module";

/// One symbol row as stored, without the file context.
#[derive(Debug, Clone, PartialEq)]
pub struct SymRow {
    pub name: String,
    pub kind: String,
    pub line: i64,
    pub end_line: i64,
}

/// Everything the resolver needs to know about one indexed file.
#[derive(Debug, Default, Clone)]
pub struct FileData {
    pub file_id: i64,
    /// Canonical module path, e.g. `crate`, `crate::store`.
    pub module: String,
    pub symbols: Vec<SymRow>,
    pub module_spans: Vec<ModuleSpan>,
    pub impl_spans: Vec<ImplSpan>,
    pub trait_spans: Vec<TraitSpan>,
    pub uses: Vec<UseDecl>,
}

/// One call edge as stored (resolution fields excluded).
#[derive(Debug, Clone)]
pub struct EdgeRow {
    pub id: i64,
    pub src_file_id: i64,
    pub src_line: i64,
    pub callee: String,
    pub qualifier: Option<String>,
    /// `plain` | `method` | `path`
    pub call_kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    pub tier: &'static str,
    pub target: Option<(i64, i64)>,
    pub reason: Option<&'static str>,
}

fn resolved(target: (i64, i64)) -> Resolution {
    Resolution {
        tier: TIER_RESOLVED,
        target: Some(target),
        reason: None,
    }
}

fn candidate(reason: &'static str) -> Resolution {
    Resolution {
        tier: TIER_CANDIDATE,
        target: None,
        reason: Some(reason),
    }
}

fn unresolved(reason: &'static str) -> Resolution {
    Resolution {
        tier: TIER_UNRESOLVED,
        target: None,
        reason: Some(reason),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub name: String,
    pub kind: String,
    pub file_id: i64,
    pub line: i64,
    /// Inside an `impl` or `trait` body: not a module-scope callable.
    pub in_body: bool,
}

#[derive(Debug, Clone)]
pub struct ImplInfo {
    pub fns: Vec<(String, i64, i64)>,
}

#[derive(Clone, Debug)]
enum Binding {
    Module(String),
    Item(Item),
}

#[derive(Debug, Default)]
pub struct Program {
    modules: HashSet<String>,
    items: HashMap<String, Vec<Item>>,
    imports: HashMap<String, Vec<UseDecl>>,
    impls: HashMap<String, Vec<ImplInfo>>,
    /// Crate name → namespace root (`crate::` style prefix), single-crate
    /// trees only; lets `bin` code call `pkg::item` in the same crate.
    crate_aliases: HashMap<String, String>,
}

struct FileCtx {
    module: String,
    spans: Vec<ModuleSpan>,
    impls: Vec<ImplSpan>,
}

pub struct Resolver {
    program: Program,
    files: HashMap<i64, FileCtx>,
}

/// Deepest inline module containing `line`, else the file's own module.
fn deepen(file_module: &str, spans: &[ModuleSpan], line: i64) -> String {
    let mut best: Option<&ModuleSpan> = None;
    for span in spans {
        if (span.start_line as i64) < line
            && line <= span.end_line as i64
            && best.is_none_or(|b| b.start_line < span.start_line)
        {
            best = Some(span);
        }
    }
    match best {
        Some(s) => format!("{file_module}::{}", s.path),
        None => file_module.to_string(),
    }
}

fn ns_root(module: &str) -> &str {
    module.split("::").next().unwrap_or(module)
}

fn parent(module: &str) -> Option<String> {
    module.rsplit_once("::").map(|(p, _)| p.to_string())
}

impl Resolver {
    pub fn build(files: Vec<FileData>, crate_aliases: HashMap<String, String>) -> Resolver {
        let mut program = Program {
            crate_aliases,
            ..Default::default()
        };
        let mut ctx = HashMap::new();

        for f in files {
            program.modules.insert(f.module.clone());
            for m in &f.module_spans {
                program.modules.insert(format!("{}::{}", f.module, m.path));
            }
            let in_body = |line: i64| {
                f.impl_spans
                    .iter()
                    .any(|i| (i.start_line as i64) < line && line <= i.end_line as i64)
                    || f.trait_spans
                        .iter()
                        .any(|t| (t.start_line as i64) < line && line <= t.end_line as i64)
            };
            for s in &f.symbols {
                let module = deepen(&f.module, &f.module_spans, s.line);
                program.items.entry(module).or_default().push(Item {
                    name: s.name.clone(),
                    kind: s.kind.clone(),
                    file_id: f.file_id,
                    line: s.line,
                    in_body: in_body(s.line),
                });
            }
            for imp in &f.impl_spans {
                let fns = f
                    .symbols
                    .iter()
                    .filter(|s| {
                        s.kind == "fn"
                            && (imp.start_line as i64) < s.line
                            && s.line <= imp.end_line as i64
                    })
                    .map(|s| (s.name.clone(), f.file_id, s.line))
                    .collect();
                program
                    .impls
                    .entry(imp.type_name.clone())
                    .or_default()
                    .push(ImplInfo { fns });
            }
            for u in &f.uses {
                let module = deepen(&f.module, &f.module_spans, u.line as i64);
                program.imports.entry(module).or_default().push(u.clone());
            }
            ctx.insert(
                f.file_id,
                FileCtx {
                    module: f.module,
                    spans: f.module_spans,
                    impls: f.impl_spans,
                },
            );
        }

        Resolver {
            program,
            files: ctx,
        }
    }

    /// Resolve one stored call edge.
    pub fn resolve_edge(&self, edge: &EdgeRow) -> Resolution {
        let Some(ctx) = self.files.get(&edge.src_file_id) else {
            return unresolved(R_NOT_IN_SCOPE);
        };
        let module = deepen(&ctx.module, &ctx.spans, edge.src_line);
        match edge.call_kind.as_str() {
            "method" => candidate(R_METHOD),
            // `self.foo()` has a known receiver: the enclosing impl's
            // type. Outside an impl the code does not compile, so the
            // edge stays an honest candidate there.
            "self_method" => match self.self_type(ctx, edge.src_line) {
                Some(t) => self.resolve_in_impl(&t, &edge.callee),
                None => candidate(R_METHOD),
            },
            "path" => match &edge.qualifier {
                Some(q) => {
                    let self_type = self.self_type(ctx, edge.src_line);
                    self.resolve_path_call(&module, q, &edge.callee, self_type.as_deref())
                }
                None => self.resolve_plain(&module, &edge.callee),
            },
            _ => self.resolve_plain(&module, &edge.callee),
        }
    }

    fn self_type(&self, ctx: &FileCtx, line: i64) -> Option<String> {
        let mut best: Option<&ImplSpan> = None;
        for imp in &ctx.impls {
            if (imp.start_line as i64) < line
                && line <= imp.end_line as i64
                && best.is_none_or(|b| b.start_line < imp.start_line)
            {
                best = Some(imp);
            }
        }
        best.map(|i| i.type_name.clone())
    }

    fn resolve_plain(&self, module: &str, name: &str) -> Resolution {
        let local: Vec<&Item> = self
            .program
            .items
            .get(module)
            .map(|v| v.iter().filter(|i| i.name == name && !i.in_body).collect())
            .unwrap_or_default();
        match local.len() {
            1 if local[0].kind == "fn" => return resolved((local[0].file_id, local[0].line)),
            1 => return unresolved(R_NOT_FN),
            n if n > 1 => return candidate(R_AMBIGUOUS),
            _ => {}
        }

        let mut glob = false;
        let mut found: Vec<Item> = Vec::new();
        for imp in self.program.imports.get(module).into_iter().flatten() {
            if imp.glob {
                glob = true;
                continue;
            }
            if imp.alias != name {
                continue;
            }
            match self.resolve_path(&imp.path, module) {
                Some(Binding::Item(it)) => {
                    if it.kind == "fn" {
                        found.push(it);
                    } else {
                        return unresolved(R_NOT_FN);
                    }
                }
                Some(Binding::Module(_)) => return unresolved(R_NOT_FN),
                None => {} // external import — cannot resolve
            }
        }
        match found.len() {
            1 => resolved((found[0].file_id, found[0].line)),
            n if n > 1 => candidate(R_AMBIGUOUS),
            _ if glob => candidate(R_GLOB),
            _ => unresolved(R_NOT_IN_SCOPE),
        }
    }

    fn resolve_path_call(
        &self,
        module: &str,
        qualifier: &str,
        name: &str,
        self_type: Option<&str>,
    ) -> Resolution {
        if qualifier == "Self" {
            let Some(t) = self_type else {
                return unresolved(R_QUALIFIER);
            };
            return self.resolve_in_impl(t, name);
        }
        // A bare name resolves through the current scope (items then
        // imports). An explicit path (`crate::a`, `self::x`) may also be
        // resolved as a path — but only when it is written as one: a
        // bare name that is not in scope must not silently reach a
        // sibling module.
        let binding = self.lookup_name(module, qualifier).or_else(|| {
            // Explicit paths and crate-name aliases are the only bare
            // names allowed through: `a::b` is written as a path, and a
            // crate alias names the current crate by definition.
            if qualifier.contains("::") || self.program.crate_aliases.contains_key(qualifier) {
                self.resolve_path(qualifier, module)
            } else {
                None
            }
        });
        match binding {
            None => unresolved(R_QUALIFIER),
            Some(Binding::Module(m)) => {
                match self
                    .program
                    .items
                    .get(&m)
                    .into_iter()
                    .flatten()
                    .find(|i| i.name == name)
                {
                    Some(it) if it.kind == "fn" => resolved((it.file_id, it.line)),
                    Some(_) => unresolved(R_NOT_FN),
                    None => unresolved(R_NOT_IN_MODULE),
                }
            }
            Some(Binding::Item(it)) => self.resolve_in_impl(&it.name, name),
        }
    }

    fn resolve_in_impl(&self, type_name: &str, name: &str) -> Resolution {
        let Some(blocks) = self.program.impls.get(type_name) else {
            return unresolved(R_NO_METHOD);
        };
        let fns: Vec<&(String, i64, i64)> = blocks
            .iter()
            .flat_map(|b| b.fns.iter())
            .filter(|(n, _, _)| n == name)
            .collect();
        match fns.len() {
            0 => unresolved(R_NO_METHOD),
            1 => resolved((fns[0].1, fns[0].2)),
            _ => candidate(R_AMBIGUOUS),
        }
    }

    /// A name in scope: module item first (Rust forbids shadowing it with
    /// an import), then `use` bindings. `in_body` items (methods, trait
    /// items) are not scope names.
    fn lookup_name(&self, module: &str, name: &str) -> Option<Binding> {
        if let Some(it) = self
            .program
            .items
            .get(module)
            .into_iter()
            .flatten()
            .find(|i| i.name == name && !i.in_body)
        {
            if it.kind == "mod" {
                let m = format!("{module}::{name}");
                if self.program.modules.contains(&m) {
                    return Some(Binding::Module(m));
                }
            }
            return Some(Binding::Item(it.clone()));
        }
        for imp in self.program.imports.get(module).into_iter().flatten() {
            if imp.glob || imp.alias != name {
                continue;
            }
            if let Some(b) = self.resolve_path(&imp.path, module) {
                return Some(b);
            }
        }
        None
    }

    /// Resolve a `use`-style path (`crate::a::b`, `self::x`, `super::y`,
    /// `bare::path`) from `from_module` to a module or an item.
    fn resolve_path(&self, path: &str, from_module: &str) -> Option<Binding> {
        let mut segs: Vec<&str> = path.split("::").collect();
        if segs.first() == Some(&"") {
            return None; // `::abs` — 2015 absolute path, external
        }
        let mut cur: Option<String> = None;
        while let Some(first) = segs.first().copied() {
            match first {
                "crate" => {
                    cur = Some(ns_root(from_module).to_string());
                    segs.remove(0);
                    break;
                }
                "self" => {
                    cur = Some(from_module.to_string());
                    segs.remove(0);
                    break;
                }
                "super" => {
                    let base = cur.as_deref().unwrap_or(from_module);
                    cur = Some(parent(base)?);
                    segs.remove(0);
                }
                _ => break,
            }
        }
        let mut cur = match cur {
            Some(c) => c,
            None => {
                // Bare path: local top-level module or item, else the
                // crate alias, else an external crate we do not index.
                let first = *segs.first()?;
                let local = format!("{}::{}", ns_root(from_module), first);
                if self.program.modules.contains(&local) {
                    segs.remove(0);
                    local
                } else if self.program.items.contains_key(ns_root(from_module))
                    && self
                        .program
                        .items
                        .get(ns_root(from_module))
                        .is_some_and(|v| v.iter().any(|i| i.name == first))
                {
                    ns_root(from_module).to_string()
                } else {
                    let root = self.program.crate_aliases.get(first)?;
                    segs.remove(0);
                    root.clone()
                }
            }
        };
        while !segs.is_empty() {
            let seg = segs.remove(0);
            let next = format!("{cur}::{seg}");
            if segs.is_empty() {
                if self.program.modules.contains(&next) {
                    return Some(Binding::Module(next));
                }
                return self
                    .program
                    .items
                    .get(&cur)
                    .into_iter()
                    .flatten()
                    .find(|i| i.name == seg)
                    .cloned()
                    .map(Binding::Item);
            }
            if !self.program.modules.contains(&next) {
                return None;
            }
            cur = next;
        }
        Some(Binding::Module(cur))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    /// Index a tempdir tree and return every edge with its resolution —
    /// the same fixture style the rest of the project uses.
    fn run(files: &[(&str, &str)]) -> (tempfile::TempDir, Vec<crate::store::EdgeInfo>) {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }
        crate::index(dir.path()).unwrap();
        let store = Store::open(&dir.path().join(".codeindex/index.db")).unwrap();
        (dir, store.edges_with_resolution().unwrap())
    }

    fn at<'a>(
        edges: &'a [crate::store::EdgeInfo],
        file: &str,
        line: i64,
    ) -> &'a crate::store::EdgeInfo {
        edges
            .iter()
            .find(|e| e.file == file && e.line == line)
            .unwrap_or_else(|| panic!("no edge at {file}:{line} in {edges:#?}"))
    }

    #[test]
    fn plain_calls_resolve_within_the_module() {
        let (_d, e) = run(&[(
            "lib.rs",
            "fn helper() {}\nfn caller() {\n    helper();\n}\n",
        )]);
        let edge = at(&e, "lib.rs", 3);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("lib.rs".into(), 1)));
        assert_eq!(edge.reason, None);
    }

    #[test]
    fn use_import_and_alias_bring_foreign_fns_into_scope() {
        let (_d, e) = run(&[
            ("lib.rs", "mod a;\nmod b;\n"),
            ("a.rs", "pub fn target() {}\n"),
            ("b.rs", "use crate::a::target;\nuse crate::a::target as t;\nfn go() {\n    target();\n    t();\n}\n"),
        ]);
        assert_eq!(at(&e, "b.rs", 4).target, Some(("a.rs".into(), 1)));
        assert_eq!(at(&e, "b.rs", 5).target, Some(("a.rs".into(), 1)));
    }

    #[test]
    fn super_paths_and_inline_module_paths_resolve() {
        let (_d, e) = run(&[
            ("lib.rs", "mod a {\n    pub fn target() {}\n}\n"),
            ("b.rs", "use super::a;\nfn go() {\n    a::target();\n}\n"),
        ]);
        // `a::target()` from crate::b — qualifier `a` found as a module.
        let edge = at(&e, "b.rs", 3);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("lib.rs".into(), 2)));
    }

    #[test]
    fn impl_type_calls_and_self_calls_resolve() {
        let (_d, e) = run(&[
            ("lib.rs", "mod store;\n"),
            (
                "store.rs",
                "pub struct Store;\nimpl Store {\n    pub fn open() {}\n    pub fn via_self() {\n        Self::open();\n    }\n}\nfn outside() {\n    Store::open();\n}\n",
            ),
        ]);
        // Self::open() inside the impl → the impl's fn open.
        let self_call = at(&e, "store.rs", 5);
        assert_eq!(self_call.tier, TIER_RESOLVED);
        assert_eq!(self_call.target, Some(("store.rs".into(), 3)));
        // Store::open() outside → same impl through the type.
        let via_type = at(&e, "store.rs", 9);
        assert_eq!(via_type.tier, TIER_RESOLVED);
        assert_eq!(via_type.target, Some(("store.rs".into(), 3)));
    }

    #[test]
    fn self_method_calls_resolve_inside_the_impl() {
        let (_d, e) = run(&[(
            "lib.rs",
            "struct S;\nimpl S {\n    fn helper(&self) {}\n    fn go(&self) {\n        self.helper();\n    }\n}\n",
        )]);
        let edge = at(&e, "lib.rs", 5);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("lib.rs".into(), 3)));
    }

    #[test]
    fn method_calls_stay_candidates_and_say_why() {
        let (_d, e) = run(&[("lib.rs", "fn f(x: &Vec<u8>) {\n    x.push(1);\n}\n")]);
        let edge = at(&e, "lib.rs", 2);
        assert_eq!(
            edge.tier, TIER_CANDIDATE,
            "type inference is not implemented"
        );
        assert_eq!(edge.reason.as_deref(), Some(R_METHOD));
        assert_eq!(edge.target, None);
    }

    #[test]
    fn glob_imports_are_candidates_not_guesses() {
        let (_d, e) = run(&[
            ("lib.rs", "mod a;\nmod b;\n"),
            ("a.rs", "pub fn target() {}\n"),
            ("b.rs", "use crate::a::*;\nfn go() {\n    target();\n}\n"),
        ]);
        let edge = at(&e, "b.rs", 3);
        assert_eq!(edge.tier, TIER_CANDIDATE);
        assert_eq!(edge.reason.as_deref(), Some(R_GLOB));
    }

    #[test]
    fn external_qualifiers_are_unresolved() {
        let (_d, e) = run(&[("lib.rs", "fn f() {\n    std::mem::drop(1);\n}\n")]);
        let edge = at(&e, "lib.rs", 2);
        assert_eq!(edge.tier, TIER_UNRESOLVED);
        assert_eq!(edge.reason.as_deref(), Some(R_QUALIFIER));
    }

    #[test]
    fn methods_are_not_module_scope_names() {
        // `helper` exists only as a method; an unqualified call must not
        // pretend it resolves to it.
        let (_d, e) = run(&[(
            "lib.rs",
            "struct S;\nimpl S {\n    fn helper() {}\n}\nfn caller() {\n    helper();\n}\n",
        )]);
        let edge = at(&e, "lib.rs", 6);
        assert_eq!(edge.tier, TIER_UNRESOLVED);
        assert_eq!(edge.reason.as_deref(), Some(R_NOT_IN_SCOPE));
    }

    #[test]
    fn crate_name_alias_resolves_bin_to_lib_calls() {
        let (_d, e) = run(&[
            ("pkg/src/lib.rs", "pub fn scan() {}\n"),
            ("pkg/src/main.rs", "fn main() {\n    pkg::scan();\n}\n"),
        ]);
        let edge = at(&e, "pkg/src/main.rs", 2);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("pkg/src/lib.rs".into(), 1)));
    }
}
