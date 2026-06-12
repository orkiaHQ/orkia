// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! SQLite schema. Mirrors the cloud Postgres tables 1:1 so a row
//! syncs without reshaping. Closed-domain columns hold the enum's serde
//! discriminant as TEXT. UUIDs are hyphenated TEXT; timestamps are
//! RFC3339 TEXT (lexicographically sortable in UTC).

/// Full DDL, run once at open under `CREATE TABLE IF NOT EXISTS`.
pub(crate) const SCHEMA_SQL: &str = "
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS session (
    id                   TEXT PRIMARY KEY,
    workspace_id         TEXT NOT NULL,
    account_id           TEXT NOT NULL,
    project_id           TEXT,
    rfc_id               TEXT,
    rfc_section          TEXT,
    agent_name           TEXT NOT NULL,
    status               TEXT NOT NULL,
    turn_count           INTEGER NOT NULL DEFAULT 0,
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL,
    completed_at         TEXT,
    last_consolidated_at TEXT,
    dirty                INTEGER NOT NULL DEFAULT 1,
    synced_at            TEXT
);

CREATE TABLE IF NOT EXISTS turn (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    seq             INTEGER NOT NULL,
    role            TEXT NOT NULL,
    turn_type       TEXT NOT NULL,
    summary         TEXT,
    content_hash    TEXT NOT NULL,
    token_count     INTEGER,
    metadata        TEXT,
    thinking_trace  TEXT,
    thinking_tokens INTEGER,
    parent_turn_id  TEXT,
    relation_type   TEXT,
    project_id      TEXT,
    rfc_id          TEXT,
    rfc_section     TEXT,
    occurred_at     TEXT NOT NULL,
    dirty           INTEGER NOT NULL DEFAULT 1,
    synced_at       TEXT,
    FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS knowledge_node (
    id                TEXT PRIMARY KEY,
    workspace_id      TEXT NOT NULL,
    project_id        TEXT,
    rfc_id            TEXT,
    node_type         TEXT NOT NULL,
    summary           TEXT NOT NULL,
    details           TEXT,
    confidence        REAL NOT NULL,
    origin            TEXT NOT NULL,
    source_turn_id    TEXT,
    source_session_id TEXT,
    seal_id           TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    superseded_at     TEXT,
    -- KG product fields. Nullable/defaulted so a
    -- pre-KG cache opens clean and backfills lazily (see MIGRATIONS_SQL).
    domain            TEXT,                               -- e.g. pty, seal, sync
    context_block     TEXT,                               -- materialized compile_context_block(); derivable, not authoritative
    last_accessed     TEXT,                               -- RFC3339; bumped on L2/L3 read
    access_count      INTEGER NOT NULL DEFAULT 0,
    status            TEXT NOT NULL DEFAULT 'active',     -- active|superseded|archived
    source_type       TEXT NOT NULL DEFAULT 'session',    -- session|pr|adr|doc|slack|manual
    source_ref        TEXT                                -- PR url, ADR id, doc path, message link
);

CREATE TABLE IF NOT EXISTS preference_signal (
    id                TEXT PRIMARY KEY,
    workspace_id      TEXT NOT NULL,
    account_id        TEXT NOT NULL,
    dimension         TEXT NOT NULL,
    direction         TEXT NOT NULL,
    strength          REAL NOT NULL,
    source_session_id TEXT NOT NULL,
    source_turn_id    TEXT,
    project_id        TEXT,
    rfc_id            TEXT,
    created_at        TEXT NOT NULL,
    dirty             INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS user_preference (
    id                TEXT PRIMARY KEY,
    workspace_id      TEXT NOT NULL,
    account_id        TEXT NOT NULL,
    dimension         TEXT NOT NULL,
    value             TEXT NOT NULL,
    confidence        REAL NOT NULL,
    observation_count INTEGER NOT NULL,
    scope_type        TEXT NOT NULL,
    scope_id          TEXT,
    first_seen        TEXT NOT NULL,
    last_seen         TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_turn_session   ON turn(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_turn_project   ON turn(project_id);
CREATE INDEX IF NOT EXISTS idx_turn_rfc       ON turn(rfc_id);
CREATE INDEX IF NOT EXISTS idx_turn_dirty     ON turn(dirty);
CREATE INDEX IF NOT EXISTS idx_node_project   ON knowledge_node(project_id);
CREATE INDEX IF NOT EXISTS idx_node_rfc       ON knowledge_node(rfc_id);
CREATE INDEX IF NOT EXISTS idx_signal_dirty   ON preference_signal(dirty, created_at);
CREATE INDEX IF NOT EXISTS idx_pref_ws        ON user_preference(workspace_id, dimension);
";

///
/// `CREATE TABLE IF NOT EXISTS` does not alter an existing table, so a cache
/// that predates the KG fields needs these `ALTER`s. Each is idempotent at the
/// call site: re-adding an existing column raises a "duplicate column name"
/// error, which the runner swallows (see `ReasoningStore::migrate`). On a fresh
/// DB the columns already exist from `SCHEMA_SQL`, so every statement here is a
/// harmless no-op. Order does not matter — each column is independent.
pub(crate) const MIGRATIONS_SQL: &[&str] = &[
    "ALTER TABLE knowledge_node ADD COLUMN domain TEXT",
    "ALTER TABLE knowledge_node ADD COLUMN context_block TEXT",
    "ALTER TABLE knowledge_node ADD COLUMN last_accessed TEXT",
    "ALTER TABLE knowledge_node ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE knowledge_node ADD COLUMN status TEXT NOT NULL DEFAULT 'active'",
    "ALTER TABLE knowledge_node ADD COLUMN source_type TEXT NOT NULL DEFAULT 'session'",
    "ALTER TABLE knowledge_node ADD COLUMN source_ref TEXT",
];
