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

/// A local binding with a known type inside one function: a parameter
/// (`fn f(col: &Store)`) or an annotated `let` (`let c: Store = ...`).
/// It is what turns `x.method()` into a resolvable target without type
/// inference.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalBinding {
    /// Start line of the enclosing fn.
    pub fn_line: i64,
    pub name: String,
    /// Base type name (`&mut Vec<u8>` → `Vec`).
    pub type_name: String,
    /// Line where the binding is introduced (latest wins).
    pub line: i64,
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
    pub bindings: Vec<LocalBinding>,
}

/// One call edge as stored (resolution fields excluded).
#[derive(Debug, Clone)]
pub struct EdgeRow {
    pub id: i64,
    pub src_file_id: i64,
    pub src_line: i64,
    pub callee: String,
    pub qualifier: Option<String>,
    /// Receiver identifier for method calls (`x` in `x.foo()`).
    pub receiver: Option<String>,
    /// `plain` | `method` | `self_method` | `path`
    pub call_kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    pub tier: &'static str,
    pub target: Option<(i64, i64)>,
    pub reason: Option<&'static str>,
    /// Root of an unresolved path that is not a local module — the crate
    /// the call most likely lands in (`std` in `std::mem::drop`). A
    /// label, not a resolution: the tier stays `unresolved`.
    pub external_crate: Option<String>,
}

fn resolved(target: (i64, i64)) -> Resolution {
    Resolution {
        tier: TIER_RESOLVED,
        target: Some(target),
        reason: None,
        external_crate: None,
    }
}

fn candidate(reason: &'static str) -> Resolution {
    Resolution {
        tier: TIER_CANDIDATE,
        target: None,
        reason: Some(reason),
        external_crate: None,
    }
}

fn unresolved(reason: &'static str) -> Resolution {
    Resolution {
        tier: TIER_UNRESOLVED,
        target: None,
        reason: Some(reason),
        external_crate: None,
    }
}

fn unresolved_external(reason: &'static str, crate_name: String) -> Resolution {
    Resolution {
        tier: TIER_UNRESOLVED,
        target: None,
        reason: Some(reason),
        external_crate: Some(crate_name),
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
    /// The module the item lives in — needed to find qualified children
    /// (`Enum::Variant`, `Trait::method`) declared beside it.
    pub module: String,
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
    /// fn spans (start, end) — used to find the enclosing fn of a call.
    fns: Vec<(i64, i64)>,
    bindings: Vec<LocalBinding>,
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
                program.items.entry(module.clone()).or_default().push(Item {
                    name: s.name.clone(),
                    kind: s.kind.clone(),
                    file_id: f.file_id,
                    line: s.line,
                    in_body: in_body(s.line),
                    module,
                });
            }
            // Associated consts/statics of `impl Type` blocks register as
            // qualified children (`Store::MAX`), same as enum variants.
            for imp in &f.impl_spans {
                let module = deepen(&f.module, &f.module_spans, imp.start_line as i64);
                for s in f.symbols.iter().filter(|s| {
                    matches!(s.kind.as_str(), "const" | "static")
                        && (imp.start_line as i64) < s.line
                        && s.line <= imp.end_line as i64
                }) {
                    program.items.entry(module.clone()).or_default().push(Item {
                        name: format!("{}::{}", imp.type_name, s.name),
                        kind: s.kind.clone(),
                        file_id: f.file_id,
                        line: s.line,
                        in_body: true,
                        module: module.clone(),
                    });
                }
            }
            // Trait methods are not module-scope names, but `Trait::method`
            // and `use Trait::method` do reach them: register them as
            // qualified items in the trait's own module.
            for tr in &f.trait_spans {
                let module = deepen(&f.module, &f.module_spans, tr.start_line as i64);
                for s in f.symbols.iter().filter(|s| {
                    s.kind == "fn"
                        && (tr.start_line as i64) < s.line
                        && s.line <= tr.end_line as i64
                }) {
                    program.items.entry(module.clone()).or_default().push(Item {
                        name: format!("{}::{}", tr.name, s.name),
                        kind: "fn".to_string(),
                        file_id: f.file_id,
                        line: s.line,
                        in_body: true,
                        module: module.clone(),
                    });
                }
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
            let fns = f
                .symbols
                .iter()
                .filter(|s| s.kind == "fn")
                .map(|s| (s.line, s.end_line))
                .collect();
            ctx.insert(
                f.file_id,
                FileCtx {
                    module: f.module,
                    spans: f.module_spans,
                    impls: f.impl_spans,
                    fns,
                    bindings: f.bindings,
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
        let resolution = match edge.call_kind.as_str() {
            "method" => self.resolve_method(&module, edge),
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
        };
        // A qualified path whose root is not local points outside the
        // index: label the crate so callers can say "external" honestly.
        if resolution.tier == TIER_UNRESOLVED
            && resolution.external_crate.is_none()
            && resolution.reason == Some(R_QUALIFIER)
        {
            if let Some(qualifier) = &edge.qualifier {
                if let Some(root) = self.external_root(qualifier, &module) {
                    return unresolved_external(R_QUALIFIER, root);
                }
            }
        }
        resolution
    }

    /// First path segment of `path` if it names something outside this
    /// crate: not `crate`/`self`/`super`, not a local module or item, not
    /// a crate alias. Single-segment paths are left alone — `Vec::new()`
    /// would otherwise be labelled with a "crate" called Vec.
    fn external_root(&self, path: &str, module: &str) -> Option<String> {
        let trimmed = path.strip_prefix("::").unwrap_or(path);
        let first = trimmed.split("::").next()?;
        if first.is_empty() || matches!(first, "crate" | "self" | "super") {
            return None;
        }
        // Convention separates the two shapes: crates are snake_case,
        // types are CamelCase. `serde_json::parse` names a crate;
        // `Vec::new` / `Foo::bar` name a type we simply do not index.
        // Primitives (`f32`, `usize`, …) are types too.
        if first.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            || matches!(
                first,
                "f32"
                    | "f64"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "u128"
                    | "usize"
                    | "i8"
                    | "i16"
                    | "i32"
                    | "i64"
                    | "i128"
                    | "isize"
                    | "bool"
                    | "char"
                    | "str"
            )
        {
            return None;
        }
        if self.program.crate_aliases.contains_key(first) {
            return None;
        }
        let root = ns_root(module);
        if self.program.modules.contains(&format!("{root}::{first}")) {
            return None;
        }
        if self.lookup_name(root, first).is_some() {
            return None;
        }
        Some(first.to_string())
    }

    /// External crate behind one stored `use` declaration, if any.
    pub fn external_import_crate(&self, file_id: i64, line: i64, path: &str) -> Option<String> {
        let ctx = self.files.get(&file_id)?;
        let module = deepen(&ctx.module, &ctx.spans, line);
        if self.resolve_path_inner(path, &module, 0).is_some() {
            return None;
        }
        self.external_root(path, &module)
    }

    /// `x.foo()`: the receiver's declared type (parameter or annotated
    /// `let`) picks a local type, whose impls (or, for `dyn Trait`, the
    /// trait's methods) are searched. External types stay candidates —
    /// std is not indexed, and guessing would be dishonest.
    fn resolve_method(&self, module: &str, edge: &EdgeRow) -> Resolution {
        let Some(ctx) = self.files.get(&edge.src_file_id) else {
            return candidate(R_METHOD);
        };
        let Some(receiver) = &edge.receiver else {
            return candidate(R_METHOD);
        };
        let Some(fn_start) = ctx
            .fns
            .iter()
            .filter(|(start, end)| *start < edge.src_line && edge.src_line <= *end)
            .max_by_key(|(start, _)| *start)
            .map(|(start, _)| *start)
        else {
            return candidate(R_METHOD);
        };
        let Some(binding) = ctx
            .bindings
            .iter()
            .filter(|b| b.fn_line == fn_start && b.name == *receiver && b.line <= edge.src_line)
            .max_by_key(|b| b.line)
        else {
            return candidate(R_METHOD);
        };
        let type_name = if binding.type_name == "Self" {
            match self.self_type(ctx, edge.src_line) {
                Some(t) => t,
                None => return candidate(R_METHOD),
            }
        } else {
            binding.type_name.clone()
        };
        let in_scope = self
            .lookup_name(module, &type_name)
            .or_else(|| self.resolve_path(&type_name, module));
        match in_scope {
            Some(Binding::Item(it)) => {
                let by_impl = self.resolve_in_impl(&it.name, &edge.callee);
                if by_impl.tier != TIER_UNRESOLVED {
                    return by_impl;
                }
                match self.qualified_item(&it, &edge.callee) {
                    Some(q) => resolved((q.file_id, q.line)),
                    None => by_impl,
                }
            }
            _ => candidate(R_METHOD),
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
                    // An imported variant is a constructor call, which is
                    // as real a target as a fn.
                    if it.kind == "fn" || it.kind == "variant" {
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
            Some(Binding::Item(it)) => {
                let by_impl = self.resolve_in_impl(&it.name, name);
                if by_impl.tier != TIER_UNRESOLVED {
                    return by_impl;
                }
                // No impl fn: an enum's variant or a trait's method is a
                // qualified item beside its owner.
                match self.qualified_item(&it, name) {
                    Some(q) => resolved((q.file_id, q.line)),
                    None => by_impl,
                }
            }
        }
    }

    /// A child registered under its owner (`Enum::Variant`,
    /// `Trait::method`) in the owner's module.
    fn qualified_item(&self, owner: &Item, member: &str) -> Option<Item> {
        let qualified = format!("{}::{member}", owner.name);
        self.program
            .items
            .get(&owner.module)
            .into_iter()
            .flatten()
            .find(|i| i.name == qualified)
            .cloned()
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
    /// `bare::path`) from `from_module` to a module or an item. Re-export
    /// chains are followed through `use` bindings, bounded by depth.
    fn resolve_path(&self, path: &str, from_module: &str) -> Option<Binding> {
        self.resolve_path_inner(path, from_module, 0)
    }

    fn resolve_path_inner(&self, path: &str, from_module: &str, depth: usize) -> Option<Binding> {
        if depth > 8 {
            return None; // a re-export cycle
        }
        let mut segs: Vec<&str> = path.split("::").collect();
        if segs.first() == Some(&"") {
            return None; // `::abs` — 2015 absolute path, external
        }
        let mut base = from_module.to_string();
        let mut absolute = false;
        loop {
            match segs.first().copied() {
                Some("crate") => {
                    base = ns_root(from_module).to_string();
                    segs.remove(0);
                    absolute = true;
                    break;
                }
                Some("self") => {
                    segs.remove(0);
                    absolute = true;
                    break;
                }
                Some("super") => {
                    base = parent(&base)?;
                    segs.remove(0);
                    absolute = true;
                }
                _ => break,
            }
        }
        if !absolute {
            // Bare path: something at the namespace root (or a crate
            // alias for `pkg::item` in bin code), else external.
            let first = *segs.first()?;
            match self.resolve_module_or_item(ns_root(from_module), first, depth) {
                Some(binding) => {
                    segs.remove(0);
                    return self.walk_binding(binding, segs, depth);
                }
                None => {
                    base = self.program.crate_aliases.get(first)?.clone();
                    segs.remove(0);
                }
            }
        }
        self.walk_module(base, segs, depth)
    }

    /// Continue from an already resolved first binding.
    fn walk_binding(&self, binding: Binding, segs: Vec<&str>, depth: usize) -> Option<Binding> {
        match binding {
            Binding::Module(m) => self.walk_module(m, segs, depth),
            Binding::Item(it) => {
                let mut segs = segs;
                if segs.is_empty() {
                    return Some(Binding::Item(it));
                }
                // Remaining segments name a qualified child.
                let child = segs.remove(0);
                let q = self.qualified_item(&it, child)?;
                self.walk_binding(Binding::Item(q), segs, depth)
            }
        }
    }

    fn walk_module(&self, base: String, mut segs: Vec<&str>, depth: usize) -> Option<Binding> {
        let mut cur = base;
        while let Some(seg) = segs.first().copied() {
            let binding = self.resolve_module_or_item(&cur, seg, depth)?;
            segs.remove(0);
            match binding {
                Binding::Module(m) => cur = m,
                Binding::Item(it) => return self.walk_binding(Binding::Item(it), segs, depth),
            }
        }
        Some(Binding::Module(cur))
    }

    /// One step of a path: a module, an item declared in `module`, or a
    /// binding re-exported into it.
    fn resolve_module_or_item(&self, module: &str, seg: &str, depth: usize) -> Option<Binding> {
        let m = format!("{module}::{seg}");
        if self.program.modules.contains(&m) {
            return Some(Binding::Module(m));
        }
        if let Some(it) = self
            .program
            .items
            .get(module)
            .into_iter()
            .flatten()
            .find(|i| i.name == seg && !i.in_body)
        {
            return Some(Binding::Item(it.clone()));
        }
        self.resolve_import(module, seg, depth)
    }

    /// Follow a `use` binding: the re-export chain ends when the target
    /// resolves, or the depth cap stops a cycle.
    fn resolve_import(&self, module: &str, name: &str, depth: usize) -> Option<Binding> {
        if depth > 8 {
            return None;
        }
        for imp in self.program.imports.get(module).into_iter().flatten() {
            if imp.glob || imp.alias != name {
                continue;
            }
            if let Some(b) = self.resolve_path_inner(&imp.path, module, depth + 1) {
                return Some(b);
            }
        }
        None
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
    fn enum_variants_resolve_through_their_enum() {
        let (_d, e) = run(&[(
            "lib.rs",
            "enum Color {\n    Red,\n    Green(u8),\n    Blue { shade: u8 },\n}\nfn paint() {\n    Color::Green(1);\n}\n",
        )]);
        let edge = at(&e, "lib.rs", 7);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("lib.rs".into(), 3)));
    }

    #[test]
    fn imported_variant_constructors_resolve() {
        let (_d, e) = run(&[
            ("lib.rs", "mod a;\nmod b;\n"),
            ("a.rs", "pub enum Color { Red, Green }\n"),
            (
                "b.rs",
                "use crate::a::Color::Green;\nfn paint() {\n    Green();\n}\n",
            ),
        ]);
        let edge = at(&e, "b.rs", 3);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("a.rs".into(), 1)));
    }

    #[test]
    fn re_export_chains_resolve() {
        let (_d, e) = run(&[
            ("lib.rs", "mod a;\nmod facade;\nmod b;\n"),
            ("a.rs", "pub enum Color { Red, Green }\n"),
            ("facade.rs", "pub use crate::a::Color;\n"),
            (
                "b.rs",
                "use crate::facade::Color;\nfn paint() {\n    Color::Green();\n    let _ = Color::Red;\n}\n",
            ),
        ]);
        let edge = at(&e, "b.rs", 3);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("a.rs".into(), 1)));
    }

    #[test]
    fn trait_methods_resolve_through_the_trait() {
        let (_d, e) = run(&[(
            "lib.rs",
            "trait Shape {\n    fn area(&self) -> u8;\n}\nfn measure(s: &dyn Shape) {\n    Shape::area(s);\n}\n",
        )]);
        let edge = at(&e, "lib.rs", 5);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("lib.rs".into(), 2)));
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
    fn receiver_types_from_parameters_resolve_methods() {
        let (_d, e) = run(&[(
            "lib.rs",
            "struct Store;\nimpl Store {\n    fn open(&self) {}\n}\nfn use_it(s: &Store) {\n    s.open();\n}\n",
        )]);
        let edge = at(&e, "lib.rs", 6);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("lib.rs".into(), 3)));
    }

    #[test]
    fn receiver_types_from_let_annotations_resolve_methods() {
        let (_d, e) = run(&[(
            "lib.rs",
            "struct Store;\nimpl Store {\n    fn open(&self) {}\n}\nfn f() {\n    let s: Store = Store;\n    s.open();\n}\n",
        )]);
        let edge = at(&e, "lib.rs", 7);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("lib.rs".into(), 3)));
    }

    #[test]
    fn dyn_trait_receivers_resolve_to_the_trait_method() {
        let (_d, e) = run(&[(
            "lib.rs",
            "trait Shape {\n    fn area(&self) -> u8;\n}\nfn measure(s: &dyn Shape) {\n    s.area();\n}\n",
        )]);
        let edge = at(&e, "lib.rs", 5);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("lib.rs".into(), 2)));
    }

    #[test]
    fn shadowed_receivers_use_the_latest_binding() {
        let (_d, e) = run(&[(
            "lib.rs",
            "struct A;\nstruct B;\nimpl A {\n    fn go(&self) {}\n}\nimpl B {\n    fn go(&self) {}\n}\nfn f(x: &A) {\n    let x: B = B;\n    x.go();\n}\n",
        )]);
        let edge = at(&e, "lib.rs", 11);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("lib.rs".into(), 7)));
    }

    #[test]
    fn external_receiver_types_stay_candidates() {
        let (_d, e) = run(&[("lib.rs", "fn f(s: &str) {\n    s.len();\n}\n")]);
        let edge = at(&e, "lib.rs", 2);
        assert_eq!(edge.tier, TIER_CANDIDATE);
        assert_eq!(edge.reason.as_deref(), Some(R_METHOD));
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
    fn unresolved_paths_carry_their_external_crate() {
        let (_d, e) = run(&[(
            "lib.rs",
            "mod a;\nfn f() {\n    std::mem::drop(1);\n    crate::a::target();\n}\n",
        )]);
        let std_edge = at(&e, "lib.rs", 3);
        assert_eq!(std_edge.tier, TIER_UNRESOLVED);
        assert_eq!(std_edge.external_crate.as_deref(), Some("std"));
        // A local path that simply does not exist is not "external".
        let local = at(&e, "lib.rs", 4);
        assert_eq!(local.tier, TIER_UNRESOLVED);
        assert_eq!(local.external_crate, None);
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
    fn sibling_crates_in_a_workspace_resolve_each_other() {
        let (_d, e) = run(&[
            ("a/src/lib.rs", "pub fn alpha() {}\n"),
            ("b/src/lib.rs", "fn caller() {\n    a::alpha();\n}\n"),
        ]);
        let edge = at(&e, "b/src/lib.rs", 2);
        assert_eq!(edge.tier, TIER_RESOLVED);
        assert_eq!(edge.target, Some(("a/src/lib.rs".into(), 1)));
    }

    #[test]
    fn imported_associated_consts_resolve() {
        // Direct resolver fixture: `use crate::a::Store::MAX` in crate::b
        // must resolve to the qualified const in crate::a.
        let fd_a = FileData {
            file_id: 1,
            module: "crate::a".into(),
            symbols: vec![
                SymRow {
                    name: "Store".into(),
                    kind: "struct".into(),
                    line: 1,
                    end_line: 1,
                },
                SymRow {
                    name: "MAX".into(),
                    kind: "const".into(),
                    line: 3,
                    end_line: 3,
                },
            ],
            impl_spans: vec![ImplSpan {
                type_name: "Store".into(),
                trait_name: None,
                start_line: 2,
                end_line: 4,
            }],
            ..Default::default()
        };
        let fd_b = FileData {
            file_id: 2,
            module: "crate::b".into(),
            uses: vec![crate::scopes::UseDecl {
                path: "crate::a::Store::MAX".into(),
                alias: "MAX".into(),
                glob: false,
                line: 1,
            }],
            ..Default::default()
        };
        let resolver = Resolver::build(vec![fd_a, fd_b], HashMap::new());
        match resolver.resolve_path("crate::a::Store::MAX", "crate::b") {
            Some(Binding::Item(it)) => {
                assert_eq!(it.kind, "const");
                assert_eq!(it.line, 3);
            }
            other => panic!("expected the qualified const, got {other:?}"),
        }
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
