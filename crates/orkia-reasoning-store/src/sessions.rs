// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Session rows: create, fetch, status transitions, turn-count bookkeeping.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use orkia_reasoning_core::dto::RfcRef;
use orkia_reasoning_core::enums::SessionStatus;

use crate::convert::{enum_from_text, enum_to_text, parse_ts, parse_uuid, ts_text, uuid_text};
use crate::{ReasoningStore, StoreError};

/// Inputs to open a new reasoning session. Optional project/RFC scoping.
pub struct NewSession {
    pub workspace_id: Uuid,
    pub account_id: Uuid,
    pub agent_name: String,
    pub project_id: Option<Uuid>,
    pub rfc_ref: Option<RfcRef>,
}

/// A session row as stored.
#[derive(Debug, Clone)]
pub struct StoredSession {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub account_id: Uuid,
    pub project_id: Option<Uuid>,
    pub rfc_ref: Option<RfcRef>,
    pub agent_name: String,
    pub status: SessionStatus,
    pub turn_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub dirty: bool,
}

impl ReasoningStore {
    /// Create a session in `Active` status, returning its new id.
    pub fn create_session(&self, s: &NewSession) -> Result<Uuid, StoreError> {
        let id = Uuid::new_v4();
        let now = ts_text(Utc::now());
        let (rfc_id, rfc_section) = split_rfc(s.rfc_ref.as_ref());
        self.conn.execute(
            "INSERT INTO session (id, workspace_id, account_id, project_id, rfc_id, rfc_section,
                 agent_name, status, turn_count, created_at, updated_at, dirty)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?9, 1)",
            params![
                uuid_text(id),
                uuid_text(s.workspace_id),
                uuid_text(s.account_id),
                s.project_id.map(uuid_text),
                rfc_id,
                rfc_section,
                s.agent_name,
                enum_to_text(&SessionStatus::Active)?,
                now,
            ],
        )?;
        Ok(id)
    }

    /// Fetch a session by id.
    pub fn get_session(&self, id: Uuid) -> Result<Option<StoredSession>, StoreError> {
        self.conn
            .query_row(
                "SELECT id, workspace_id, account_id, project_id, rfc_id, rfc_section,
                     agent_name, status, turn_count, created_at, updated_at, dirty
                 FROM session WHERE id = ?1",
                params![uuid_text(id)],
                map_session,
            )
            .optional()?
            .transpose()
    }

    /// Transition a session to a new lifecycle status (marks it dirty).
    pub fn set_session_status(&self, id: Uuid, status: SessionStatus) -> Result<(), StoreError> {
        let completed = matches!(status, SessionStatus::Completed | SessionStatus::Abandoned)
            .then(|| ts_text(Utc::now()));
        self.conn.execute(
            "UPDATE session SET status = ?2, updated_at = ?3, completed_at = ?4, dirty = 1
             WHERE id = ?1",
            params![
                uuid_text(id),
                enum_to_text(&status)?,
                ts_text(Utc::now()),
                completed,
            ],
        )?;
        Ok(())
    }

    /// Increment a session's turn counter and bump `updated_at`.
    pub fn bump_turn_count(&self, id: Uuid) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE session SET turn_count = turn_count + 1, updated_at = ?2, dirty = 1
             WHERE id = ?1",
            params![uuid_text(id), ts_text(Utc::now())],
        )?;
        Ok(())
    }
}

/// Split an optional `RfcRef` into `(rfc_id, rfc_section)` TEXT columns.
pub(crate) fn split_rfc(rfc: Option<&RfcRef>) -> (Option<String>, Option<String>) {
    match rfc {
        None => (None, None),
        Some(r) => (
            Some(r.rfc_id.as_str().to_string()),
            r.section.as_ref().map(|s| s.as_str().to_string()),
        ),
    }
}

/// Rebuild an optional `RfcRef` from `(rfc_id, rfc_section)` TEXT columns.
pub(crate) fn join_rfc(rfc_id: Option<String>, section: Option<String>) -> Option<RfcRef> {
    rfc_id.map(|id| RfcRef {
        rfc_id: orkia_rfc_core::id::RfcId::new(id),
        section: section.map(orkia_rfc_core::id::SectionPath::new),
    })
}

fn map_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<StoredSession, StoreError>> {
    Ok(build_session(row))
}

fn build_session(row: &rusqlite::Row<'_>) -> Result<StoredSession, StoreError> {
    let project_id = row
        .get::<_, Option<String>>(3)?
        .map(|s| parse_uuid(&s))
        .transpose()?;
    Ok(StoredSession {
        id: parse_uuid(&row.get::<_, String>(0)?)?,
        workspace_id: parse_uuid(&row.get::<_, String>(1)?)?,
        account_id: parse_uuid(&row.get::<_, String>(2)?)?,
        project_id,
        rfc_ref: join_rfc(row.get(4)?, row.get(5)?),
        agent_name: row.get(6)?,
        status: enum_from_text(&row.get::<_, String>(7)?)?,
        turn_count: row.get(8)?,
        created_at: parse_ts(&row.get::<_, String>(9)?)?,
        updated_at: parse_ts(&row.get::<_, String>(10)?)?,
        dirty: row.get::<_, i64>(11)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_rfc_core::id::{RfcId, SectionPath};

    fn store() -> ReasoningStore {
        ReasoningStore::in_memory().unwrap()
    }

    fn new_session() -> NewSession {
        NewSession {
            workspace_id: Uuid::from_u128(1),
            account_id: Uuid::from_u128(2),
            agent_name: "faye".into(),
            project_id: Some(Uuid::from_u128(3)),
            rfc_ref: Some(RfcRef::with_section(
                RfcId::new("rfc-7"),
                SectionPath::new("goals"),
            )),
        }
    }

    #[test]
    fn create_then_get_round_trips_scoping() {
        let s = store();
        let id = s.create_session(&new_session()).unwrap();
        let got = s.get_session(id).unwrap().unwrap();
        assert_eq!(got.status, SessionStatus::Active);
        assert_eq!(got.project_id, Some(Uuid::from_u128(3)));
        let rfc = got.rfc_ref.unwrap();
        assert_eq!(rfc.rfc_id.as_str(), "rfc-7");
        assert_eq!(rfc.section.unwrap().as_str(), "goals");
    }

    #[test]
    fn status_and_turn_count_update() {
        let s = store();
        let id = s.create_session(&new_session()).unwrap();
        s.bump_turn_count(id).unwrap();
        s.bump_turn_count(id).unwrap();
        s.set_session_status(id, SessionStatus::Completed).unwrap();
        let got = s.get_session(id).unwrap().unwrap();
        assert_eq!(got.turn_count, 2);
        assert_eq!(got.status, SessionStatus::Completed);
    }

    #[test]
    fn get_missing_session_is_none() {
        assert!(store().get_session(Uuid::from_u128(99)).unwrap().is_none());
    }
}
