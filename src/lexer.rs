//! Hand-written Rust lexer. Produces a flat token stream sufficient for
//! item and call-site scanning — no AST, no external grammar.
//!
//! Correctly handles the constructs that would otherwise corrupt a naive
//! word-splitter: line/block comments (block comments nest), raw and byte
//! strings, char literals vs lifetimes (the `'a` vs `'a'` ambiguity),
//! string escapes, float literals vs range operators.

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident,
    Lifetime,
    /// String, byte string, raw string, char or byte char literal.
    Literal,
    /// Numeric literal (int or float, with suffix).
    Num,
    /// Any punctuation or operator character (`::`, `->` arrive as
    /// individual chars — the item scanner works on Ident/Literal text).
    Punct,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: Tok,
    /// Byte offsets into the source.
    pub start: usize,
    pub end: usize,
    /// 1-based line of the token start.
    pub line: usize,
}

/// Tokenize a Rust source buffer.
pub fn lex(src: &str) -> Vec<Token> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;

    while i < b.len() {
        let c = b[i];
        match c {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,

            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                // Block comment, nested per the Rust reference.
                let mut depth = 1usize;
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i] == b'\n' {
                        line += 1;
                    }
                    if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if i + 1 < b.len() && b[i] == b'*' && b[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }

            b'"' => {
                let start_line = line;
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                        if i < b.len() && b[i] == b'\n' {
                            line += 1;
                        }
                    } else if b[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
                if i < b.len() {
                    i += 1;
                }
                out.push(Token {
                    kind: Tok::Literal,
                    start,
                    end: i,
                    line: start_line,
                });
            }

            b'\'' => {
                let start = i;
                i += 1;
                if i < b.len() && b[i] == b'\\' {
                    i += 2;
                    while i < b.len() && b[i] != b'\'' {
                        if b[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    if i < b.len() {
                        i += 1;
                    }
                    out.push(Token {
                        kind: Tok::Literal,
                        start,
                        end: i,
                        line,
                    });
                } else if i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
                    // 'ident then either closing ' (char literal 'a') or
                    // nothing (lifetime 'a) — the lookahead decides.
                    let mut j = i;
                    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                        j += 1;
                    }
                    if j < b.len() && b[j] == b'\'' {
                        i = j + 1;
                        out.push(Token {
                            kind: Tok::Literal,
                            start,
                            end: i,
                            line,
                        });
                    } else {
                        i = j;
                        out.push(Token {
                            kind: Tok::Lifetime,
                            start,
                            end: i,
                            line,
                        });
                    }
                } else {
                    i += 1;
                    out.push(Token {
                        kind: Tok::Punct,
                        start,
                        end: i,
                        line,
                    });
                }
            }

            c if c.is_ascii_digit() => {
                let start = i;
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                // Float dot: only when followed by a digit or `_` — keeps
                // `0..range` and `1.method()` intact.
                if i < b.len()
                    && b[i] == b'.'
                    && i + 1 < b.len()
                    && (b[i + 1].is_ascii_digit() || b[i + 1] == b'_')
                {
                    i += 1;
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                        i += 1;
                    }
                }
                out.push(Token {
                    kind: Tok::Num,
                    start,
                    end: i,
                    line,
                });
            }

            c if c == b'_' || c.is_ascii_alphabetic() => {
                let start_line = line;
                let start = i;

                // Raw/byte string prefixes: r"…", r#"…"#, br"…", b"…", b'…'
                let mut j = i;
                let mut hashes = 0usize;
                if c == b'r' || c == b'b' {
                    j += 1;
                    if j < b.len() && b[j] == b'r' && c == b'b' {
                        j += 1; // br
                    }
                    while j < b.len() && b[j] == b'#' {
                        hashes += 1;
                        j += 1;
                    }
                }
                let is_raw_str = c == b'r' && j < b.len() && b[j] == b'"';
                let is_raw_byte = c == b'b' && j + 1 < b.len() && b[j] == b'r' && b[j + 1] == b'"';
                let is_byte_str = c == b'b' && j < b.len() && (b[j] == b'"');
                let is_byte_char = c == b'b' && j < b.len() && (b[j] == b'\'');

                if is_raw_str || is_raw_byte {
                    let quote_at = if is_raw_byte { j + 1 } else { j };
                    i = quote_at + 1; // past the opening quote
                    while i < b.len() {
                        if b[i] == b'\n' {
                            line += 1;
                        }
                        if b[i] == b'"' {
                            let mut k = i + 1;
                            let mut h = 0usize;
                            while k < b.len() && b[k] == b'#' {
                                h += 1;
                                k += 1;
                            }
                            if h == hashes {
                                i = k;
                                break;
                            }
                        }
                        i += 1;
                    }
                    out.push(Token {
                        kind: Tok::Literal,
                        start,
                        end: i,
                        line: start_line,
                    });
                } else if is_byte_str || is_byte_char {
                    let term = if is_byte_char { b'\'' } else { b'"' };
                    i = j + 1;
                    while i < b.len() && b[i] != term {
                        if b[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    if i < b.len() {
                        i += 1;
                    }
                    out.push(Token {
                        kind: Tok::Literal,
                        start,
                        end: i,
                        line: start_line,
                    });
                } else {
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                        i += 1;
                    }
                    out.push(Token {
                        kind: Tok::Ident,
                        start,
                        end: i,
                        line: start_line,
                    });
                }
            }

            _ => {
                // Consume the WHOLE UTF-8 scalar: byte-at-a-time Punct
                // tokens would land token bounds inside a multibyte
                // character (e.g. `…` in code), and any later text()
                // slice on such a token panics on the char boundary.
                let ch_len = src[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                i += ch_len;
                out.push(Token {
                    kind: Tok::Punct,
                    start: i - ch_len,
                    end: i,
                    line,
                });
            }
        }
    }
    out
}

/// Slice a token's text out of the source.
pub fn text<'a>(src: &'a str, t: &Token) -> &'a str {
    &src[t.start..t.end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(Tok, String)> {
        lex(src)
            .iter()
            .map(|t| (t.kind.clone(), text(src, t).to_string()))
            .collect()
    }

    #[test]
    fn strings_lifetimes_chars_are_distinguished() {
        let toks = kinds("let s = \"a\\\"b\";\nlet c = 'x';\nlet r: &'a str = \"z\";\n");
        let idents: Vec<&String> = toks
            .iter()
            .filter(|(k, _)| *k == Tok::Ident)
            .map(|(_, t)| t)
            .collect();
        assert_eq!(idents, &["let", "s", "let", "c", "let", "r", "str"]);
        assert!(toks.iter().any(|(k, t)| *k == Tok::Literal && t == "'x'"));
        assert!(toks.iter().any(|(k, t)| *k == Tok::Lifetime && t == "'a"));
        // The escaped quote must not end the string early.
        assert!(toks
            .iter()
            .any(|(k, t)| *k == Tok::Literal && t.contains('b')));
    }

    #[test]
    fn raw_strings_and_nested_block_comments() {
        let toks = kinds(
            "const A: &str = r#\"line1 \"quoted\" line2\"#;\n/* outer /* inner */ still */\nconst B = 1;\n",
        );
        assert!(
            toks.iter()
                .any(|(k, t)| *k == Tok::Literal && t.contains("quoted")),
            "raw string kept whole: {toks:?}"
        );
        assert!(!toks.iter().any(|(_, t)| t == "still"));
        assert!(toks.iter().any(|(k, t)| *k == Tok::Ident && t == "B"));
    }

    #[test]
    fn floats_do_not_eat_range_or_method() {
        let toks = kinds("let x = 0..n; let y = 1.5; let z = 3_f64;\n");
        assert!(toks.iter().any(|(k, t)| *k == Tok::Num && t == "0"));
        assert!(toks.iter().any(|(k, t)| *k == Tok::Ident && t == "n"));
        assert!(toks.iter().any(|(k, t)| *k == Tok::Num && t == "1.5"));
        assert!(toks.iter().any(|(k, t)| *k == Tok::Num && t == "3_f64"));
    }

    #[test]
    fn lines_are_tracked_through_multiline_constructs() {
        let src = "fn a() {\n    // c\n    let s = \"x\ny\";\n}\nfn b() {}\n";
        let toks = lex(src);
        let b = toks
            .iter()
            .find(|t| t.kind == Tok::Ident && &src[t.start..t.end] == "b")
            .unwrap();
        assert_eq!(b.line, 6);
    }

    #[test]
    fn byte_and_prefixed_literals_stay_whole() {
        let toks = kinds("let a = b\"x\\\"y\"; let r2 = r\"raw\"; let c = b'q';\n");
        assert!(toks
            .iter()
            .any(|(k, t)| *k == Tok::Literal && t.starts_with("b\"")));
        assert!(toks
            .iter()
            .any(|(k, t)| *k == Tok::Literal && t.starts_with("r\"")));
        assert!(toks
            .iter()
            .any(|(k, t)| *k == Tok::Literal && t.starts_with("b'")));
        assert!(!toks.iter().any(|(_, t)| t == "y"));
    }
}
