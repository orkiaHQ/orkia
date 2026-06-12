// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Assertions over Postgres state via raw sqlx (independent of the
//! server's sea-orm ORM by design — the harness must not be coupled
//! to the server's storage choices).
//!
//! Every assertion is scoped to the e2e test workspace
//! ([`crate::fixtures::TEST_WORKSPACE_ID`]) so flows cannot leak rows
//! into each other across the same compose stack.

use sqlx::{PgPool, Row};

use crate::error::{AssertKind, HarnessError};
use crate::fixtures::TEST_WORKSPACE_ID;

pub struct BackendAssert<'a> {
    pool: &'a PgPool,
}

impl<'a> BackendAssert<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Assert row count in `<table>` scoped to the test workspace.
    /// Requires the table to have a `workspace_id` column.
    pub async fn count_in(self, table: &str, expected: i64) -> crate::Result<()> {
        validate_ident(table)?;
        let sql = format!("SELECT count(*) FROM {table} WHERE workspace_id = $1::uuid");
        let row = sqlx::query(&sql)
            .bind(TEST_WORKSPACE_ID)
            .fetch_one(self.pool)
            .await?;
        let got: i64 = row.try_get(0)?;
        if got != expected {
            let state = self.dump_table_snapshot(table, 8).await;
            return Err(HarnessError::assertion(
                format!("count_in({table}): expected {expected}, got {got}"),
                AssertKind::Backend,
                state,
            ));
        }
        Ok(())
    }

    /// Assert a row matching the WHERE clause exists in `<table>`,
    /// scoped to the test workspace. The clause is appended after
    /// `workspace_id = '...' AND ` so flow code only writes the
    /// row-specific predicate.
    ///
    /// # Security contract
    /// `where_clause` MUST be a **static literal** in test flow code.
    /// It is interpolated directly into the SQL string without escaping.
    /// Never build `where_clause` from PTY output, agent output, or any
    /// other runtime-controlled value.
    pub async fn row_exists(self, table: &str, where_clause: &str) -> crate::Result<()> {
        validate_ident(table)?;
        let sql = format!(
            "SELECT 1 FROM {table} WHERE workspace_id = $1::uuid AND ({where_clause}) LIMIT 1"
        );
        let row = sqlx::query(&sql)
            .bind(TEST_WORKSPACE_ID)
            .fetch_optional(self.pool)
            .await?;
        if row.is_none() {
            let state = self.dump_table_snapshot(table, 8).await;
            return Err(HarnessError::assertion(
                format!("row_exists({table}, {where_clause}): no matching row"),
                AssertKind::Backend,
                state,
            ));
        }
        Ok(())
    }

    /// Execute an arbitrary `SELECT`, expect exactly one row, compare
    /// the first column (as JSON) to `expected`.
    ///
    /// # Security contract
    /// `query` MUST be a **static literal** in test flow code. It is
    /// executed without parameterization. Never pass a query that is
    /// constructed from runtime-controlled input.
    pub async fn query_returns(
        self,
        query: &str,
        expected: serde_json::Value,
    ) -> crate::Result<()> {
        let row = sqlx::query(query).fetch_one(self.pool).await?;
        let got: serde_json::Value = row.try_get(0)?;
        if got != expected {
            return Err(HarnessError::assertion(
                format!("query_returns: expected {expected}, got {got}"),
                AssertKind::Backend,
                format!("--- query ---\n{query}\n--- got ---\n{got}"),
            ));
        }
        Ok(())
    }

    /// Capture-on-fail helper: dump first N row ids (text form) and
    /// the count for `<table>` scoped to the test workspace. Identifier
    /// already validated by the caller. Best-effort — returns a textual
    /// error on failure.
    async fn dump_table_snapshot(&self, table: &str, limit: usize) -> String {
        let limit = limit.clamp(1, 100);
        let sql = format!(
            "SELECT id::text FROM {table} WHERE workspace_id = $1::uuid ORDER BY id LIMIT {limit}"
        );
        let rows = match sqlx::query(&sql)
            .bind(TEST_WORKSPACE_ID)
            .fetch_all(self.pool)
            .await
        {
            Ok(r) => r,
            Err(e) => return format!("--- snapshot of {table} failed: {e} ---"),
        };
        let mut out = format!(
            "--- {table} (workspace={TEST_WORKSPACE_ID}, showing {}) ---\n",
            rows.len()
        );
        for row in &rows {
            let id: Result<String, _> = row.try_get(0);
            match id {
                Ok(s) => out.push_str(&format!("  {s}\n")),
                Err(e) => out.push_str(&format!("  <decode error: {e}>\n")),
            }
        }
        out
    }
}

/// Reject anything that isn't a bare identifier so flow code can't
/// smuggle a SQL fragment in via the `table` arg. Underscores allowed.
fn validate_ident(s: &str) -> crate::Result<()> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(HarnessError::assertion(
            format!("invalid table identifier: {s:?}"),
            AssertKind::Backend,
            String::new(),
        ));
    }
    Ok(())
}
