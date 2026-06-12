// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::path::Path;

use chrono::Utc;
use orkia_reasoning_core::dto::{KnowledgeNode, RfcRef};
use orkia_reasoning_core::enums::{KnowledgeNodeKind, NodeOrigin};
use orkia_reasoning_store::{NewSession, NodeInsert};
use orkia_rfc_core::RfcId;
use uuid::Uuid;

use super::*;

pub(super) fn citation() -> Citation {
    Citation {
        id: "kg:abc".into(),
        source: "knowledge_node".into(),
        summary: "Auth uses PKCE".into(),
        score: 42,
        timestamp: None,
        source_ref: Some("kg://node/abc".into()),
        node_id: Some("abc".into()),
        seal_id: None,
        job_id: None,
    }
}

pub(super) fn ask(question: &str) -> AskArgs {
    AskArgs {
        question: question.into(),
        agent: None,
        evidence_agent: None,
        domain: None,
        cwd: None,
        last: 5,
        job: None,
        rfc: None,
        since: None,
        evidence_only: false,
        timeout_ms: 1_500,
        json: false,
    }
}

pub(super) fn seed_node_with_domain(data_dir: &Path, summary: &str, domain: &str) -> Uuid {
    upsert_seed_node(
        data_dir,
        SeedNode {
            summary,
            domain: Some(domain),
            ..SeedNode::default()
        },
    )
}

pub(super) fn seed_node(
    data_dir: &Path,
    summary: &str,
    details: Option<&str>,
    context_block: Option<&str>,
) -> Uuid {
    upsert_seed_node(
        data_dir,
        SeedNode {
            summary,
            details,
            context_block,
            ..SeedNode::default()
        },
    )
}

pub(super) fn seed_node_with_agent(data_dir: &Path, summary: &str, agent: &str) -> Uuid {
    let path = crate::reasoning_builtins::store_path(data_dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store = ReasoningStore::open(&path).unwrap();
    let session_id = store
        .create_session(&NewSession {
            workspace_id: Uuid::from_u128(1),
            account_id: Uuid::from_u128(2),
            agent_name: agent.into(),
            project_id: None,
            rfc_ref: None,
        })
        .unwrap();
    upsert_seed_node(
        data_dir,
        SeedNode {
            summary,
            source_session_id: Some(session_id),
            ..SeedNode::default()
        },
    )
}

pub(super) fn seed_node_with_rfc(data_dir: &Path, summary: &str, rfc: &str) -> Uuid {
    upsert_seed_node(
        data_dir,
        SeedNode {
            summary,
            rfc_ref: Some(RfcRef::new(RfcId::new(rfc))),
            ..SeedNode::default()
        },
    )
}

#[derive(Default)]
struct SeedNode<'a> {
    summary: &'a str,
    details: Option<&'a str>,
    domain: Option<&'a str>,
    context_block: Option<&'a str>,
    rfc_ref: Option<RfcRef>,
    source_session_id: Option<Uuid>,
}

fn upsert_seed_node(data_dir: &Path, seed: SeedNode<'_>) -> Uuid {
    let path = crate::reasoning_builtins::store_path(data_dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store = ReasoningStore::open(&path).unwrap();
    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        workspace_id: Uuid::from_u128(1),
        project_id: None,
        rfc_ref: seed.rfc_ref,
        kind: KnowledgeNodeKind::Decision,
        summary: seed.summary.into(),
        confidence: 0.9,
        origin: NodeOrigin::Cloud,
        created_at: Utc::now(),
    };
    store
        .upsert_node(&NodeInsert {
            node: &node,
            details: seed.details.map(str::to_string),
            domain: seed.domain,
            context_block: seed.context_block.map(str::to_string),
            source_turn_id: None,
            source_session_id: seed.source_session_id,
            seal_id: None,
        })
        .unwrap();
    node.id
}
