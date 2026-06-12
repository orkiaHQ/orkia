// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Turn rows: insert, query (by session / project / rfc), and the GC pass that
//! drops summaries/thinking from long-consolidated turns while keeping the
//! content hash and graph links.

use chrono::{DateTime, Duration, Utc};
use rusqlite::params;
use uuid::Uuid;

use orkia_reasoning_core::dto::{RfcRef, TurnDto};
use orkia_reasoning_core::enums::{TurnKind, TurnRelation, TurnRole};

use crate::convert::{enum_from_text, enum_to_text, parse_ts, parse_uuid, ts_text, uuid_text};
use crate::sessions::{join_rfc, split_rfc};
use crate::{ReasoningStore, StoreError};

/// A turn to insert. The DTO carries the wire fields; `seq` and the optional
/// thinking trace are local-only.
pub struct TurnInsert<'a> {
    pub dto: &'a TurnDto,
    pub seq: i64,
    pub thinking_trace: Option<String>,
    pub thinking_tokens: Option<i32>,
}

/// A turn row as stored.
#[derive(Debug, Clone)]
pub struct StoredTurn {
    pub id: Uuid,
    pub session_id: Uuid,
    pub seq: i64,
    pub role: TurnRole,
    pub kind: TurnKind,
    pub summary: Option<String>,
    pub content_hash: String,
    pub parent_turn_id: Option<Uuid>,
    pub relation: Option<TurnRelation>,
    pub project_id: Option<Uuid>,
    pub rfc_ref: Option<RfcRef>,
    pub occurred_at: DateTime<Utc>,
    pub dirty: bool,
}

impl ReasoningStore {
    /// Insert a turn. Its id is the DTO's `client_event_id` (idempotency key);
    /// re-inserting the same event is a no-op via `INSERT OR IGNORE`.
    pub fn insert_turn(&self, t: &TurnInsert<'_>) -> Result<Uuid, StoreError> {
        let d = t.dto;
        let session_id = d
            .session_id
            .ok_or_else(|| StoreError::Corrupt("turn has no session_id".into()))?;
        let (rfc_id, rfc_section) = split_rfc(d.rfc_ref.as_ref());
        let metadata = (!d.metadata.is_null()).then(|| d.metadata.to_string());
        self.conn.execute(
            "INSERT OR IGNORE INTO turn (id, session_id, seq, role, turn_type, summary,
                 content_hash, token_count, metadata, thinking_trace, thinking_tokens,
                 parent_turn_id, relation_type, project_id, rfc_id, rfc_section, occurred_at, dirty)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, 1)",
            params![
                uuid_text(d.client_event_id),
                uuid_text(session_id),
                t.seq,
                enum_to_text(&d.role)?,
                enum_to_text(&d.kind)?,
                d.summary,
                d.content_hash,
                d.token_count,
                metadata,
                t.thinking_trace,
                t.thinking_tokens,
                d.parent_turn_id.map(uuid_text),
                d.relation.map(|r| enum_to_text(&r)).transpose()?,
                d.project_id.map(uuid_text),
                rfc_id,
                rfc_section,
                ts_text(d.occurred_at),
            ],
        )?;
        Ok(d.client_event_id)
    }

    /// All turns in a session, ordered by sequence.
    pub fn turns_for_session(&self, session_id: Uuid) -> Result<Vec<StoredTurn>, StoreError> {
        self.query_turns("session_id = ?1 ORDER BY seq", uuid_text(session_id))
    }

    /// All turns attributed to a project.
    pub fn turns_for_project(&self, project_id: Uuid) -> Result<Vec<StoredTurn>, StoreError> {
        self.query_turns(
            "project_id = ?1 ORDER BY occurred_at",
            uuid_text(project_id),
        )
    }

    /// All turns attributed to an RFC.
    pub fn turns_for_rfc(&self, rfc_id: &str) -> Result<Vec<StoredTurn>, StoreError> {
        self.query_turns("rfc_id = ?1 ORDER BY occurred_at", rfc_id.to_string())
    }

    fn query_turns(&self, where_clause: &str, key: String) -> Result<Vec<StoredTurn>, StoreError> {
        let sql = format!(
            "SELECT id, session_id, seq, role, turn_type, summary, content_hash,
                 parent_turn_id, relation_type, project_id, rfc_id, rfc_section, occurred_at, dirty
             FROM turn WHERE {where_clause}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![key], |row| Ok(build_turn(row)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// The oldest `limit` un-synced turns, rebuilt as wire DTOs ready to push to
    /// cloud (joins `session` for the workspace id + agent name the turn row does
    /// not itself carry). The `client_event_id` is the turn's own id, so a replay
    /// dedupes server-side.
    pub fn dirty_turn_dtos(&self, limit: usize) -> Result<Vec<TurnDto>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.session_id, s.workspace_id, t.project_id, t.rfc_id, t.rfc_section,
                 s.agent_name, t.role, t.turn_type, t.summary, t.content_hash, t.token_count,
                 t.metadata, t.parent_turn_id, t.relation_type, t.occurred_at
             FROM turn t JOIN session s ON s.id = t.session_id
             WHERE t.dirty = 1 ORDER BY t.occurred_at ASC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| Ok(build_dirty_dto(row)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Clear the dirty flag on turns the cloud confirmed accepted, stamping
    /// `synced_at`. Idempotent; unknown ids are ignored.
    pub fn mark_turns_synced(&self, ids: &[Uuid]) -> Result<(), StoreError> {
        let now = ts_text(Utc::now());
        let tx = self.conn.unchecked_transaction()?;
        for id in ids {
            tx.execute(
                "UPDATE turn SET dirty = 0, synced_at = ?2 WHERE id = ?1",
                params![uuid_text(*id), now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// GC: for turns older than `max_age` whose session has been consolidated,
    /// drop the heavy text (summary, thinking) but keep the hash + links.
    /// Returns the number of rows trimmed.
    pub fn gc_consolidated_turns(&self, max_age: Duration) -> Result<usize, StoreError> {
        let cutoff = ts_text(Utc::now() - max_age);
        let n = self.conn.execute(
            "UPDATE turn SET summary = NULL, thinking_trace = NULL, thinking_tokens = NULL
             WHERE occurred_at < ?1
               AND summary IS NOT NULL
               AND session_id IN (SELECT id FROM session WHERE last_consolidated_at IS NOT NULL)",
            params![cutoff],
        )?;
        Ok(n)
    }
}

fn build_turn(row: &rusqlite::Row<'_>) -> Result<StoredTurn, StoreError> {
    let parent = row
        .get::<_, Option<String>>(7)?
        .map(|s| parse_uuid(&s))
        .transpose()?;
    let project_id = row
        .get::<_, Option<String>>(9)?
        .map(|s| parse_uuid(&s))
        .transpose()?;
    let relation = row
        .get::<_, Option<String>>(8)?
        .map(|s| enum_from_text(&s))
        .transpose()?;
    Ok(StoredTurn {
        id: parse_uuid(&row.get::<_, String>(0)?)?,
        session_id: parse_uuid(&row.get::<_, String>(1)?)?,
        seq: row.get(2)?,
        role: enum_from_text(&row.get::<_, String>(3)?)?,
        kind: enum_from_text(&row.get::<_, String>(4)?)?,
        summary: row.get(5)?,
        content_hash: row.get(6)?,
        parent_turn_id: parent,
        relation,
        project_id,
        rfc_ref: join_rfc(row.get(10)?, row.get(11)?),
        occurred_at: parse_ts(&row.get::<_, String>(12)?)?,
        dirty: row.get::<_, i64>(13)? != 0,
    })
}

fn build_dirty_dto(row: &rusqlite::Row<'_>) -> Result<TurnDto, StoreError> {
    let project_id = row
        .get::<_, Option<String>>(3)?
        .map(|s| parse_uuid(&s))
        .transpose()?;
    let parent_turn_id = row
        .get::<_, Option<String>>(13)?
        .map(|s| parse_uuid(&s))
        .transpose()?;
    let relation = row
        .get::<_, Option<String>>(14)?
        .map(|s| enum_from_text(&s))
        .transpose()?;
    let metadata = match row.get::<_, Option<String>>(12)? {
        Some(s) => serde_json::from_str(&s)?,
        None => serde_json::Value::Null,
    };
    Ok(TurnDto {
        client_event_id: parse_uuid(&row.get::<_, String>(0)?)?,
        session_id: Some(parse_uuid(&row.get::<_, String>(1)?)?),
        workspace_id: parse_uuid(&row.get::<_, String>(2)?)?,
        project_id,
        rfc_ref: join_rfc(row.get(4)?, row.get(5)?),
        agent_name: row.get(6)?,
        role: enum_from_text(&row.get::<_, String>(7)?)?,
        kind: enum_from_text(&row.get::<_, String>(8)?)?,
        summary: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
        content_hash: row.get(10)?,
        token_count: row.get(11)?,
        metadata,
        parent_turn_id,
        relation,
        occurred_at: parse_ts(&row.get::<_, String>(15)?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::NewSession;
    use orkia_rfc_core::id::RfcId;

    fn store_with_session() -> (ReasoningStore, Uuid) {
        let s = ReasoningStore::in_memory().unwrap();
        let id = s
            .create_session(&NewSession {
                workspace_id: Uuid::from_u128(1),
                account_id: Uuid::from_u128(2),
                agent_name: "faye".into(),
                project_id: Some(Uuid::from_u128(3)),
                rfc_ref: Some(RfcRef::new(RfcId::new("rfc-9"))),
            })
            .unwrap();
        (s, id)
    }

    fn dto(session: Uuid, kind: TurnKind) -> TurnDto {
        TurnDto {
            client_event_id: Uuid::new_v4(),
            session_id: Some(session),
            workspace_id: Uuid::from_u128(1),
            project_id: Some(Uuid::from_u128(3)),
            rfc_ref: Some(RfcRef::new(RfcId::new("rfc-9"))),
            agent_name: "faye".into(),
            role: TurnRole::Agent,
            kind,
            summary: "did a thing".into(),
            content_hash: "h".into(),
            token_count: Some(5),
            metadata: serde_json::Value::Null,
            parent_turn_id: None,
            relation: Some(TurnRelation::FollowUp),
            occurred_at: Utc::now(),
        }
    }

    #[test]
    fn insert_and_query_by_session_project_rfc() {
        let (s, sid) = store_with_session();
        let d = dto(sid, TurnKind::ToolCall("Bash".into()));
        s.insert_turn(&TurnInsert {
            dto: &d,
            seq: 1,
            thinking_trace: None,
            thinking_tokens: None,
        })
        .unwrap();
        assert_eq!(s.turns_for_session(sid).unwrap().len(), 1);
        assert_eq!(s.turns_for_project(Uuid::from_u128(3)).unwrap().len(), 1);
        let by_rfc = s.turns_for_rfc("rfc-9").unwrap();
        assert_eq!(by_rfc.len(), 1);
        assert_eq!(by_rfc[0].kind, TurnKind::ToolCall("Bash".into()));
        assert_eq!(by_rfc[0].relation, Some(TurnRelation::FollowUp));
    }

    #[test]
    fn insert_is_idempotent_on_client_event_id() {
        let (s, sid) = store_with_session();
        let d = dto(sid, TurnKind::UserPrompt);
        let ins = TurnInsert {
            dto: &d,
            seq: 1,
            thinking_trace: None,
            thinking_tokens: None,
        };
        s.insert_turn(&ins).unwrap();
        s.insert_turn(&ins).unwrap();
        assert_eq!(s.turns_for_session(sid).unwrap().len(), 1);
    }

    #[test]
    fn dirty_turn_dtos_join_session_and_drain_on_sync() {
        let (s, sid) = store_with_session();
        let d = dto(sid, TurnKind::ToolCall("Read".into()));
        let id = d.client_event_id;
        s.insert_turn(&TurnInsert {
            dto: &d,
            seq: 1,
            thinking_trace: None,
            thinking_tokens: None,
        })
        .unwrap();
        let dtos = s.dirty_turn_dtos(10).unwrap();
        assert_eq!(dtos.len(), 1);
        // workspace_id + agent_name are reconstructed from the session join.
        assert_eq!(dtos[0].workspace_id, Uuid::from_u128(1));
        assert_eq!(dtos[0].agent_name, "faye");
        assert_eq!(dtos[0].kind, TurnKind::ToolCall("Read".into()));
        assert_eq!(dtos[0].project_id, Some(Uuid::from_u128(3)));
        // Once marked synced the row drops out of the dirty drain.
        s.mark_turns_synced(&[id]).unwrap();
        assert!(s.dirty_turn_dtos(10).unwrap().is_empty());
    }

    #[test]
    fn dirty_drain_is_bounded_by_batch_limit() {
        // Cost guardrail: a tool-call storm must never push more than the
        // worker's BATCH in one request, no matter how many rows are dirty.
        let (s, sid) = store_with_session();
        for i in 0..50 {
            let d = dto(sid, TurnKind::ToolCall(format!("t{i}")));
            s.insert_turn(&TurnInsert {
                dto: &d,
                seq: i + 1,
                thinking_trace: None,
                thinking_tokens: None,
            })
            .unwrap();
        }
        // The store honors the cap exactly; the worker passes BATCH here.
        assert_eq!(s.dirty_turn_dtos(10).unwrap().len(), 10);
        assert_eq!(s.dirty_turn_dtos(200).unwrap().len(), 50);
    }

    #[test]
    fn gc_trims_only_consolidated_old_turns() {
        let (s, sid) = store_with_session();
        let mut d = dto(sid, TurnKind::AgentOutput);
        d.occurred_at = Utc::now() - Duration::days(40);
        s.insert_turn(&TurnInsert {
            dto: &d,
            seq: 1,
            thinking_trace: Some("deep".into()),
            thinking_tokens: Some(10),
        })
        .unwrap();
        // Not consolidated yet → GC leaves it.
        assert_eq!(s.gc_consolidated_turns(Duration::days(30)).unwrap(), 0);
        s.conn
            .execute(
                "UPDATE session SET last_consolidated_at = ?2 WHERE id = ?1",
                params![uuid_text(sid), ts_text(Utc::now())],
            )
            .unwrap();
        assert_eq!(s.gc_consolidated_turns(Duration::days(30)).unwrap(), 1);
        assert!(s.turns_for_session(sid).unwrap()[0].summary.is_none());
    }
}
