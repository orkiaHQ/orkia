// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Knowledge-node rows: upsert (cold-pass output), query by project/rfc,
//! supersede, and prune of long-superseded nodes.

use chrono::{Duration, Utc};
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use orkia_reasoning_core::compile_context_block;
use orkia_reasoning_core::dto::KnowledgeNode;

use crate::convert::{enum_from_text, enum_to_text, parse_ts, parse_uuid, ts_text, uuid_text};
use crate::sessions::{join_rfc, split_rfc};
use crate::{ReasoningStore, StoreError};

#[derive(Clone, Debug)]
pub struct KnowledgeNodeSearchHit {
    pub node: KnowledgeNode,
    pub domain: Option<String>,
    pub context_block: Option<String>,
    pub seal_id: Option<String>,
    pub agent_name: Option<String>,
    pub source_turn_id: Option<Uuid>,
    pub source_session_id: Option<Uuid>,
}

/// Extra provenance fields stored alongside a [`KnowledgeNode`] but not part of
/// the wire DTO.
pub struct NodeInsert<'a> {
    pub node: &'a KnowledgeNode,
    pub details: Option<String>,
    pub domain: Option<&'a str>,
    pub context_block: Option<String>,
    pub source_turn_id: Option<Uuid>,
    pub source_session_id: Option<Uuid>,
    pub seal_id: Option<String>,
}

impl ReasoningStore {
    /// Insert or replace a knowledge node by id.
    pub fn upsert_node(&self, n: &NodeInsert<'_>) -> Result<(), StoreError> {
        let k = n.node;
        let (rfc_id, _section) = split_rfc(k.rfc_ref.as_ref());
        let now = ts_text(Utc::now());
        let context_block = n
            .context_block
            .clone()
            .unwrap_or_else(|| compile_context_block(k));
        self.conn.execute(
            "INSERT INTO knowledge_node (id, workspace_id, project_id, rfc_id, node_type, summary,
                 details, domain, context_block, confidence, origin, source_turn_id, source_session_id, seal_id,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15)
             ON CONFLICT(id) DO UPDATE SET
                 summary = excluded.summary, details = excluded.details,
                 domain = excluded.domain, context_block = excluded.context_block,
                 confidence = excluded.confidence, updated_at = excluded.updated_at",
            params![
                uuid_text(k.id),
                uuid_text(k.workspace_id),
                k.project_id.map(uuid_text),
                rfc_id,
                enum_to_text(&k.kind)?,
                k.summary,
                n.details,
                n.domain,
                context_block,
                k.confidence,
                enum_to_text(&k.origin)?,
                n.source_turn_id.map(uuid_text),
                n.source_session_id.map(uuid_text),
                n.seal_id,
                now,
            ],
        )?;
        Ok(())
    }

    /// Active (non-superseded) nodes for a project.
    pub fn nodes_for_project(&self, project_id: Uuid) -> Result<Vec<KnowledgeNode>, StoreError> {
        self.query_nodes(
            "project_id = ?1 AND superseded_at IS NULL ORDER BY confidence DESC",
            uuid_text(project_id),
        )
    }

    /// Active (non-superseded) nodes for an RFC.
    pub fn nodes_for_rfc(&self, rfc_id: &str) -> Result<Vec<KnowledgeNode>, StoreError> {
        self.query_nodes(
            "rfc_id = ?1 AND superseded_at IS NULL ORDER BY confidence DESC",
            rfc_id.to_string(),
        )
    }

    /// The most recently consolidated active nodes, newest first. Backs the
    /// unscoped `$reasoning graph` view (no `--project`/`--rfc` filter).
    pub fn recent_nodes(&self, limit: usize) -> Result<Vec<KnowledgeNode>, StoreError> {
        let sql = format!(
            "SELECT id, workspace_id, project_id, rfc_id, node_type, summary, confidence,
                 origin, created_at
             FROM knowledge_node WHERE superseded_at IS NULL
             ORDER BY created_at DESC LIMIT {}",
            limit.min(1000)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| Ok(build_node(row)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    fn query_nodes(
        &self,
        where_clause: &str,
        key: String,
    ) -> Result<Vec<KnowledgeNode>, StoreError> {
        let sql = format!(
            "SELECT id, workspace_id, project_id, rfc_id, node_type, summary, confidence,
                 origin, created_at
             FROM knowledge_node WHERE {where_clause}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![key], |row| Ok(build_node(row)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Fetch a single node by id (any lifecycle status). Backs the
    pub fn node_by_id(&self, id: Uuid) -> Result<Option<KnowledgeNode>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, workspace_id, project_id, rfc_id, node_type, summary, confidence,
                 origin, created_at
             FROM knowledge_node WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![uuid_text(id)], |row| Ok(build_node(row)))?;
        match rows.next() {
            Some(r) => Ok(Some(r??)),
            None => Ok(None),
        }
    }

    /// Nodes filtered by lifecycle `status` (`active` / `superseded` / …),
    pub fn nodes_by_status(
        &self,
        status: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeNode>, StoreError> {
        let sql = format!(
            "SELECT id, workspace_id, project_id, rfc_id, node_type, summary, confidence,
                 origin, created_at
             FROM knowledge_node WHERE status = ?1
             ORDER BY created_at DESC LIMIT {}",
            limit.min(1000)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![status], |row| Ok(build_node(row)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Active nodes whose `domain` matches, highest-confidence first. Backs L2b
    /// `status='active'` is the lifecycle filter (superseded/archived excluded).
    pub fn nodes_for_domain(
        &self,
        domain: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeNode>, StoreError> {
        let sql = format!(
            "SELECT id, workspace_id, project_id, rfc_id, node_type, summary, confidence,
                 origin, created_at
             FROM knowledge_node WHERE domain = ?1 AND status = 'active'
             ORDER BY confidence DESC LIMIT {}",
            limit.min(1000)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![domain], |row| Ok(build_node(row)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Active nodes whose summary/details/context block match any query token.
    /// This is intentionally plain SQL LIKE, not vector search: v1 projection
    /// needs deterministic local recall over the existing cache.
    pub fn search_nodes_text(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeNode>, StoreError> {
        Ok(self
            .search_node_hits(query, limit)?
            .into_iter()
            .map(|hit| hit.node)
            .collect())
    }

    pub fn search_node_hits(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeNodeSearchHit>, StoreError> {
        let terms = search_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let clauses = vec![
            "(lower(kn.summary) LIKE ? OR lower(coalesce(kn.details, '')) LIKE ? OR lower(coalesce(kn.context_block, '')) LIKE ?)";
            terms.len()
        ]
        .join(" OR ");
        let sql = format!(
            "SELECT kn.id, kn.workspace_id, kn.project_id, kn.rfc_id, kn.node_type, kn.summary,
                 kn.confidence, kn.origin, kn.created_at, kn.domain, kn.context_block, kn.seal_id,
                 s.agent_name, kn.source_turn_id, coalesce(kn.source_session_id, t.session_id)
             FROM knowledge_node kn
             LEFT JOIN turn t ON t.id = kn.source_turn_id
             LEFT JOIN session s ON s.id = coalesce(kn.source_session_id, t.session_id)
             WHERE kn.superseded_at IS NULL AND ({clauses})
             ORDER BY kn.confidence DESC, kn.created_at DESC LIMIT {}",
            limit.min(100)
        );
        let params: Vec<String> = terms
            .iter()
            .flat_map(|term| {
                let like = format!("%{term}%");
                [like.clone(), like.clone(), like]
            })
            .collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok(build_node_hit(row))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    pub fn node_hits_for_rfc(
        &self,
        rfc_id: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeNodeSearchHit>, StoreError> {
        let sql = format!(
            "SELECT kn.id, kn.workspace_id, kn.project_id, kn.rfc_id, kn.node_type, kn.summary,
                 kn.confidence, kn.origin, kn.created_at, kn.domain, kn.context_block, kn.seal_id,
                 s.agent_name, kn.source_turn_id, coalesce(kn.source_session_id, t.session_id)
             FROM knowledge_node kn
             LEFT JOIN turn t ON t.id = kn.source_turn_id
             LEFT JOIN session s ON s.id = coalesce(kn.source_session_id, t.session_id)
             WHERE kn.rfc_id = ?1 AND kn.superseded_at IS NULL
             ORDER BY kn.confidence DESC, kn.created_at DESC LIMIT {}",
            limit.min(1000)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![rfc_id], |row| Ok(build_node_hit(row)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    pub fn node_hit_by_id(&self, id: Uuid) -> Result<Option<KnowledgeNodeSearchHit>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT kn.id, kn.workspace_id, kn.project_id, kn.rfc_id, kn.node_type, kn.summary,
                 kn.confidence, kn.origin, kn.created_at, kn.domain, kn.context_block, kn.seal_id,
                 s.agent_name, kn.source_turn_id, coalesce(kn.source_session_id, t.session_id)
             FROM knowledge_node kn
             LEFT JOIN turn t ON t.id = kn.source_turn_id
             LEFT JOIN session s ON s.id = coalesce(kn.source_session_id, t.session_id)
             WHERE kn.id = ?1",
        )?;
        let mut rows = stmt.query_map(params![uuid_text(id)], |row| Ok(build_node_hit(row)))?;
        match rows.next() {
            Some(r) => Ok(Some(r??)),
            None => Ok(None),
        }
    }

    pub fn node_hits_by_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeNodeSearchHit>, StoreError> {
        let sql = format!(
            "SELECT kn.id, kn.workspace_id, kn.project_id, kn.rfc_id, kn.node_type, kn.summary,
                 kn.confidence, kn.origin, kn.created_at, kn.domain, kn.context_block, kn.seal_id,
                 s.agent_name, kn.source_turn_id, coalesce(kn.source_session_id, t.session_id)
             FROM knowledge_node kn
             LEFT JOIN turn t ON t.id = kn.source_turn_id
             LEFT JOIN session s ON s.id = coalesce(kn.source_session_id, t.session_id)
             WHERE kn.id LIKE ?1
             ORDER BY kn.created_at DESC LIMIT {}",
            limit.min(1000)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![format!("{prefix}%")], |row| Ok(build_node_hit(row)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Record that `ids` were served to an agent/human read: bump `access_count`
    /// and stamp `last_accessed=now`. This is the one write on the read path
    /// Applied by the REPL-owned store, never by the read-only MCP handle.
    /// Returns the number of rows touched.
    pub fn touch_nodes_accessed(&self, ids: &[Uuid]) -> Result<usize, StoreError> {
        let now = ts_text(Utc::now());
        let mut touched = 0;
        for id in ids {
            touched += self.conn.execute(
                "UPDATE knowledge_node
                 SET access_count = access_count + 1, last_accessed = ?2
                 WHERE id = ?1",
                params![uuid_text(*id), now],
            )?;
        }
        Ok(touched)
    }

    /// Read the decay counter for a node: how many times it has been served on
    /// a read path. `None` when the node is unknown. Used by GC/decay scoring
    /// and to verify the access bump landed.
    pub fn access_count(&self, id: Uuid) -> Result<Option<i64>, StoreError> {
        self.conn
            .query_row(
                "SELECT access_count FROM knowledge_node WHERE id = ?1",
                params![uuid_text(id)],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Mark a node superseded (replaced by a newer consolidation). Keeps the
    /// invariant `status='superseded'` ⇔ `superseded_at IS NOT NULL`.
    pub fn supersede_node(&self, id: Uuid) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE knowledge_node SET superseded_at = ?2, updated_at = ?2,
                 status = 'superseded' WHERE id = ?1",
            params![uuid_text(id), ts_text(Utc::now())],
        )?;
        Ok(())
    }

    /// Delete nodes superseded longer ago than `retention`. Returns the count.
    pub fn prune_superseded(&self, retention: Duration) -> Result<usize, StoreError> {
        let cutoff = ts_text(Utc::now() - retention);
        let n = self.conn.execute(
            "DELETE FROM knowledge_node WHERE superseded_at IS NOT NULL AND superseded_at < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }
}

fn search_terms(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::trim)
        .filter(|term| term.len() >= 3)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn build_node(row: &rusqlite::Row<'_>) -> Result<KnowledgeNode, StoreError> {
    let project_id = row
        .get::<_, Option<String>>(2)?
        .map(|s| parse_uuid(&s))
        .transpose()?;
    Ok(KnowledgeNode {
        id: parse_uuid(&row.get::<_, String>(0)?)?,
        workspace_id: parse_uuid(&row.get::<_, String>(1)?)?,
        project_id,
        rfc_ref: join_rfc(row.get(3)?, None),
        kind: enum_from_text(&row.get::<_, String>(4)?)?,
        summary: row.get(5)?,
        confidence: row.get(6)?,
        origin: enum_from_text(&row.get::<_, String>(7)?)?,
        created_at: parse_ts(&row.get::<_, String>(8)?)?,
    })
}

fn build_node_hit(row: &rusqlite::Row<'_>) -> Result<KnowledgeNodeSearchHit, StoreError> {
    Ok(KnowledgeNodeSearchHit {
        node: build_node(row)?,
        domain: row.get(9)?,
        context_block: row.get(10)?,
        seal_id: row.get(11)?,
        agent_name: row.get(12)?,
        source_turn_id: row
            .get::<_, Option<String>>(13)?
            .map(|id| parse_uuid(&id))
            .transpose()?,
        source_session_id: row
            .get::<_, Option<String>>(14)?
            .map(|id| parse_uuid(&id))
            .transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_reasoning_core::dto::RfcRef;
    use orkia_reasoning_core::enums::{KnowledgeNodeKind, NodeOrigin};
    use orkia_rfc_core::id::RfcId;

    fn node(kind: KnowledgeNodeKind, conf: f32) -> KnowledgeNode {
        KnowledgeNode {
            id: Uuid::new_v4(),
            workspace_id: Uuid::from_u128(1),
            project_id: Some(Uuid::from_u128(3)),
            rfc_ref: Some(RfcRef::new(RfcId::new("rfc-9"))),
            kind,
            summary: "use sqlite".into(),
            confidence: conf,
            origin: NodeOrigin::Local,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn upsert_and_query_by_project_and_rfc() {
        let s = ReasoningStore::in_memory().unwrap();
        let n = node(KnowledgeNodeKind::Decision, 0.8);
        s.upsert_node(&NodeInsert {
            node: &n,
            details: Some("rationale".into()),
            domain: None,
            context_block: None,
            source_turn_id: None,
            source_session_id: None,
            seal_id: None,
        })
        .unwrap();
        assert_eq!(s.nodes_for_project(Uuid::from_u128(3)).unwrap().len(), 1);
        let by_rfc = s.nodes_for_rfc("rfc-9").unwrap();
        assert_eq!(by_rfc.len(), 1);
        assert_eq!(by_rfc[0].kind, KnowledgeNodeKind::Decision);
    }

    #[test]
    fn nodes_for_domain_filters_active_and_touch_bumps_access() {
        let s = ReasoningStore::in_memory().unwrap();
        let n = node(KnowledgeNodeKind::Decision, 0.9);
        let id = n.id;
        s.upsert_node(&NodeInsert {
            node: &n,
            details: None,
            domain: Some("auth"),
            context_block: None,
            source_turn_id: None,
            source_session_id: None,
            seal_id: None,
        })
        .unwrap();

        let hits = s.nodes_for_domain("auth", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(s.nodes_for_domain("sync", 10).unwrap().len(), 0);

        // Access bump: count goes 0 → 1 and last_accessed is stamped.
        assert_eq!(s.touch_nodes_accessed(&[id]).unwrap(), 1);
        let (count, last): (i64, Option<String>) = s
            .conn
            .query_row(
                "SELECT access_count, last_accessed FROM knowledge_node WHERE id = ?1",
                params![uuid_text(id)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert!(last.is_some());

        // A superseded node drops out of the domain query (status flips).
        s.supersede_node(id).unwrap();
        assert_eq!(s.nodes_for_domain("auth", 10).unwrap().len(), 0);
    }

    #[test]
    fn nodes_by_status_splits_active_and_superseded() {
        let s = ReasoningStore::in_memory().unwrap();
        let n = node(KnowledgeNodeKind::Decision, 0.9);
        let id = n.id;
        s.upsert_node(&NodeInsert {
            node: &n,
            details: None,
            domain: None,
            context_block: None,
            source_turn_id: None,
            source_session_id: None,
            seal_id: None,
        })
        .unwrap();
        assert_eq!(s.nodes_by_status("active", 10).unwrap().len(), 1);
        assert_eq!(s.nodes_by_status("superseded", 10).unwrap().len(), 0);

        s.supersede_node(id).unwrap();
        assert_eq!(s.nodes_by_status("active", 10).unwrap().len(), 0);
        assert_eq!(s.nodes_by_status("superseded", 10).unwrap().len(), 1);
    }

    #[test]
    fn supersede_hides_then_prune_deletes() {
        let s = ReasoningStore::in_memory().unwrap();
        let n = node(KnowledgeNodeKind::Fact, 0.5);
        let id = n.id;
        s.upsert_node(&NodeInsert {
            node: &n,
            details: None,
            domain: None,
            context_block: None,
            source_turn_id: None,
            source_session_id: None,
            seal_id: None,
        })
        .unwrap();
        s.supersede_node(id).unwrap();
        assert_eq!(s.nodes_for_rfc("rfc-9").unwrap().len(), 0);
        // Superseded just now → not yet past retention.
        assert_eq!(s.prune_superseded(Duration::days(7)).unwrap(), 0);
        assert_eq!(s.prune_superseded(Duration::zero()).unwrap(), 1);
    }
}
