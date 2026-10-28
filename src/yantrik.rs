//! M5 semantic layer: a DEDICATED YantrikDB instance — never the agent
//! memory one.
//!
//! Two databases with different contracts live behind the same engine:
//! the memory instance is belief-revised (decay, supersede, conflict
//! scan) and is shared by every agent client; a code index must be
//! deterministic. So this module talks to a separate instance (own
//! `data_dir`, own port, maintenance disabled) under its own namespace
//! and domain. Nothing here can read or write memory records.
//!
//! SQLite stays the source of truth: a symbol's declaration line is
//! mirrored as a memory whose text starts with a self-describing marker
//! (`[codeindex key=… | file | kind | name | line]`). Semantic hits are
//! hydrated from that marker — never guessed from a server-side score —
//! and sync is a delta: list what the instance already holds, push the
//! missing, forget the stale. The instance's `idempotency_key` support
//! makes re-pushing after a lost local state a no-op.

use crate::store::{SemanticRow, Store};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Connection settings, from the environment so the MCP server can be
/// launched with them (zcode/opencode config `env`).
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub url: String,
    pub token: String,
    pub namespace: String,
    pub domain: String,
}

impl Config {
    /// `None` when no instance is configured — the semantic layer is
    /// opt-in and silently absent, not silently degraded.
    pub fn from_env() -> Option<Config> {
        let url = std::env::var("CODEINDEX_YANTRIK_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        Some(Config {
            url: url.trim_end_matches('/').to_string(),
            token: std::env::var("CODEINDEX_YANTRIK_TOKEN").unwrap_or_default(),
            namespace: std::env::var("CODEINDEX_YANTRIK_NAMESPACE")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "codeindex".to_string()),
            domain: std::env::var("CODEINDEX_YANTRIK_DOMAIN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "code".to_string()),
        })
    }
}

/// A record found in the code instance's namespace.
#[derive(Debug, Clone, PartialEq)]
pub struct Remote {
    pub rid: String,
    /// Parsed from the text marker; `None` for foreign records, which
    /// sync leaves alone.
    pub key: Option<String>,
}

/// One semantic hit, hydrated from its marker.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub score: f64,
    pub file: String,
    pub kind: String,
    pub name: String,
    pub line: i64,
    pub signature: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub pushed: usize,
    /// Already present in the instance — nothing to do.
    pub existing: usize,
    pub forgotten: usize,
}

/// Parsed marker: the identity codeindex wrote, read back verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub key: String,
    pub file: String,
    pub kind: String,
    pub name: String,
    pub line: i64,
}

/// Stable identity of a symbol row: content-addressed, so an edited
/// declaration changes the key (the old record is forgotten, the new one
/// pushed) and a re-index with unchanged content hits the same key.
pub fn symbol_key(row: &SemanticRow) -> String {
    let material = format!(
        "v1|{}|{}|{}|{}|{}",
        row.file, row.kind, row.name, row.line, row.signature
    );
    crate::sha256_hex(material.as_bytes())[..16].to_string()
}

/// The memory text: a machine-readable marker followed by the
/// declaration itself. The marker makes a hit self-describing; the rest
/// is what the embedder sees.
pub fn memory_text(key: &str, row: &SemanticRow) -> String {
    format!(
        "[codeindex key={key} | {} | {} | {} | {}] {} {} — {}",
        row.file, row.kind, row.name, row.line, row.kind, row.name, row.signature
    )
}

/// Parse the marker at the start of a memory text.
pub fn parse_marker(text: &str) -> Option<Marker> {
    let rest = text.strip_prefix("[codeindex key=")?;
    let (head, _tail) = rest.split_once(']')?;
    let mut parts = head.split('|').map(str::trim);
    Some(Marker {
        key: parts.next()?.to_string(),
        file: parts.next()?.to_string(),
        kind: parts.next()?.to_string(),
        name: parts.next()?.to_string(),
        line: parts.next()?.parse().ok()?,
    })
}

/// The declaration part of a memory text (`kind name — signature`).
fn text_signature(text: &str, marker: &Marker) -> String {
    text.split_once(']')
        .map(|(_, tail)| tail.trim())
        .map(|tail| {
            let prefix = format!("{} {} — ", marker.kind, marker.name);
            tail.strip_prefix(&prefix).unwrap_or(tail).to_string()
        })
        .unwrap_or_default()
}

/// Transport seam: real TCP in production, a canned responder in tests.
pub trait Transport {
    fn request(
        &self,
        method: &str,
        url: &str,
        token: &str,
        body: Option<&Value>,
    ) -> Result<Value>;
}

pub struct HttpTransport;

impl Transport for HttpTransport {
    fn request(
        &self,
        method: &str,
        url: &str,
        token: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        let (host, port, path) = parse_http_url(url)?;
        let mut stream = TcpStream::connect_timeout(
            &format!("{host}:{port}")
                .parse()
                .with_context(|| format!("invalid address {host}:{port}"))?,
            Duration::from_secs(5),
        )
        .with_context(|| format!("connecting to {url}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(120)))?;
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\n\
             Accept: application/json\r\nConnection: close\r\n"
        );
        if !token.is_empty() {
            req.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if body.is_some() {
            req.push_str("Content-Type: application/json\r\n");
        }
        req.push_str(&format!("Content-Length: {}\r\n\r\n{payload}", payload.len()));
        stream.write_all(req.as_bytes())?;
        stream.flush()?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        parse_http_response(&raw)
    }
}

fn parse_http_url(url: &str) -> Result<(String, u16, String)> {
    let (scheme, rest) = url
        .split_once("://")
        .with_context(|| format!("URL without scheme: {url}"))?;
    if scheme != "http" {
        bail!(
            "only http:// is supported (got {scheme}://); the code instance is local"
        );
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/').to_string()),
        None => (rest, String::new()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .with_context(|| format!("bad port in {url}"))?,
        ),
        None => (authority.to_string(), 80),
    };
    Ok((host, port, path))
}

fn parse_http_response(raw: &[u8]) -> Result<Value> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("HTTP response without header terminator")?;
    let head = std::str::from_utf8(&raw[..split]).context("non-UTF-8 HTTP headers")?;
    let body = &raw[split + 4..];
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .context("HTTP response without status code")?;
    let decoded = decode_chunked_if_needed(head, body)?;
    if !(200..300).contains(&status) {
        let snippet = String::from_utf8_lossy(&decoded);
        bail!("HTTP {status}: {}", snippet.trim());
    }
    serde_json::from_slice(&decoded).context("response is not JSON")
}

fn decode_chunked_if_needed(head: &str, body: &[u8]) -> Result<Vec<u8>> {
    let chunked = head.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        line.starts_with("transfer-encoding:") && line.contains("chunked")
    });
    if !chunked {
        return Ok(body.to_vec());
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let line_end = body[pos..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("chunked body: missing size line")?;
        let size_line = std::str::from_utf8(&body[pos..pos + line_end])?;
        let size = usize::from_str_radix(
            size_line.split(';').next().unwrap_or("").trim(),
            16,
        )
        .context("chunked body: bad chunk size")?;
        pos += line_end + 2;
        if size == 0 {
            break;
        }
        out.extend_from_slice(&body[pos..pos + size]);
        pos += size + 2;
    }
    Ok(out)
}

pub struct Client {
    cfg: Config,
    transport: Box<dyn Transport>,
}

impl Client {
    pub fn new(cfg: Config) -> Client {
        Client {
            cfg,
            transport: Box::new(HttpTransport),
        }
    }

    #[cfg(test)]
    fn with_transport(cfg: Config, transport: Box<dyn Transport>) -> Client {
        Client { cfg, transport }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.transport
            .request("POST", &format!("{}{path}", self.cfg.url), &self.cfg.token, Some(body))
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.transport
            .request("GET", &format!("{}{path}", self.cfg.url), &self.cfg.token, None)
    }

    /// Idempotent by `key`: re-sending the same key + text is a no-op,
    /// the instance answers with the existing rid.
    pub fn remember(&self, key: &str, text: &str) -> Result<()> {
        self.remember_many(&[(key.to_string(), text.to_string())])
    }

    /// Batch form of [`Client::remember`]. The instance's quota allows
    /// 10k items per call; chunks of 500 keep requests bounded and are
    /// still 100× fewer round-trips than one-call-per-symbol.
    pub fn remember_many(&self, items: &[(String, String)]) -> Result<()> {
        for chunk in items.chunks(500) {
            let memories: Vec<Value> = chunk
                .iter()
                .map(|(key, text)| {
                    json!({
                        "text": text,
                        "memory_type": "semantic",
                        "importance": 0.5,
                        "namespace": self.cfg.namespace,
                        "domain": self.cfg.domain,
                        "idempotency_key": key,
                    })
                })
                .collect();
            let response = self.post("/v1/remember/batch", &json!({ "memories": memories }))?;
            if let Some(err) = response.get("error") {
                bail!("remember batch refused: {err}");
            }
        }
        Ok(())
    }

    pub fn forget(&self, rid: &str) -> Result<()> {
        self.post("/v1/forget", &json!({ "rid": rid }))?;
        Ok(())
    }

    /// Every record this namespace holds, paged. Foreign records (no
    /// marker) come back with `key: None`.
    pub fn list(&self) -> Result<Vec<Remote>> {
        let mut out = Vec::new();
        let mut offset = 0usize;
        loop {
            // The instance caps `limit` at 200; page through.
            let page = self.get(&format!(
                "/v1/memories?limit=200&offset={offset}&namespace={}",
                self.cfg.namespace
            ))?;
            let items = page
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                break;
            }
            let count = items.len();
            for item in items {
                if item.get("namespace").and_then(|v| v.as_str()) != Some(self.cfg.namespace.as_str())
                {
                    continue;
                }
                let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                out.push(Remote {
                    rid: item
                        .get("rid")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    key: parse_marker(text).map(|m| m.key),
                });
            }
            offset += count;
            if count < 200 {
                break;
            }
        }
        Ok(out)
    }

    /// Semantic recall inside the code namespace.
    pub fn recall(&self, query: &str, top_k: usize) -> Result<Vec<Hit>> {
        let body = json!({
            "query": query,
            "top_k": top_k,
            "namespace": self.cfg.namespace,
            "domain": self.cfg.domain,
        });
        let response = self.post("/v1/recall", &body)?;
        let results = response
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut hits = Vec::new();
        for item in results {
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let Some(marker) = parse_marker(text) else {
                continue; // not ours — never present foreign records as symbols
            };
            hits.push(Hit {
                score: item.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0),
                file: marker.file.clone(),
                kind: marker.kind.clone(),
                name: marker.name.clone(),
                line: marker.line,
                signature: text_signature(text, &marker),
            });
        }
        Ok(hits)
    }
}

/// Delta sync of the whole index into the code instance: push what is
/// missing, forget what no longer exists. Foreign records are untouched.
pub fn sync(store: &Store, client: &Client) -> Result<SyncStats> {
    let rows = store.semantic_rows()?;
    let mut remote: HashMap<String, Remote> = client
        .list()?
        .into_iter()
        .filter_map(|r| r.key.clone().map(|k| (k, r)))
        .collect();
    let mut stats = SyncStats::default();
    let mut missing: Vec<(String, String)> = Vec::new();
    for row in &rows {
        let key = symbol_key(row);
        if remote.remove(&key).is_some() {
            stats.existing += 1;
            continue;
        }
        missing.push((key.clone(), memory_text(&key, row)));
    }
    stats.pushed = missing.len();
    client.remember_many(&missing)?;
    for (_, stale) in remote {
        client.forget(&stale.rid)?;
        stats.forgotten += 1;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn row(file: &str, name: &str, line: i64, signature: &str) -> SemanticRow {
        SemanticRow {
            file: file.into(),
            kind: "fn".into(),
            name: name.into(),
            line,
            signature: signature.into(),
        }
    }

    /// Canned responder that keeps the instance's state in memory.
    struct Mock {
        records: RefCell<Vec<(String, String, String)>>, // rid, text, namespace
        pushes: RefCell<Vec<Value>>,
        forgets: RefCell<Vec<String>>,
        recall: RefCell<Vec<Value>>,
    }

    impl Mock {
        fn new() -> Mock {
            Mock {
                records: RefCell::new(Vec::new()),
                pushes: RefCell::new(Vec::new()),
                forgets: RefCell::new(Vec::new()),
                recall: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Mock {
        fn request(
            &self,
            method: &str,
            url: &str,
            _token: &str,
            body: Option<&Value>,
        ) -> Result<Value> {
            let path = url.rsplit("/v1/").next().unwrap_or("");
            match (method, path) {
                ("POST", "remember") => {
                    let body = body.cloned().unwrap_or_default();
                    self.pushes.borrow_mut().push(body.clone());
                    let key = body["idempotency_key"].as_str().unwrap_or("").to_string();
                    let text = body["text"].as_str().unwrap_or("").to_string();
                    let mut records = self.records.borrow_mut();
                    // idempotency: same key + text keeps the existing rid
                    if let Some((rid, ..)) = records.iter().find(|(_, t, _)| {
                        t.contains(&format!("key={key} ")) && t == &text
                    }) {
                        return Ok(json!({ "rid": rid }));
                    }
                    let rid = format!("rid-{}", records.len() + 1);
                    records.push((
                        rid.clone(),
                        text,
                        body["namespace"].as_str().unwrap_or("").to_string(),
                    ));
                    Ok(json!({ "rid": rid }))
                }
                ("POST", "remember/batch") => {
                    let items = body
                        .and_then(|b| b.get("memories"))
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let mut rids = Vec::new();
                    let mut records = self.records.borrow_mut();
                    for item in items {
                        self.pushes.borrow_mut().push(item.clone());
                        let rid = format!("rid-{}", records.len() + 1);
                        records.push((
                            rid.clone(),
                            item["text"].as_str().unwrap_or("").to_string(),
                            item["namespace"].as_str().unwrap_or("").to_string(),
                        ));
                        rids.push(rid);
                    }
                    Ok(json!({ "rids": rids, "count": rids.len() }))
                }
                ("GET", p) if p.starts_with("memories") => {
                    let items: Vec<Value> = self
                        .records
                        .borrow()
                        .iter()
                        .map(|(rid, text, ns)| {
                            json!({ "rid": rid, "text": text, "namespace": ns })
                        })
                        .collect();
                    Ok(json!({ "items": items, "total": items.len() }))
                }
                ("POST", "forget") => {
                    let rid = body.and_then(|b| b["rid"].as_str()).unwrap_or("").to_string();
                    self.forgets.borrow_mut().push(rid.clone());
                    self.records.borrow_mut().retain(|(r, ..)| r != &rid);
                    Ok(json!({ "found": true }))
                }
                ("POST", "recall") => Ok(json!({ "results": self.recall.borrow().clone() })),
                _ => bail!("unexpected {method} {url}"),
            }
        }
    }

    fn cfg() -> Config {
        Config {
            url: "http://127.0.0.1:1".into(),
            token: "t".into(),
            namespace: "codeindex".into(),
            domain: "code".into(),
        }
    }

    #[test]
    fn marker_roundtrips_and_rejects_foreign_text() {
        let r = row("src/lexer.rs", "lex", 33, "fn lex(src: &str) -> Vec<Token> {");
        let key = symbol_key(&r);
        let text = memory_text(&key, &r);
        let marker = parse_marker(&text).unwrap();
        assert_eq!(marker.key, key);
        assert_eq!(marker.file, "src/lexer.rs");
        assert_eq!(marker.kind, "fn");
        assert_eq!(marker.name, "lex");
        assert_eq!(marker.line, 33);
        assert_eq!(
            text_signature(&text, &marker),
            "fn lex(src: &str) -> Vec<Token> {"
        );
        assert!(parse_marker("just a memory").is_none());
    }

    #[test]
    fn keys_are_content_addressed() {
        let a = row("src/a.rs", "f", 1, "fn f() {}");
        let b = row("src/a.rs", "f", 1, "fn f(x: u8) {}");
        assert_eq!(symbol_key(&a), symbol_key(&row("src/a.rs", "f", 1, "fn f() {}")));
        assert_ne!(symbol_key(&a), symbol_key(&b));
    }

    #[test]
    fn sync_pushes_missing_and_forgets_stale() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("index.db")).unwrap();
        let mock = Mock::new();
        let client = Client::with_transport(cfg(), Box::new(mock));

        let a = row("a.rs", "alpha", 1, "fn alpha() {}");
        let b = row("b.rs", "beta", 2, "fn beta() {}");
        store
            .replace_file(
                "a.rs",
                "h",
                1,
                "crate::a",
                &[crate::Symbol {
                    name: "alpha".into(),
                    kind: "fn",
                    file: "a.rs".into(),
                    line: 1,
                    end_line: 1,
                    signature: "fn alpha() {}".into(),
                }],
                &[],
                &crate::scopes::Scopes::default(),
            )
            .unwrap();
        store
            .replace_file(
                "b.rs",
                "h",
                1,
                "crate::b",
                &[crate::Symbol {
                    name: "beta".into(),
                    kind: "fn",
                    file: "b.rs".into(),
                    line: 2,
                    end_line: 2,
                    signature: "fn beta() {}".into(),
                }],
                &[],
                &crate::scopes::Scopes::default(),
            )
            .unwrap();
        let first = sync(&store, &client).unwrap();
        assert_eq!(first, SyncStats { pushed: 2, existing: 0, forgotten: 0 });

        // Nothing changed: nothing is pushed.
        let second = sync(&store, &client).unwrap();
        assert_eq!(second, SyncStats { pushed: 0, existing: 2, forgotten: 0 });

        // beta disappears: its record is forgotten, alpha stays.
        store.remove_file("b.rs").unwrap();
        let third = sync(&store, &client).unwrap();
        assert_eq!(third, SyncStats { pushed: 0, existing: 1, forgotten: 1 });
        let _ = (a, b);
    }

    #[test]
    fn recall_hydrates_only_marked_records() {
        let mock = Mock::new();
        let r = row("src/lib.rs", "scan", 254, "pub fn scan(root: &Path) -> Result<Inventory> {");
        let key = symbol_key(&r);
        {
            let mut hits = mock.recall.borrow_mut();
            hits.push(json!({
                "score": 0.77,
                "text": memory_text(&key, &r)
            }));
            hits.push(json!({ "score": 0.5, "text": "forgot-to-mark this" }));
        }
        let client = Client::with_transport(cfg(), Box::new(mock));
        let hits = client.recall("entry point for scanning a tree", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "scan");
        assert_eq!(hits[0].line, 254);
        assert_eq!(hits[0].score, 0.77);
        assert!(hits[0].signature.starts_with("pub fn scan"));
    }

    #[test]
    fn http_url_parsing_rejects_https() {
        assert!(parse_http_url("https://127.0.0.1:7442").is_err());
        let (host, port, _) = parse_http_url("http://127.0.0.1:7442").unwrap();
        assert_eq!((host.as_str(), port), ("127.0.0.1", 7442));
        let (_, port, _) = parse_http_url("http://localhost").unwrap();
        assert_eq!(port, 80);
    }

    #[test]
    fn chunked_and_plain_responses_parse() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":1}";
        assert_eq!(parse_http_response(plain).unwrap()["ok"], 1);

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n";
        assert_eq!(parse_http_response(chunked).unwrap()["a"], 1);

        let err = b"HTTP/1.1 409 Conflict\r\n\r\n{\"error\":\"idempotency conflict\"}";
        let e = parse_http_response(err).unwrap_err().to_string();
        assert!(e.contains("409") && e.contains("idempotency"), "{e}");
    }
}
