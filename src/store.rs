//! SQLite storage for the index (`.codeindex/index.db`).
//!
//! M1 schema: `meta(schema_version)`, `files(path, hash, mtime)`,
//! `symbols(file_id, name, kind, line)`. The `edges` table waits for M2/M3:
//! it should not exist before call-site extraction defines its columns —
//! a table we cannot honestly fill is noise.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

pub const SCHEMA_VERSION: &str = "1";

/// One indexed symbol joined with its file — the minimal read API M2
/// queries grow from.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedSymbol {
    /// Path relative to the indexed root.
    pub file: String,
    pub name: String,
    pub kind: String,
    pub line: i64,
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
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;
        // WAL survives a crashed run; foreign keys make symbol cleanup
        // ride on file deletion.
        let _mode: String =
            conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS files (
                 id INTEGER PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE,
                 hash TEXT NOT NULL,
                 mtime INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS symbols (
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 name TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 line INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
             CREATE INDEX IF NOT EXISTS idx_symbols_file_id ON symbols(file_id);",
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

    /// Upsert one file and replace its symbol rows wholesale. Per-file
    /// wholesale replacement keeps the parse-and-store step free of
    /// line-level diffing — the hash already decided the file changed.
    pub fn replace_file(
        &self,
        path: &str,
        hash: &str,
        mtime: i64,
        symbols: &[super::Symbol],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO files(path, hash, mtime) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET hash = excluded.hash, mtime = excluded.mtime",
            params![path, hash, mtime],
        )?;
        let file_id: i64 =
            tx.query_row("SELECT id FROM files WHERE path = ?1", params![path], |r| {
                r.get(0)
            })?;
        tx.execute("DELETE FROM symbols WHERE file_id = ?1", params![file_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO symbols(file_id, name, kind, line) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for s in symbols {
                stmt.execute(params![file_id, s.name, s.kind, s.line as i64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop a file and (via cascade) its symbols.
    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn symbol_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_tmp(dir: &Path) -> Store {
        Store::open(&dir.join("index.db")).unwrap()
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
        let syms = |names: &[&str]| -> Vec<super::super::Symbol> {
            names
                .iter()
                .map(|n| super::super::Symbol {
                    name: n.to_string(),
                    kind: "fn",
                    file: "t.rs".into(),
                    line: 1,
                })
                .collect()
        };
        store.replace_file("a.rs", "h1", 1, &syms(&["f", "g"])).unwrap();
        // Same path, changed content: row count must stay 1, symbols
        // replaced wholesale.
        store.replace_file("a.rs", "h2", 2, &syms(&["h"])).unwrap();
        assert_eq!(store.symbol_count().unwrap(), 1);
        let all = store.symbols().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "h");
        assert_eq!(all[0].file, "a.rs");
    }

    #[test]
    fn remove_file_cascades_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_tmp(dir.path());
        let sym = super::super::Symbol {
            name: "f".into(),
            kind: "fn",
            file: "a.rs".into(),
            line: 1,
        };
        store.replace_file("a.rs", "h1", 1, &[sym]).unwrap();
        store.remove_file("a.rs").unwrap();
        assert_eq!(store.symbol_count().unwrap(), 0);
        assert!(store.file_hashes().unwrap().is_empty());
    }
}
