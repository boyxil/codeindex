//! Scope extraction: inline module spans, impl/trait spans and `use`
//! bindings — the raw material the M3 name resolver works from.
//!
//! All of it is token-level and approximate by design: the resolver only
//! promotes an edge to `resolved` when these facts are exact, and marks
//! everything else as candidate/unresolved with a reason.

use crate::lexer::{text, Tok, Token};

/// An inline `mod name { … }` block; lines are 1-based and inclusive.
/// `path` is the module chain relative to the file's own module
/// (`inner`, `inner::deeper`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleSpan {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// An `impl` block: the implemented type and its line span.
#[derive(Debug, Clone, PartialEq)]
pub struct ImplSpan {
    pub type_name: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// A `trait` block — needed only to know that `fn`s inside it are not
/// module-level callable names.
#[derive(Debug, Clone, PartialEq)]
pub struct TraitSpan {
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// One binding introduced by a `use` declaration. `path` is the target
/// as written (`crate::foo::Bar`, `super::x`, `::ext::T`); `alias` is
/// the bound name, empty for globs.
#[derive(Debug, Clone, PartialEq)]
pub struct UseDecl {
    pub path: String,
    pub alias: String,
    pub glob: bool,
    pub line: usize,
}

#[derive(Debug, Default, PartialEq)]
pub struct Scopes {
    pub modules: Vec<ModuleSpan>,
    pub impls: Vec<ImplSpan>,
    pub traits: Vec<TraitSpan>,
    pub uses: Vec<UseDecl>,
}

fn is_punct(src: &str, t: &Token, p: &str) -> bool {
    t.kind == Tok::Punct && text(src, t) == p
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}::{name}")
    }
}

/// Index of the `}` matching the `{` at `open`.
pub(crate) fn matching_brace(tokens: &[Token], src: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut j = open;
    while j < tokens.len() {
        if tokens[j].kind == Tok::Punct {
            match text(src, &tokens[j]) {
                "{" => depth += 1,
                "}" => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
        }
        j += 1;
    }
    None
}

pub fn extract_scopes(tokens: &[Token], src: &str) -> Scopes {
    let mut scopes = Scopes::default();
    walk(tokens, src, 0, tokens.len(), "", &mut scopes);
    scopes
}

fn walk(tokens: &[Token], src: &str, start: usize, end: usize, prefix: &str, out: &mut Scopes) {
    let mut i = start;
    while i < end {
        let tok = &tokens[i];
        if tok.kind != Tok::Ident {
            i += 1;
            continue;
        }
        match text(src, tok) {
            "mod" => {
                let Some(name_tok) = tokens.get(i + 1).filter(|t| t.kind == Tok::Ident) else {
                    i += 1;
                    continue;
                };
                let name = text(src, name_tok);
                if let Some(brace) = tokens.get(i + 2).filter(|t| is_punct(src, t, "{")) {
                    let _ = brace;
                    if let Some(close) = matching_brace(tokens, src, i + 2) {
                        let path = join(prefix, name);
                        out.modules.push(ModuleSpan {
                            path: path.clone(),
                            start_line: tok.line,
                            end_line: tokens[close].line,
                        });
                        // Recurse so nested inline modules get full chains.
                        walk(tokens, src, i + 3, close, &path, out);
                        i = close + 1;
                        continue;
                    }
                }
                // `mod name;` — the module comes from the file layout.
            }
            "impl" => {
                if let Some((type_name, brace)) = impl_header(tokens, src, i) {
                    if let Some(close) = matching_brace(tokens, src, brace) {
                        out.impls.push(ImplSpan {
                            type_name,
                            start_line: tok.line,
                            end_line: tokens[close].line,
                        });
                        i = close + 1;
                        continue;
                    }
                }
            }
            "trait" => {
                if let Some((name, brace)) = trait_header(tokens, src, i) {
                    if let Some(close) = matching_brace(tokens, src, brace) {
                        out.traits.push(TraitSpan {
                            name,
                            start_line: tok.line,
                            end_line: tokens[close].line,
                        });
                        i = close + 1;
                        continue;
                    }
                }
            }
            "use" => {
                if let Some(after) = parse_use(tokens, src, i, end, out) {
                    i = after;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Header of an `impl` item starting at `tokens[at]`: returns the
/// implemented type name and the index of the opening brace.
///
/// Idents count only at angle/paren depth 0, so `impl<T> Foo<T>` names
/// `Foo` (not the generic parameter); a `where` clause ends the type
/// search so `impl<T> Foo<T> where T: Send` cannot name `Send`.
pub(crate) fn impl_header(tokens: &[Token], src: &str, at: usize) -> Option<(String, usize)> {
    let mut depth = 0i32;
    let mut last: Option<&Token> = None;
    let mut j = at + 1;
    while j < tokens.len() {
        let t = &tokens[j];
        match t.kind {
            Tok::Ident if depth == 0 => {
                if text(src, t) == "where" {
                    while j < tokens.len()
                        && !(tokens[j].kind == Tok::Punct && text(src, &tokens[j]) == "{")
                    {
                        j += 1;
                    }
                    return if j < tokens.len() {
                        last.map(|t| (text(src, t).to_string(), j))
                    } else {
                        None
                    };
                }
                last = Some(t);
            }
            Tok::Punct => match text(src, t) {
                "<" => depth += 1,
                // A `->` in the header (`impl Fn() -> u8 for X`) yields a
                // bare `>` — clamp so depth cannot go negative.
                ">" => depth = (depth - 1).max(0),
                "(" => depth += 1,
                ")" => depth = (depth - 1).max(0),
                "{" if depth <= 0 => return last.map(|t| (text(src, t).to_string(), j)),
                _ => {}
            },
            _ => {}
        }
        j += 1;
    }
    None
}

fn trait_header(tokens: &[Token], src: &str, at: usize) -> Option<(String, usize)> {
    let name_tok = tokens.get(at + 1).filter(|t| t.kind == Tok::Ident)?;
    let name = text(src, name_tok).to_string();
    let mut depth = 0i32;
    let mut j = at + 2;
    while j < tokens.len() {
        let t = &tokens[j];
        if t.kind == Tok::Punct {
            match text(src, t) {
                "<" => depth += 1,
                ">" => depth = (depth - 1).max(0),
                "{" if depth <= 0 => return Some((name, j)),
                _ => {}
            }
        }
        j += 1;
    }
    None
}

/// Parse a `use` declaration whose `use` token sits at `at`; appends the
/// bindings it introduces and returns the index after the `;`.
fn parse_use(tokens: &[Token], src: &str, at: usize, end: usize, out: &mut Scopes) -> Option<usize> {
    let mut items: Vec<(String, bool)> = Vec::new();
    let mut depth = 0i32;
    let mut j = at + 1;
    while j < end {
        let t = &tokens[j];
        if t.kind == Tok::Punct {
            match text(src, t) {
                "{" => depth += 1,
                "}" => depth -= 1,
                ";" if depth == 0 => break,
                _ => {}
            }
        }
        items.push((text(src, t).to_string(), t.kind == Tok::Ident));
        j += 1;
    }
    let line = tokens[at].line;
    let mut parser = UseParser {
        toks: &items,
        i: 0,
        line,
        out,
    };
    parser.tree("").ok()?;
    Some(j + 1)
}

struct UseParser<'a> {
    toks: &'a [(String, bool)],
    i: usize,
    line: usize,
    out: &'a mut Scopes,
}

impl UseParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.toks.get(self.i).map(|(s, _)| s.as_str())
    }

    fn peek_ident(&self) -> Option<&str> {
        self.toks
            .get(self.i)
            .filter(|(_, is_ident)| *is_ident)
            .map(|(s, _)| s.as_str())
    }

    fn at_pair_of_colons(&self) -> bool {
        self.peek() == Some(":") && self.toks.get(self.i + 1).map(|(s, _)| s.as_str()) == Some(":")
    }

    fn push_binding(&mut self, base: &str, alias: Option<&str>, glob: bool) {
        let path = base.trim_end_matches("::").to_string();
        let alias = match alias {
            Some(a) => a.to_string(),
            None if glob => String::new(),
            None => path
                .rsplit("::")
                .next()
                .unwrap_or_default()
                .to_string(),
        };
        if !path.is_empty() && !alias.is_empty() || glob && !path.is_empty() {
            self.out.uses.push(UseDecl {
                path,
                alias,
                glob,
                line: self.line,
            });
        }
    }

    fn tree(&mut self, prefix: &str) -> Result<(), ()> {
        let mut base = prefix.to_string();
        // Leading `::` marks an absolute (crate-external) path.
        if self.at_pair_of_colons() {
            base.push_str("::");
            self.i += 2;
        }
        loop {
            match self.peek() {
                Some("{") => {
                    self.i += 1;
                    loop {
                        match self.peek() {
                            Some("}") => {
                                self.i += 1;
                                break;
                            }
                            Some(_) => {
                                self.tree(&base)?;
                                if self.peek() == Some(",") {
                                    self.i += 1;
                                }
                            }
                            None => return Err(()),
                        }
                    }
                    return Ok(());
                }
                Some("*") => {
                    self.i += 1;
                    self.push_binding(&base, None, true);
                    return Ok(());
                }
                Some("as") => {
                    self.i += 1;
                    let alias = self.peek_ident().unwrap_or("").to_string();
                    self.i += 1;
                    self.push_binding(&base, Some(&alias), false);
                    return Ok(());
                }
                Some(seg) if self.toks[self.i].1 => {
                    if !base.is_empty() && !base.ends_with("::") {
                        base.push_str("::");
                    }
                    base.push_str(seg);
                    self.i += 1;
                    if self.at_pair_of_colons() {
                        self.i += 2;
                    }
                }
                _ => {
                    self.push_binding(&base, None, false);
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    #[test]
    fn inline_modules_nest_and_carry_spans() {
        let src = "mod outer {\n    mod inner {\n        fn deep() {}\n    }\n    fn mid() {}\n}\nfn top() {}\n";
        let scopes = extract_scopes(&lex(src), src);
        let spans: Vec<(&str, usize, usize)> = scopes
            .modules
            .iter()
            .map(|m| (m.path.as_str(), m.start_line, m.end_line))
            .collect();
        assert_eq!(spans, vec![("outer", 1, 6), ("outer::inner", 2, 4)]);
    }

    #[test]
    fn impl_and_trait_spans_are_extracted() {
        let src = "impl Foo { fn a() {} }\ntrait Bar {\n    fn b(&self);\n}\nimpl<T> Pair<T> where T: Clone {\n    fn c() {}\n}\n";
        let scopes = extract_scopes(&lex(src), src);
        let impls: Vec<(&str, usize, usize)> = scopes
            .impls
            .iter()
            .map(|i| (i.type_name.as_str(), i.start_line, i.end_line))
            .collect();
        assert_eq!(impls, vec![("Foo", 1, 1), ("Pair", 5, 7)]);
        assert_eq!(scopes.traits.len(), 1);
        assert_eq!(scopes.traits[0].name, "Bar");
        assert_eq!((scopes.traits[0].start_line, scopes.traits[0].end_line), (2, 4));
    }

    #[test]
    fn use_declarations_cover_the_common_forms() {
        let src = "use crate::foo::Bar;\nuse crate::x as y;\nuse crate::m::{a, b::c, d as e};\nuse super::x::*;\nuse ::ext::T;\n";
        let uses = extract_scopes(&lex(src), src).uses;
        let got: Vec<(String, String, bool)> = uses
            .iter()
            .map(|u| (u.path.clone(), u.alias.clone(), u.glob))
            .collect();
        assert_eq!(
            got,
            vec![
                ("crate::foo::Bar".into(), "Bar".into(), false),
                ("crate::x".into(), "y".into(), false),
                ("crate::m::a".into(), "a".into(), false),
                ("crate::m::b::c".into(), "c".into(), false),
                ("crate::m::d".into(), "e".into(), false),
                ("super::x".into(), String::new(), true),
                ("::ext::T".into(), "T".into(), false),
            ]
        );
    }
}
