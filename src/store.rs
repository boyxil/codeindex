//! SQLite storage for the index (`.codeindex/index.db`).
//!
//! Schema v2: `meta(schema_version, extractor_version)`,
//! `files(path, hash, mtime, module)`, `symbols`, `edges` (with the
//! resolution fields), and the resolver's raw material: `modules`
//! (inline spans), `impls`, `traits`, `imports`.
//!
//! Edges carry the project's honesty contract in columns: `tier`
//! (`resolved` | `candidate` | `unresolved`), `resolved_file_id` +
//! `resolved_line` for the exact target, and `reason` for everything
//! that is not resolved.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

use crate::resolve::{self, EdgeRow, FileData, Resolution, SymRow};
use crate::scopes::Scopes;

pub const SCHEMA_VERSION: &str = "5";

/// What the extractor writes per file (symbols+edges since M2, call
/// scopes since M3). Unlike schema_version this describes the *derived*
/// data: bumping it makes stored hashes stale as parse decisions, so the
/// next index run re-parses everything once.
pub const EXTRACTOR_VERSION: &str = "8";

/// One indexed symbol joined with its file — the read API M2 queries
/// grow from.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedSymbol {
    /// Path relative to the indexed root.
    pub file: String,
    pub name: String,
    pub kind: String,
    pub line: i64,
}

/// One call site found for a callee name, with its resolution state.
#[derive(Debug, Clone, PartialEq)]
pub struct CallRef {
    pub file: String,
    pub line: i64,
    pub tier: String,
    pub target: Option<(String, i64)>,
    pub reason: Option<String>,
}

/// A call edge with its resolution, for call-graph queries.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeInfo {
    pub file: String,
    pub line: i64,
    pub callee: String,
    pub tier: String,
    pub target: Option<(String, i64)>,
    pub reason: Option<String>,
}

/// One symbol as the semantic layer sees it: the declaration line is the
/// text embedded, so a hit can be hydrated without trusting the server.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticRow {
    pub file: String,
    pub kind: String,
    pub name: String,
    pub line: i64,
    pub signature: String,
}

pub struct Store {
    conn: Connection,
}

// Connection itself is not Debug; tests want `.unwrap_err()` on open().
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open (creating if needed) the database and verify the schema
    /// version. A foreign version means we cannot interpret the rows —
    /// refusing is the honest move, silently migrating is not.
    pub fn open(db_path: &Path) -> Result<Store> {
        if let Some(dir) = db_path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn =
            Connection::open(db_path).with_context(|| format!("opening {}", db_path.display()))?;
        // WAL survives a crashed run; foreign keys make symbol cleanup
        // ride on file deletion.
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        // Two writers exist by design: the MCP server and manual CLI
        // runs. Wait instead of failing with "database is locked".
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS files (
                 id INTEGER PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE,
                 hash TEXT NOT NULL,
                 mtime INTEGER NOT NULL,
                 module TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS symbols (
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 name TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL,
                 signature TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
             CREATE INDEX IF NOT EXISTS idx_symbols_file_id ON symbols(file_id);
             CREATE TABLE IF NOT EXISTS edges (
                 id INTEGER PRIMARY KEY,
                 src_file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 src_line INTEGER NOT NULL,
                 callee TEXT NOT NULL,
                 qualifier TEXT,
                 receiver TEXT,
                 call_kind TEXT NOT NULL,
                 tier TEXT NOT NULL,
                 resolved_file_id INTEGER REFERENCES files(id),
                 resolved_line INTEGER,
                 reason TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_edges_callee ON edges(callee);
             CREATE INDEX IF NOT EXISTS idx_edges_file_id ON edges(src_file_id);
             CREATE TABLE IF NOT EXISTS modules (
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 path TEXT NOT NULL,
                 start_line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS impls (
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 type_name TEXT NOT NULL,
                 start_line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS traits (
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 name TEXT NOT NULL,
                 start_line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS embeddings (
                 symbol_key TEXT PRIMARY KEY,
                 model TEXT NOT NULL,
                 dim INTEGER NOT NULL,
                 vector BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS bindings (
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 fn_line INTEGER NOT NULL,
                 name TEXT NOT NULL,
                 type_name TEXT NOT NULL,
                 line INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_bindings_file ON bindings(file_id);
             CREATE TABLE IF NOT EXISTS imports (
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 path TEXT NOT NULL,
                 alias TEXT NOT NULL,
                 glob INTEGER NOT NULL,
                 line INTEGER NOT NULL
             );",
        )?;
        let stored: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match stored {
            None => {
                conn.execute(
                    "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION],
                )?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            Some(v) => bail!(
                "index schema version {v} but this build expects {SCHEMA_VERSION}; \
                 remove the .codeindex directory and re-index"
            ),
        }
        Ok(Store { conn })
    }

    /// Hash of every indexed file, keyed by relative path.
    pub fn file_hashes(&self) -> Result<HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT path, hash FROM files")?;
        let rows = stmt.query_map([], |r| {
            let path: String = r.get(0)?;
            let hash: String = r.get(1)?;
            Ok((path, hash))
        })?;
        rows.collect::<std::result::Result<HashMap<_, _>, _>>()
            .map_err(Into::into)
    }

    /// Upsert one file and replace its symbols, edges and scopes
    /// wholesale. Per-file wholesale replacement keeps the
    /// parse-and-store step free of line-level diffing — the hash
    /// already decided the file changed. The arguments are one parse
    /// result split for clarity; bundling them would not simplify it.
    #[allow(clippy::too_many_arguments)]
    pub fn replace_file(
        &self,
        path: &str,
        hash: &str,
        mtime: i64,
        module: &str,
        symbols: &[super::Symbol],
        calls: &[super::CallSite],
        scopes: &Scopes,
        bindings: &[crate::resolve::LocalBinding],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO files(path, hash, mtime, module) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
                 hash = excluded.hash, mtime = excluded.mtime, module = excluded.module",
            params![path, hash, mtime, module],
        )?;
        let file_id: i64 =
            tx.query_row("SELECT id FROM files WHERE path = ?1", params![path], |r| {
                r.get(0)
            })?;
        for table in [
            "symbols", "edges", "modules", "impls", "traits", "imports", "bindings",
        ] {
            let column = if table == "edges" {
                "src_file_id"
            } else {
                "file_id"
            };
            tx.execute(
                &format!("DELETE FROM {table} WHERE {column} = ?1"),
                params![file_id],
            )?;
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO symbols(file_id, name, kind, line, end_line, signature)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for s in symbols {
                stmt.execute(params![
                    file_id,
                    s.name,
                    s.kind,
                    s.line as i64,
                    s.end_line as i64,
                    s.signature
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO edges(
                     src_file_id, src_line, callee, qualifier, receiver, call_kind, tier
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for c in calls {
                stmt.execute(params![
                    file_id,
                    c.line as i64,
                    c.callee,
                    c.qualifier,
                    c.receiver,
                    call_kind_name(c.kind),
                    resolve::TIER_CANDIDATE
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO modules(file_id, path, start_line, end_line)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for m in &scopes.modules {
                stmt.execute(params![
                    file_id,
                    m.path,
                    m.start_line as i64,
                    m.end_line as i64
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO impls(file_id, type_name, start_line, end_line)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for i in &scopes.impls {
                stmt.execute(params![
                    file_id,
                    i.type_name,
                    i.start_line as i64,
                    i.end_line as i64
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO traits(file_id, name, start_line, end_line)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for t in &scopes.traits {
                stmt.execute(params![
                    file_id,
                    t.name,
                    t.start_line as i64,
                    t.end_line as i64
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO imports(file_id, path, alias, glob, line)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for u in &scopes.uses {
                stmt.execute(params![file_id, u.path, u.alias, u.glob, u.line as i64])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO bindings(file_id, fn_line, name, type_name, line)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for b in bindings {
                stmt.execute(params![file_id, b.fn_line, b.name, b.type_name, b.line])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop a file and (via cascade) everything derived from it.
    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", params![path])?;
        Ok(())
    }

    /// True when the store was written by the extractor this build runs.
    pub fn extractor_is_current(&self) -> bool {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'extractor_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .as_deref()
            == Some(EXTRACTOR_VERSION)
    }

    /// Record the extractor version the store has been written with.
    /// Public so tests can simulate older builds.
    pub fn set_extractor_version(&self, version: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES ('extractor_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![version],
        )?;
        Ok(())
    }

    /// Everything the resolver needs, one FileData per indexed file.
    pub fn load_files(&self) -> Result<Vec<FileData>> {
        let mut files: Vec<FileData> = Vec::new();
        let mut index_of: HashMap<i64, usize> = HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id, module FROM files ORDER BY path")?;
            let rows = stmt.query_map([], |r| {
                Ok(FileData {
                    file_id: r.get(0)?,
                    module: r.get(1)?,
                    ..Default::default()
                })
            })?;
            for f in rows {
                let f = f?;
                index_of.insert(f.file_id, files.len());
                files.push(f);
            }
        }
        let mut symbol_rows = self
            .conn
            .prepare("SELECT file_id, name, kind, line, end_line FROM symbols")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    SymRow {
                        name: r.get(1)?,
                        kind: r.get(2)?,
                        line: r.get(3)?,
                        end_line: r.get(4)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (file_id, row) in symbol_rows.drain(..) {
            if let Some(&idx) = index_of.get(&file_id) {
                files[idx].symbols.push(row);
            }
        }
        let mut module_rows = self
            .conn
            .prepare("SELECT file_id, path, start_line, end_line FROM modules")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    crate::scopes::ModuleSpan {
                        path: r.get(1)?,
                        start_line: r.get::<_, i64>(2)? as usize,
                        end_line: r.get::<_, i64>(3)? as usize,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (file_id, span) in module_rows.drain(..) {
            if let Some(&idx) = index_of.get(&file_id) {
                files[idx].module_spans.push(span);
            }
        }
        let mut impl_rows = self
            .conn
            .prepare("SELECT file_id, type_name, start_line, end_line FROM impls")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    crate::scopes::ImplSpan {
                        type_name: r.get(1)?,
                        start_line: r.get::<_, i64>(2)? as usize,
                        end_line: r.get::<_, i64>(3)? as usize,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (file_id, span) in impl_rows.drain(..) {
            if let Some(&idx) = index_of.get(&file_id) {
                files[idx].impl_spans.push(span);
            }
        }
        let mut trait_rows = self
            .conn
            .prepare("SELECT file_id, name, start_line, end_line FROM traits")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    crate::scopes::TraitSpan {
                        name: r.get(1)?,
                        start_line: r.get::<_, i64>(2)? as usize,
                        end_line: r.get::<_, i64>(3)? as usize,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (file_id, span) in trait_rows.drain(..) {
            if let Some(&idx) = index_of.get(&file_id) {
                files[idx].trait_spans.push(span);
            }
        }
        let mut import_rows = self
            .conn
            .prepare("SELECT file_id, path, alias, glob, line FROM imports")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    crate::scopes::UseDecl {
                        path: r.get(1)?,
                        alias: r.get(2)?,
                        glob: r.get(3)?,
                        line: r.get::<_, i64>(4)? as usize,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (file_id, decl) in import_rows.drain(..) {
            if let Some(&idx) = index_of.get(&file_id) {
                files[idx].uses.push(decl);
            }
        }
        let mut binding_rows = self
            .conn
            .prepare("SELECT file_id, fn_line, name, type_name, line FROM bindings")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    crate::resolve::LocalBinding {
                        fn_line: r.get(1)?,
                        name: r.get(2)?,
                        type_name: r.get(3)?,
                        line: r.get(4)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (file_id, binding) in binding_rows.drain(..) {
            if let Some(&idx) = index_of.get(&file_id) {
                files[idx].bindings.push(binding);
            }
        }
        Ok(files)
    }

    /// Every call edge, without resolution fields.
    pub fn all_edges(&self) -> Result<Vec<EdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, src_file_id, src_line, callee, qualifier, receiver, call_kind FROM edges",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(EdgeRow {
                id: r.get(0)?,
                src_file_id: r.get(1)?,
                src_line: r.get(2)?,
                callee: r.get(3)?,
                qualifier: r.get(4)?,
                receiver: r.get(5)?,
                call_kind: r.get(6)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Write resolution results back onto the edges (one transaction).
    pub fn apply_resolutions(&self, updates: &[(i64, Resolution)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE edges SET tier = ?2, resolved_file_id = ?3,
                 resolved_line = ?4, reason = ?5 WHERE id = ?1",
            )?;
            for (id, r) in updates {
                stmt.execute(params![
                    id,
                    r.tier,
                    r.target.map(|t| t.0),
                    r.target.map(|t| t.1),
                    r.reason
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Edge counts per tier: (resolved, candidate, unresolved).
    pub fn edge_tier_counts(&self) -> Result<(usize, usize, usize)> {
        let mut stmt = self
            .conn
            .prepare("SELECT tier, COUNT(*) FROM edges GROUP BY tier")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let (mut resolved, mut candidate, mut unresolved) = (0usize, 0usize, 0usize);
        for row in rows {
            let (tier, n) = row?;
            match tier.as_str() {
                resolve::TIER_RESOLVED => resolved = n as usize,
                resolve::TIER_CANDIDATE => candidate = n as usize,
                _ => unresolved = n as usize,
            }
        }
        Ok((resolved, candidate, unresolved))
    }

    /// `fn` spans per file — the call-graph queries use them to climb
    /// from a call site to the function that contains it.
    pub fn function_spans(&self) -> Result<HashMap<String, Vec<(i64, i64)>>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, s.line, s.end_line FROM symbols s
             JOIN files f ON f.id = s.file_id
             WHERE s.kind = 'fn'
             ORDER BY f.path, s.line",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut out: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
        for row in rows {
            let (path, start, end) = row?;
            out.entry(path).or_default().push((start, end));
        }
        Ok(out)
    }

    pub fn symbol_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    pub fn file_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Indexed file paths (relative to the root), ordered.
    pub fn file_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Symbol counts per kind, highest first — for `stats`.
    pub fn symbol_kind_counts(&self) -> Result<Vec<(String, usize)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT kind, COUNT(*) FROM symbols GROUP BY kind ORDER BY 2 DESC, 1")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Non-resolved edge reasons, highest first — the honesty ledger.
    pub fn edge_reason_counts(&self) -> Result<Vec<(String, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT reason, COUNT(*) FROM edges
             WHERE reason IS NOT NULL
             GROUP BY reason ORDER BY 2 DESC, 1",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Every symbol with its declaration text — input for the semantic
    /// sync (M5).
    pub fn semantic_rows(&self) -> Result<Vec<SemanticRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, s.kind, s.name, s.line, s.signature
             FROM symbols s JOIN files f ON f.id = s.file_id
             ORDER BY f.path, s.line, s.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SemanticRow {
                file: r.get(0)?,
                kind: r.get(1)?,
                name: r.get(2)?,
                line: r.get(3)?,
                signature: r.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Cached embedding of one symbol, if any: `(model, vector)`.
    pub fn embedding(&self, symbol_key: &str) -> Result<Option<(String, Vec<f32>)>> {
        let row = self
            .conn
            .query_row(
                "SELECT model, vector FROM embeddings WHERE symbol_key = ?1",
                params![symbol_key],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        Ok(row.map(|(model, bytes)| (model, decode_vector(&bytes))))
    }

    /// Cache one symbol's embedding. The model name is stored so a model
    /// switch invalidates old vectors instead of mixing spaces.
    pub fn put_embedding(&self, symbol_key: &str, model: &str, vector: &[f32]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO embeddings(symbol_key, model, dim, vector) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(symbol_key) DO UPDATE SET
                 model = excluded.model, dim = excluded.dim, vector = excluded.vector",
            params![
                symbol_key,
                model,
                vector.len() as i64,
                encode_vector(vector)
            ],
        )?;
        Ok(())
    }

    /// Drop cache rows that no longer belong to an indexed symbol.
    pub fn prune_embeddings(&self, keep: &std::collections::HashSet<String>) -> Result<usize> {
        let keys: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT symbol_key FROM embeddings")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut removed = 0;
        for key in keys {
            if !keep.contains(&key) {
                self.conn
                    .execute("DELETE FROM embeddings WHERE symbol_key = ?1", params![key])?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub fn embedding_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Every indexed symbol joined with its file path, ordered for
    /// stable output.
    pub fn symbols(&self) -> Result<Vec<IndexedSymbol>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, s.name, s.kind, s.line
             FROM symbols s JOIN files f ON f.id = s.file_id
             ORDER BY f.path, s.line, s.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(IndexedSymbol {
                file: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                line: r.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Substring, case-insensitive match on the symbol name, optionally
    /// narrowed by exact kind and by a path substring. Filtering happens
    /// in Rust rather than SQL: a `%needle%` LIKE cannot use the name
    /// index anyway, and Rust lowercasing is Unicode-correct where
    /// SQLite's is ASCII-only.
    pub fn search_symbols(
        &self,
        query: &str,
        kind: Option<&str>,
        path: Option<&str>,
    ) -> Result<Vec<IndexedSymbol>> {
        // `*`/`?` switch to glob matching on the name (case-sensitive:
        // patterns describe code); otherwise substring, case-insensitive.
        let glob = query.contains('*') || query.contains('?');
        let needle = query.to_lowercase();
        let matches = |name: &str| {
            if glob {
                glob_match(query, name)
            } else {
                name.to_lowercase().contains(&needle)
            }
        };
        Ok(self
            .symbols()?
            .into_iter()
            .filter(|s| matches(&s.name))
            .filter(|s| kind.is_none_or(|k| s.kind == k))
            .filter(|s| path.is_none_or(|p| s.file.contains(p)))
            .collect())
    }

    /// Exact-name definitions; a name may legitimately exist in several
    /// modules — all are reported until the resolver can disambiguate.
    pub fn definitions(&self, name: &str) -> Result<Vec<IndexedSymbol>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, s.name, s.kind, s.line
             FROM symbols s JOIN files f ON f.id = s.file_id
             WHERE s.name = ?1
             ORDER BY f.path, s.line",
        )?;
        let rows = stmt.query_map(params![name], |r| {
            Ok(IndexedSymbol {
                file: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                line: r.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Call sites whose written name equals `callee`, deduplicated per
    /// (file, line), carrying the resolution state verbatim.
    pub fn callers(&self, callee: &str) -> Result<Vec<CallRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT f.path, e.src_line, e.tier, tf.path, e.resolved_line, e.reason
             FROM edges e
             JOIN files f ON f.id = e.src_file_id
             LEFT JOIN files tf ON tf.id = e.resolved_file_id
             WHERE e.callee = ?1
             ORDER BY f.path, e.src_line",
        )?;
        let rows = stmt.query_map(params![callee], row_to_call_ref)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Every edge with its resolution — the call-graph query source.
    pub fn edges_with_resolution(&self) -> Result<Vec<EdgeInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, e.src_line, e.callee, e.tier, tf.path, e.resolved_line, e.reason
             FROM edges e
             JOIN files f ON f.id = e.src_file_id
             LEFT JOIN files tf ON tf.id = e.resolved_file_id
             ORDER BY f.path, e.src_line",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(EdgeInfo {
                file: r.get(0)?,
                line: r.get(1)?,
                callee: r.get(2)?,
                tier: r.get(3)?,
                target: match (r.get::<_, Option<String>>(4)?, r.get::<_, Option<i64>>(5)?) {
                    (Some(f), Some(l)) => Some((f, l)),
                    _ => None,
                },
                reason: r.get(6)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

fn row_to_call_ref(r: &rusqlite::Row<'_>) -> rusqlite::Result<CallRef> {
    let target_file: Option<String> = r.get(3)?;
    let target_line: Option<i64> = r.get(4)?;
    Ok(CallRef {
        file: r.get(0)?,
        line: r.get(1)?,
        tier: r.get(2)?,
        target: match (target_file, target_line) {
            (Some(f), Some(l)) => Some((f, l)),
            _ => None,
        },
        reason: r.get(5)?,
    })
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

/// Minimal glob: `*` matches any run, `?` one char. Iterative with
/// backtracking, no recursion.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn call_kind_name(kind: super::CallKind) -> &'static str {
    match kind {
        super::CallKind::Plain => "plain",
        super::CallKind::Method => "method",
        super::CallKind::MethodSelf => "self_method",
        super::CallKind::Path => "path",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_tmp(dir: &Path) -> Store {
        Store::open(&dir.join("index.db")).unwrap()
    }

    fn sym(name: &str, kind: &'static str) -> super::super::Symbol {
        super::super::Symbol {
            name: name.to_string(),
            kind,
            file: "t.rs".into(),
            line: 1,
            end_line: 1,
            signature: "fn t()".into(),
        }
    }

    fn call(name: &str, line: usize) -> super::super::CallSite {
        super::super::CallSite {
            callee: name.to_string(),
            qualifier: None,
            receiver: None,
            kind: super::super::CallKind::Plain,
            line,
        }
    }

    #[test]
    fn open_creates_schema_and_is_reopenable() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = open_tmp(dir.path());
            assert_eq!(store.symbol_count().unwrap(), 0);
        }
        // Reopening the same path must not recreate or fail.
        let store = open_tmp(dir.path());
        assert_eq!(store.symbol_count().unwrap(), 0);
    }

    #[test]
    fn foreign_schema_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        {
            let _ = Store::open(&db).unwrap();
        }
        // A future or past build wrote a different version — we must
        // refuse instead of misreading its rows.
        let conn = Connection::open(&db).unwrap();
        conn.execute("UPDATE meta SET value = '999'", []).unwrap();
        let err = Store::open(&db).unwrap_err();
        assert!(err.to_string().contains("999"));
    }

    #[test]
    fn replace_file_upserts_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_tmp(dir.path());
        store
            .replace_file(
                "a.rs",
                "h1",
                1,
                "crate::a",
                &[sym("f", "fn"), sym("g", "fn")],
                &[call("q", 2)],
                &Scopes::default(),
                &[],
            )
            .unwrap();
        // Same path, changed content: rows replaced wholesale.
        store
            .replace_file(
                "a.rs",
                "h2",
                2,
                "crate::a",
                &[sym("h", "fn")],
                &[],
                &Scopes::default(),
                &[],
            )
            .unwrap();
        assert_eq!(store.symbol_count().unwrap(), 1);
        let all = store.symbols().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "h");
        assert_eq!(all[0].file, "a.rs");
        // The old edge is gone with the replacement.
        assert!(store.callers("q").unwrap().is_empty());
    }

    #[test]
    fn remove_file_cascades_everything() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_tmp(dir.path());
        store
            .replace_file(
                "a.rs",
                "h1",
                1,
                "crate::a",
                &[sym("f", "fn")],
                &[call("q", 2)],
                &Scopes::default(),
                &[],
            )
            .unwrap();
        store.remove_file("a.rs").unwrap();
        assert_eq!(store.symbol_count().unwrap(), 0);
        assert!(store.file_hashes().unwrap().is_empty());
        assert!(store.all_edges().unwrap().is_empty());
    }

    #[test]
    fn edges_carry_their_resolution_through_queries() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_tmp(dir.path());
        store
            .replace_file(
                "b.rs",
                "hb",
                1,
                "crate::b",
                &[sym("target", "fn")],
                &[],
                &Scopes::default(),
                &[],
            )
            .unwrap();
        store
            .replace_file(
                "a.rs",
                "h1",
                1,
                "crate::a",
                &[],
                &[call("target", 3), call("target", 3), call("other", 4)],
                &Scopes::default(),
                &[],
            )
            .unwrap();
        let b_id = store
            .load_files()
            .unwrap()
            .iter()
            .find(|f| f.module == "crate::b")
            .unwrap()
            .file_id;
        let edges = store.all_edges().unwrap();
        assert_eq!(edges.len(), 3);
        let id = edges[0].id;
        store
            .apply_resolutions(&[(
                id,
                Resolution {
                    tier: resolve::TIER_RESOLVED,
                    target: Some((b_id, 1)),
                    reason: None,
                },
            )])
            .unwrap();
        // Deduplicated per (file, line) — two calls on line 3, one
        // resolved, one still candidate: DISTINCT keeps the tiers apart.
        let callers = store.callers("target").unwrap();
        assert_eq!(callers.len(), 2);
        let resolved = callers
            .iter()
            .find(|c| c.tier == resolve::TIER_RESOLVED)
            .unwrap();
        assert_eq!(resolved.target, Some(("b.rs".into(), 1)));
        assert_eq!(store.edge_tier_counts().unwrap(), (1, 2, 0));
    }

    #[test]
    fn search_is_substring_case_insensitive_definitions_exact() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_tmp(dir.path());
        store
            .replace_file(
                "t.rs",
                "h",
                1,
                "crate::t",
                &[sym("Alpha", "fn"), sym("alphabet", "fn")],
                &[],
                &Scopes::default(),
                &[],
            )
            .unwrap();
        let hits = store.search_symbols("ALP", None, None).unwrap();
        assert_eq!(hits.len(), 2);
        // Glob patterns match the whole name, case-sensitively.
        assert_eq!(store.search_symbols("Al*", None, None).unwrap().len(), 1);
        assert_eq!(store.search_symbols("*pha", None, None).unwrap().len(), 1);
        assert_eq!(store.search_symbols("Al?ha", None, None).unwrap().len(), 1);
        // Case matters for patterns: `alphabet` starts with "al".
        assert_eq!(store.search_symbols("AL*", None, None).unwrap().len(), 0);
        assert_eq!(store.search_symbols("al*", None, None).unwrap().len(), 1);
        // Kind and path filters narrow without touching the match.
        assert_eq!(
            store.search_symbols("alp", Some("fn"), None).unwrap().len(),
            2
        );
        assert_eq!(
            store
                .search_symbols("alp", Some("struct"), None)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            store
                .search_symbols("alp", None, Some("t.rs"))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            store
                .search_symbols("alp", None, Some("nope"))
                .unwrap()
                .len(),
            0
        );
        // Exact lookup is case-sensitive: names are code, not prose.
        assert_eq!(store.definitions("Alpha").unwrap().len(), 1);
        assert!(store.definitions("alpha").unwrap().is_empty());
    }

    #[test]
    fn embeddings_roundtrip_and_prune() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_tmp(dir.path());
        store
            .put_embedding("k1", "bge-m3", &[0.5, -1.0, 2.0])
            .unwrap();
        let (model, vec) = store.embedding("k1").unwrap().unwrap();
        assert_eq!(model, "bge-m3");
        assert_eq!(vec, vec![0.5, -1.0, 2.0]);
        // Overwrite on model switch.
        store.put_embedding("k1", "other", &[1.0]).unwrap();
        assert_eq!(store.embedding("k1").unwrap().unwrap().0, "other");
        store.put_embedding("k2", "bge-m3", &[3.0]).unwrap();
        let keep = std::collections::HashSet::from(["k1".to_string()]);
        assert_eq!(store.prune_embeddings(&keep).unwrap(), 1);
        assert_eq!(store.embedding_count().unwrap(), 1);
        assert!(store.embedding("k2").unwrap().is_none());
    }

    #[test]
    fn extractor_version_is_sticky_per_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_tmp(dir.path());
        // A store fresh from M1 days has no extractor key at all.
        assert!(!store.extractor_is_current());
        store.set_extractor_version(EXTRACTOR_VERSION).unwrap();
        assert!(store.extractor_is_current());
    }
}
