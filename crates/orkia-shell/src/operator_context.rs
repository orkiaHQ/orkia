// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::path::{Path, PathBuf};

use orkia_rfc_core::frontmatter::OperatorConstraints;
use orkia_rfc_core::{RfcId, RfcRecord, RfcStore};

pub(crate) struct OperatorContext {
    pub constraints: Option<OperatorConstraints>,
    pub rfc_body: Option<String>,
    pub agents_rule: Option<String>,
    pub kg_refs: Vec<KnowledgeRef>,
}

pub(crate) struct KnowledgeRef {
    pub id: String,
    pub summary: String,
}

pub(crate) fn load(data_dir: &Path, rfc_id: &RfcId, target: Option<&str>) -> OperatorContext {
    let Some((project_dir, record)) = load_rfc_record(data_dir, rfc_id) else {
        return OperatorContext {
            constraints: None,
            rfc_body: None,
            agents_rule: nearest_agents_rule_from(std::env::current_dir().ok().as_deref()),
            kg_refs: load_kg_refs(data_dir, rfc_id),
        };
    };
    OperatorContext {
        constraints: record.fm.operator.and_then(|o| o.constraints),
        rfc_body: Some(record.body),
        agents_rule: nearest_agents_rule(&project_dir, target),
        kg_refs: load_kg_refs(data_dir, rfc_id),
    }
}

pub(crate) fn load_constraints(data_dir: &Path, rfc_id: &RfcId) -> Option<OperatorConstraints> {
    load(data_dir, rfc_id, None).constraints
}

fn load_rfc_record(data_dir: &Path, rfc_id: &RfcId) -> Option<(PathBuf, RfcRecord)> {
    let projects = data_dir.join("projects");
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
        let project_dir = entry.path();
        let store = RfcStore::new(&project_dir);
        if let Ok(record) = store.load(rfc_id) {
            return Some((project_dir, record));
        }
    }
    None
}

fn nearest_agents_rule(project_dir: &Path, target: Option<&str>) -> Option<String> {
    let start = target
        .filter(|t| !t.is_empty())
        .map(|t| project_dir.join(t))
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| project_dir.to_path_buf());
    nearest_agents_rule_from(Some(&start))
}

fn nearest_agents_rule_from(start: Option<&Path>) -> Option<String> {
    let mut dir = start?.to_path_buf();
    loop {
        let p = dir.join("AGENTS.md");
        if let Ok(s) = std::fs::read_to_string(&p)
            && let Some(line) = s.lines().find(|l| is_operator_relevant_rule(l))
        {
            return Some(line.trim().to_string());
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn is_operator_relevant_rule(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "arc<mutex",
        "shared mutable",
        "message passing",
        "test",
        "coverage",
        "secret",
        "credential",
        ".env",
        "unwrap",
        "expect",
        "panic",
        "untrusted",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn load_kg_refs(data_dir: &Path, rfc_id: &RfcId) -> Vec<KnowledgeRef> {
    let path = crate::reasoning_builtins::store_path(data_dir);
    let Ok(store) = orkia_reasoning_store::ReasoningStore::open(&path) else {
        return Vec::new();
    };
    let Ok(nodes) = store.nodes_for_rfc(rfc_id.as_str()) else {
        return Vec::new();
    };
    nodes
        .into_iter()
        .take(5)
        .map(|n| KnowledgeRef {
            id: n.id.to_string(),
            summary: n.summary,
        })
        .collect()
}
