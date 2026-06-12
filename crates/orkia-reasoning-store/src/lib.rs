// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.
#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Local SQLite store for the reasoning graph.
//!
//! One owner: the `ReasoningStore` wraps a single blocking
//! `rusqlite::Connection` and is owned by the reasoning consumer thread, so
//! there is no `Arc<Mutex>` on the data path (CLAUDE.md rule #2). The schema
//! mirrors the cloud Postgres tables 1:1 and stores every closed domain as the
//! enum's serde discriminant.

use std::path::Path;

use rusqlite::Connection;

mod convert;
mod nodes;
mod prefs;
mod schema;
mod sessions;
mod stats;
mod turns;

pub use nodes::{KnowledgeNodeSearchHit, NodeInsert};
pub use prefs::PrefUpsert;
pub use sessions::{NewSession, StoredSession};
pub use stats::StoreStats;
pub use turns::{StoredTurn, TurnInsert};

/// Errors raised by the local store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Underlying SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A (de)serialization of a closed-domain enum failed.
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    /// A stored value violated an invariant on read (bad uuid/timestamp/enum).
    /// Surfaced rather than panicked — the caller skips the row (fail closed).
    #[error("corrupt row: {0}")]
    Corrupt(String),
}

/// The local reasoning store. Single-owner, blocking.
pub struct ReasoningStore {
    conn: Connection,
}

impl ReasoningStore {
    /// Open (creating if absent) the store at `path`, applying the schema.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// In-memory store (tests and ephemeral sessions).
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    #[cfg(test)]
    fn migrate_for_test(conn: &Connection) -> Result<(), StoreError> {
        Self::migrate(conn)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        // The hot-path consumer and the sync worker each own a separate
        // connection to the same file (CLAUDE.md #2: one owner per connection).
        // WAL lets a reader and a writer proceed concurrently; the busy timeout
        // makes a momentary writer-writer overlap wait briefly instead of
        // failing with SQLITE_BUSY. Harmless for the in-memory test store.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.execute_batch(schema::SCHEMA_SQL)?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// a `duplicate column name` error means the column already exists (fresh DB
    /// or prior run), which is success — every other SQLite error propagates.
    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        for stmt in schema::MIGRATIONS_SQL {
            match conn.execute(stmt, []) {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                    if msg.contains("duplicate column name") => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    const OLD_NODE_TABLE: &str = "CREATE TABLE knowledge_node (
        id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, project_id TEXT, rfc_id TEXT,
        node_type TEXT NOT NULL, summary TEXT NOT NULL, details TEXT, confidence REAL NOT NULL,
        origin TEXT NOT NULL, source_turn_id TEXT, source_session_id TEXT, seal_id TEXT,
        created_at TEXT NOT NULL, updated_at TEXT NOT NULL, superseded_at TEXT)";

    fn columns(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("PRAGMA table_info(knowledge_node)").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn migrate_adds_kg_columns_to_old_cache_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(OLD_NODE_TABLE).unwrap();
        assert!(!columns(&conn).contains(&"context_block".to_string()));

        // First migration adds every KG field.
        ReasoningStore::migrate_for_test(&conn).unwrap();
        let cols = columns(&conn);
        for expected in [
            "domain",
            "context_block",
            "last_accessed",
            "access_count",
            "status",
            "source_type",
            "source_ref",
        ] {
            assert!(cols.contains(&expected.to_string()), "missing {expected}");
        }

        // Re-running must not error (duplicate-column is swallowed).
        ReasoningStore::migrate_for_test(&conn).unwrap();

        // Defaulted NOT NULL columns are usable immediately on an old row.
        conn.execute(
            "INSERT INTO knowledge_node
                (id, workspace_id, node_type, summary, confidence, origin, created_at, updated_at)
             VALUES ('n1','w1','decision','s',0.9,'cloud','t','t')",
            [],
        )
        .unwrap();
        let (status, access): (String, i64) = conn
            .query_row(
                "SELECT status, access_count FROM knowledge_node WHERE id='n1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "active");
        assert_eq!(access, 0);
    }
}
