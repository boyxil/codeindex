//! Language detection and per-language declaration scanning for
//! non-Rust languages. Inventory level only: what symbols exist and
//! where. The call graph, resolver and semantic layer are Rust-only —
//! adding those for other languages is a separate project.

/// Per-language configuration for the line-based declaration scanner.
pub struct LangDef {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    /// Comment prefix that kills the rest of the line.
    pub line_comment: &'static str,
    /// `(keyword, kind)` — an item starts when a stripped line begins
    /// with (modifiers +) `keyword`, and the next word is the name.
    pub decls: &'static [(&'static str, &'static str)],
    /// Modifier keywords skipped before the declaration keyword.
    pub modifiers: &'static [&'static str],
}

pub static LANGS: &[LangDef] = &[
    LangDef {
        name: "rust",
        extensions: &["rs"],
        line_comment: "//",
        decls: &[],
        modifiers: &[],
    },
    LangDef {
        name: "python",
        extensions: &["py"],
        line_comment: "#",
        decls: &[("def", "def"), ("class", "class")],
        modifiers: &["async"],
    },
    LangDef {
        name: "typescript",
        extensions: &["ts", "tsx"],
        line_comment: "//",
        decls: &[
            ("function", "function"),
            ("class", "class"),
            ("interface", "interface"),
            ("type", "type"),
            ("enum", "enum"),
        ],
        modifiers: &["export", "default", "abstract", "declare", "async", "const"],
    },
    LangDef {
        name: "javascript",
        extensions: &["js", "jsx", "mjs", "cjs"],
        line_comment: "//",
        decls: &[
            ("function", "function"),
            ("class", "class"),
            ("async", "function"),
        ],
        modifiers: &["export", "default", "async", "const"],
    },
    LangDef {
        name: "go",
        extensions: &["go"],
        line_comment: "//",
        decls: &[
            ("func", "func"),
            ("type", "type"),
            ("struct", "struct"),
            ("interface", "interface"),
        ],
        modifiers: &[],
    },
];

/// Detect the language by file extension.
pub fn detect(path: &std::path::Path) -> Option<&'static LangDef> {
    let ext = path.extension()?.to_str()?;
    LANGS.iter().find(|l| l.extensions.contains(&ext))
}

/// Strip the line comment (anything after `line_comment` outside strings).
fn strip_comment<'a>(line: &'a str, marker: &str) -> &'a str {
    let mut in_string = false;
    let mut delim = '"';
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if c == '\\' {
                i += 1; // skip escaped char (may be multi-byte — next loop iteration handles it)
            } else if c == delim {
                in_string = false;
            }
        } else if line[i..].starts_with(marker) {
            return &line[..i];
        } else if c == '"' || c == '\'' || c == '`' {
            in_string = true;
            delim = c;
        }
        i += 1;
    }
    line
}

/// One declaration found by the scanner.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanItem {
    pub name: String,
    pub kind: &'static str,
    /// 1-based line of the declaration.
    pub line: usize,
    /// The stripped declaration line (truncated to 200 chars).
    pub signature: String,
}

/// Line-based declaration scanner: for each line, strip comments,
/// skip modifiers, and if the first word matches a declaration keyword,
/// extract the next word as the symbol name.
pub fn scan(src: &str, lang: &LangDef) -> Vec<ScanItem> {
    let mut out = Vec::new();
    for (i, raw_line) in src.lines().enumerate() {
        let line = strip_comment(raw_line, lang.line_comment);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        // Skip modifiers, then check for a declaration keyword.
        let mut wi = 0usize;
        while wi < words.len() && lang.modifiers.contains(&words[wi]) {
            wi += 1;
        }
        if wi >= words.len() {
            continue;
        }
        let Some((kw, kind)) = lang.decls.iter().find(|(k, _)| *k == words[wi]) else {
            continue;
        };
        // The name is the next word after the keyword.
        let Some(name) = words.get(wi + 1) else {
            continue;
        };
        // Clean the name: strip trailing `(`, `<`, `:`, `=` etc.
        let name: String = name
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if name.is_empty() {
            continue;
        }
        // Go: `type Name struct` → kind is "struct", not "type".
        let kind: &'static str = if lang.name == "go" && *kw == "type" {
            let next = words.get(wi + 2).copied().unwrap_or("");
            match next {
                "struct" => "struct",
                "interface" => "interface",
                _ => kind,
            }
        } else {
            kind
        };
        let sig = if line.len() > 200 {
            format!("{}…", line.chars().take(199).collect::<String>())
        } else {
            line.to_string()
        };
        out.push(ScanItem {
            name,
            kind,
            line: i + 1,
            signature: sig,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lang(name: &str) -> &'static LangDef {
        LANGS.iter().find(|l| l.name == name).unwrap()
    }

    #[test]
    fn python_declarations_are_found() {
        let src = "\
import os

def process(data: list) -> dict:
    return {}

class Handler:
    def handle(self):
        pass

# def commented_out():
";
        let items = scan(src, lang("python"));
        let names: Vec<(&str, &str)> = items.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        // Nested methods are indexed too (inventory, not scope analysis).
        assert_eq!(
            names,
            vec![("process", "def"), ("Handler", "class"), ("handle", "def")]
        );
    }

    #[test]
    fn typescript_declarations_are_found() {
        let src = "\
export class Parser {
    parse(): void {}
}

export interface Options {
    verbose: boolean;
}

type Handler = (x: number) => void;

function compile(src: string) {}

const helper = (x: number) => x;

// function commented(): void {}
";
        let items = scan(src, lang("typescript"));
        let names: Vec<(&str, &str)> = items.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert_eq!(
            names,
            vec![
                ("Parser", "class"),
                ("Options", "interface"),
                ("Handler", "type"),
                ("compile", "function"),
            ]
        );
    }

    #[test]
    fn go_declarations_are_found() {
        let src = "\
package main

type Server struct {
    port int
}

type Handler interface {
    Serve()
}

func main() {}

func (s *Server) Start() {}
";
        let items = scan(src, lang("go"));
        let names: Vec<(&str, &str)> = items.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert!(names.contains(&("Server", "struct")), "{names:?}");
        assert!(names.contains(&("Handler", "interface")), "{names:?}");
        assert!(names.contains(&("main", "func")), "{names:?}");
        // KNOWN LIMITATION: Go methods with receivers (`func (s *T) Name`)
        // are not found by the line-based scanner — the receiver parens
        // confuse the keyword→name extraction. Top-level funcs are found.
    }

    #[test]
    fn comments_are_stripped_before_matching() {
        let src = "// def fake(): pass\n// class Fake {}\ndef real(): pass\n";
        let items = scan(src, lang("python"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "real");
    }

    #[test]
    fn strings_are_not_scanned_for_declarations() {
        let src = "x = 'def fake(): pass'\ndef real(): pass\n";
        let items = scan(src, lang("python"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "real");
    }

    #[test]
    fn detect_by_extension() {
        assert_eq!(detect(std::path::Path::new("a.rs")).unwrap().name, "rust");
        assert_eq!(detect(std::path::Path::new("a.py")).unwrap().name, "python");
        assert_eq!(
            detect(std::path::Path::new("a.ts")).unwrap().name,
            "typescript"
        );
        assert_eq!(detect(std::path::Path::new("a.go")).unwrap().name, "go");
        assert!(detect(std::path::Path::new("a.txt")).is_none());
    }
}
