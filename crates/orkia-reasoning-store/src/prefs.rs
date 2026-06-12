// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Preference signals (the offline sync queue) and consolidated user
//! preferences. The signal queue is **tail-preserving**: when it overflows the
//! cap, the *oldest* rows are dropped so the newest signals always survive —
//! fixing the legacy head-truncation loss.

use chrono::Utc;
use rusqlite::params;
use uuid::Uuid;

use orkia_reasoning_core::dto::{PreferenceDto, SignalDto};
use orkia_reasoning_core::enums::PreferenceScope;

use crate::convert::{enum_from_text, enum_to_text, parse_uuid, ts_text, uuid_text};
use crate::{ReasoningStore, StoreError};

/// Inputs to upsert a consolidated user preference.
pub struct PrefUpsert {
    pub workspace_id: Uuid,
    pub account_id: Uuid,
    pub pref: PreferenceDto,
    pub scope_id: Option<Uuid>,
}

impl ReasoningStore {
    /// Append a preference signal to the offline queue (dirty, awaiting sync).
    pub fn record_signal(&self, s: &SignalDto) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO preference_signal (id, workspace_id, account_id, dimension,
                 direction, strength, source_session_id, source_turn_id, project_id, rfc_id,
                 created_at, dirty)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1)",
            params![
                uuid_text(s.client_event_id),
                uuid_text(s.workspace_id),
                uuid_text(s.account_id),
                enum_to_text(&s.dimension)?,
                enum_to_text(&s.direction)?,
                s.strength,
                uuid_text(s.source_session_id),
                s.source_turn_id.map(uuid_text),
                s.project_id.map(uuid_text),
                s.rfc_ref.as_ref().map(|r| r.rfc_id.as_str().to_string()),
                ts_text(Utc::now()),
            ],
        )?;
        Ok(())
    }

    /// Drop the oldest signals beyond `max`, preserving the newest. Returns the
    /// number deleted.
    pub fn cap_signal_queue(&self, max: usize) -> Result<usize, StoreError> {
        let n = self.conn.execute(
            "DELETE FROM preference_signal WHERE id IN (
                 SELECT id FROM preference_signal ORDER BY created_at DESC, id DESC
                 LIMIT -1 OFFSET ?1
             )",
            params![max as i64],
        )?;
        Ok(n)
    }

    /// The oldest `limit` un-synced signals, ready to push to cloud.
    pub fn dirty_signals(&self, limit: usize) -> Result<Vec<SignalDto>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, workspace_id, account_id, dimension, direction, strength,
                 source_session_id, source_turn_id, project_id
             FROM preference_signal WHERE dirty = 1 ORDER BY created_at ASC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| Ok(build_signal(row)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Remove signals confirmed accepted by the cloud (drains the queue).
    pub fn mark_signals_synced(&self, ids: &[Uuid]) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        for id in ids {
            tx.execute(
                "DELETE FROM preference_signal WHERE id = ?1",
                params![uuid_text(*id)],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Upsert a consolidated preference, keyed by (workspace, dimension, scope).
    pub fn upsert_preference(&self, u: &PrefUpsert) -> Result<(), StoreError> {
        let now = ts_text(Utc::now());
        let dim = enum_to_text(&u.pref.dimension)?;
        let scope = enum_to_text(&u.pref.scope)?;
        let updated = self.conn.execute(
            "UPDATE user_preference SET value = ?4, confidence = ?5, observation_count = ?6,
                 last_seen = ?7, updated_at = ?7
             WHERE workspace_id = ?1 AND dimension = ?2 AND scope_type = ?3",
            params![
                uuid_text(u.workspace_id),
                dim,
                scope,
                u.pref.value,
                u.pref.confidence,
                u.pref.observation_count,
                now,
            ],
        )?;
        if updated == 0 {
            self.insert_preference(u, &dim, &scope, &now)?;
        }
        Ok(())
    }

    fn insert_preference(
        &self,
        u: &PrefUpsert,
        dim: &str,
        scope: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO user_preference (id, workspace_id, account_id, dimension, value,
                 confidence, observation_count, scope_type, scope_id, first_seen, last_seen,
                 updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?10)",
            params![
                uuid_text(Uuid::new_v4()),
                uuid_text(u.workspace_id),
                uuid_text(u.account_id),
                dim,
                u.pref.value,
                u.pref.confidence,
                u.pref.observation_count,
                scope,
                u.scope_id.map(uuid_text),
                now,
            ],
        )?;
        Ok(())
    }

    /// Consolidated preferences for a workspace (for cache warm + prompt inject).
    pub fn preferences_for_workspace(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<PreferenceDto>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT dimension, value, confidence, observation_count, scope_type
             FROM user_preference WHERE workspace_id = ?1 ORDER BY confidence DESC",
        )?;
        let rows = stmt.query_map(params![uuid_text(workspace_id)], |row| Ok(build_pref(row)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }
}

fn build_signal(row: &rusqlite::Row<'_>) -> Result<SignalDto, StoreError> {
    let source_turn_id = row
        .get::<_, Option<String>>(7)?
        .map(|s| parse_uuid(&s))
        .transpose()?;
    let project_id = row
        .get::<_, Option<String>>(8)?
        .map(|s| parse_uuid(&s))
        .transpose()?;
    Ok(SignalDto {
        client_event_id: parse_uuid(&row.get::<_, String>(0)?)?,
        workspace_id: parse_uuid(&row.get::<_, String>(1)?)?,
        account_id: parse_uuid(&row.get::<_, String>(2)?)?,
        dimension: enum_from_text(&row.get::<_, String>(3)?)?,
        direction: enum_from_text(&row.get::<_, String>(4)?)?,
        strength: row.get(5)?,
        source_session_id: parse_uuid(&row.get::<_, String>(6)?)?,
        source_turn_id,
        project_id,
        rfc_ref: None,
    })
}

fn build_pref(row: &rusqlite::Row<'_>) -> Result<PreferenceDto, StoreError> {
    let scope: PreferenceScope = enum_from_text(&row.get::<_, String>(4)?)?;
    Ok(PreferenceDto {
        dimension: enum_from_text(&row.get::<_, String>(0)?)?,
        value: row.get(1)?,
        confidence: row.get(2)?,
        observation_count: row.get(3)?,
        scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_reasoning_core::enums::{Dimension, SignalDirection};

    fn signal(strength: f32) -> SignalDto {
        SignalDto {
            client_event_id: Uuid::new_v4(),
            workspace_id: Uuid::from_u128(1),
            account_id: Uuid::from_u128(2),
            source_session_id: Uuid::from_u128(3),
            source_turn_id: None,
            project_id: None,
            rfc_ref: None,
            dimension: Dimension::Verbosity,
            direction: SignalDirection::Positive,
            strength,
        }
    }

    #[test]
    fn signal_queue_round_trip_and_drain() {
        let s = ReasoningStore::in_memory().unwrap();
        let sig = signal(0.7);
        let id = sig.client_event_id;
        s.record_signal(&sig).unwrap();
        let dirty = s.dirty_signals(10).unwrap();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].dimension, Dimension::Verbosity);
        s.mark_signals_synced(&[id]).unwrap();
        assert_eq!(s.dirty_signals(10).unwrap().len(), 0);
    }

    #[test]
    fn cap_is_tail_preserving() {
        let s = ReasoningStore::in_memory().unwrap();
        // Insert with strictly increasing created_at via sequential timestamps.
        for i in 0..5 {
            let sig = signal(i as f32);
            // Force ordering: overwrite created_at to be monotonic.
            s.record_signal(&sig).unwrap();
            s.conn
                .execute(
                    "UPDATE preference_signal SET created_at = ?2 WHERE id = ?1",
                    params![
                        uuid_text(sig.client_event_id),
                        format!("2026-01-0{}T00:00:00Z", i + 1)
                    ],
                )
                .unwrap();
        }
        // Keep newest 2; drop oldest 3.
        assert_eq!(s.cap_signal_queue(2).unwrap(), 3);
        let remaining = s.dirty_signals(10).unwrap();
        assert_eq!(remaining.len(), 2);
        // The two survivors are the newest (highest strength 3.0 and 4.0).
        let mut strengths: Vec<f32> = remaining.iter().map(|r| r.strength).collect();
        strengths.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(strengths, vec![3.0, 4.0]);
    }

    #[test]
    fn preference_upsert_replaces_in_place() {
        let s = ReasoningStore::in_memory().unwrap();
        let base = PreferenceDto {
            dimension: Dimension::Verbosity,
            value: "verbose".into(),
            confidence: 0.5,
            observation_count: 1,
            scope: PreferenceScope::Workspace,
        };
        s.upsert_preference(&PrefUpsert {
            workspace_id: Uuid::from_u128(1),
            account_id: Uuid::from_u128(2),
            pref: base.clone(),
            scope_id: None,
        })
        .unwrap();
        let updated = PreferenceDto {
            value: "concise".into(),
            confidence: 0.9,
            observation_count: 4,
            ..base
        };
        s.upsert_preference(&PrefUpsert {
            workspace_id: Uuid::from_u128(1),
            account_id: Uuid::from_u128(2),
            pref: updated,
            scope_id: None,
        })
        .unwrap();
        let prefs = s.preferences_for_workspace(Uuid::from_u128(1)).unwrap();
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].value, "concise");
        assert_eq!(prefs[0].observation_count, 4);
    }
}
