// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Per-app SQLite KV. One owner (the viewer process) so we can use a
//! plain blocking `rusqlite::Connection` without an `Arc<Mutex<>>` —
//! all bridge calls flow through the single-threaded Tauri command
//! dispatcher (or, in the V0 stub binary, the main event loop).

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub struct Storage {
    conn: Connection,
}

impl Storage {
    /// Open or create the storage db inside `<app_dir>/data/storage.db`.
    pub fn open(app_dir: &Path) -> Result<Self, StorageError> {
        let dir = app_dir.join("data");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Err(StorageError::Sqlite(
                rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
            ));
        }
        let path = dir.join("storage.db");
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    /// In-memory storage for tests.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, StorageError> {
        let v = self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        Ok(v)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), StorageError> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO kv (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, now],
        )?;
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.conn
            .execute("DELETE FROM kv WHERE key = ?1", params![key])?;
        Ok(())
    }

    pub fn keys(&self) -> Result<Vec<String>, StorageError> {
        let mut stmt = self.conn.prepare("SELECT key FROM kv ORDER BY key")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn row_count(&self) -> Result<u64, StorageError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip() {
        let s = Storage::in_memory().unwrap();
        assert_eq!(s.get("k").unwrap(), None);
        s.set("k", "v").unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("v"));
        s.set("k", "v2").unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("v2"));
        s.delete("k").unwrap();
        assert_eq!(s.get("k").unwrap(), None);
    }

    #[test]
    fn keys_sorted() {
        let s = Storage::in_memory().unwrap();
        s.set("zebra", "1").unwrap();
        s.set("alpha", "1").unwrap();
        s.set("mango", "1").unwrap();
        let ks = s.keys().unwrap();
        assert_eq!(ks, vec!["alpha", "mango", "zebra"]);
    }

    #[test]
    fn persists_to_disk() {
        let tmp = TempDir::new().unwrap();
        {
            let s = Storage::open(tmp.path()).unwrap();
            s.set("k", "v").unwrap();
            assert_eq!(s.row_count().unwrap(), 1);
        }
        // Reopen the same dir — value must survive.
        let s = Storage::open(tmp.path()).unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("v"));
    }

    #[test]
    fn isolated_per_app_dir() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        Storage::open(a.path()).unwrap().set("key", "A").unwrap();
        Storage::open(b.path()).unwrap().set("key", "B").unwrap();
        assert_eq!(
            Storage::open(a.path())
                .unwrap()
                .get("key")
                .unwrap()
                .as_deref(),
            Some("A")
        );
        assert_eq!(
            Storage::open(b.path())
                .unwrap()
                .get("key")
                .unwrap()
                .as_deref(),
            Some("B")
        );
    }
}
