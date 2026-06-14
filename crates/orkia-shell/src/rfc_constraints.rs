// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::collections::BTreeSet;
use std::path::Path;

use orkia_rfc_core::frontmatter::OperatorConstraints;
use orkia_rfc_core::{RfcId, RfcRecord};

pub(crate) struct ConstraintProposal {
    pub constraints: OperatorConstraints,
    pub sources: Vec<String>,
}

pub(crate) fn propose(
    data_dir: &Path,
    project_dir: &Path,
    rfc_id: &RfcId,
    record: &RfcRecord,
) -> ConstraintProposal {
    let mut paths = BTreeSet::new();
    let mut sources = Vec::new();

    collect_path_mentions(&record.body, &mut paths);
    if !paths.is_empty() {
        sources.push("RFC body path mentions".to_string());
    }

    let kg_paths = kg_path_mentions(data_dir, rfc_id);
    if !kg_paths.is_empty() {
        paths.extend(kg_paths);
        sources.push("KG nodes linked to RFC".to_string());
    }

    if paths.is_empty() {
        let inferred = infer_project_paths(project_dir);
        if !inferred.is_empty() {
            paths.extend(inferred);
            sources.push("project directory layout".to_string());
        }
    }

    if paths.is_empty() {
        paths.insert("**".to_string());
        sources.push("fallback wildcard; edit before accepting".to_string());
    }

    let agents = std::fs::read_to_string(project_dir.join("AGENTS.md")).unwrap_or_default();
    if !agents.is_empty() {
        sources.push("project AGENTS.md".to_string());
    }
    let combined = format!("{}\n{}", record.body, agents).to_lowercase();

    ConstraintProposal {
        constraints: OperatorConstraints {
            allowed_paths: paths.iter().cloned().collect(),
            forbidden_paths: forbidden_paths(&combined),
            forbidden_commands: forbidden_commands(&combined),
            risk_ceiling: Some(risk_ceiling(&combined).to_string()),
            watch_paths: paths.into_iter().take(8).collect(),
            // The frozen contract surface is declared authoritatively in the RFC
            // frontmatter (`[operator.constraints].contract_paths`), read back by
            // `operator_context::load_constraints`. The heuristic proposal does
            // not seed it (a freeze is a deliberate human act, not a guess).
            contract_paths: Vec::new(),
        },
        sources,
    }
}

fn kg_path_mentions(data_dir: &Path, rfc_id: &RfcId) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    let path = crate::reasoning_builtins::store_path(data_dir);
    let Ok(store) = orkia_reasoning_store::ReasoningStore::open(&path) else {
        return paths;
    };
    let Ok(nodes) = store.nodes_for_rfc(rfc_id.as_str()) else {
        return paths;
    };
    for node in nodes.into_iter().take(8) {
        collect_path_mentions(&node.summary, &mut paths);
    }
    paths
}

fn collect_path_mentions(text: &str, paths: &mut BTreeSet<String>) {
    for raw in text.split(char::is_whitespace) {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                '`' | '"' | '\'' | ',' | '.' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}'
            )
        });
        if let Some(path) = normalize_path_token(token) {
            paths.insert(path);
        }
    }
}

fn normalize_path_token(token: &str) -> Option<String> {
    if token.len() < 3 || token.starts_with("http://") || token.starts_with("https://") {
        return None;
    }
    if token.contains("..") || token.starts_with('/') || !token.contains('/') {
        return None;
    }
    let trimmed = token.trim_start_matches("./");
    if trimmed.contains("://") || trimmed.starts_with('.') {
        return None;
    }
    if trimmed.ends_with("/**") {
        return Some(trimmed.to_string());
    }
    if looks_like_file(trimmed) {
        return Path::new(trimmed)
            .parent()
            .and_then(Path::to_str)
            .filter(|p| !p.is_empty())
            .map(|p| format!("{p}/**"));
    }
    Some(format!("{}/**", trimmed.trim_end_matches('/')))
}

fn looks_like_file(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.rsplit_once('.').is_some())
}

fn infer_project_paths(project_dir: &Path) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for candidate in ["src", "crates", "apps", "packages", "tests", "docs"] {
        if project_dir.join(candidate).is_dir() {
            paths.insert(format!("{candidate}/**"));
        }
    }
    paths
}

fn forbidden_paths(text: &str) -> Vec<String> {
    let mut paths = BTreeSet::new();
    if text.contains("do not touch") || text.contains("don't touch") {
        paths.insert("target/**".to_string());
    }
    if text.contains("secret") || text.contains(".env") {
        paths.insert(".env*".to_string());
    }
    paths.into_iter().collect()
}

fn forbidden_commands(text: &str) -> Vec<String> {
    let mut commands = BTreeSet::new();
    commands.insert("git push*".to_string());
    commands.insert("rm -rf *".to_string());
    if text.contains("no deploy") || text.contains("do not deploy") {
        commands.insert("*deploy*".to_string());
    }
    if text.contains("no migration") || text.contains("do not run migration") {
        commands.insert("*migrate*".to_string());
    }
    commands.into_iter().collect()
}

fn risk_ceiling(text: &str) -> &'static str {
    if text.contains("read-only") || text.contains("read only") || text.contains("documentation") {
        "medium"
    } else {
        "high"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use orkia_rfc_core::{RfcFrontmatter, RfcState, content_hash_of};

    #[test]
    fn proposal_extracts_rfc_paths_and_defaults_commands() {
        let dir = tempfile::tempdir().unwrap();
        let record = record_with_body(
            "Touch crates/orkia-shell/src/operator.rs and docs/operator.md. Do not deploy.",
        );

        let proposal = propose(dir.path(), dir.path(), &RfcId::new("operator-v1"), &record);

        assert!(
            proposal
                .constraints
                .allowed_paths
                .contains(&"crates/orkia-shell/src/**".to_string())
        );
        assert!(
            proposal
                .constraints
                .allowed_paths
                .contains(&"docs/**".to_string())
        );
        assert!(
            proposal
                .constraints
                .forbidden_commands
                .contains(&"*deploy*".to_string())
        );
    }

    fn record_with_body(body: &str) -> RfcRecord {
        RfcRecord {
            fm: RfcFrontmatter {
                id: RfcId::new("operator-v1"),
                state: RfcState::DraftActive,
                version: 1,
                created_at: chrono::FixedOffset::east_opt(0)
                    .unwrap()
                    .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                    .unwrap(),
                updated_at: chrono::FixedOffset::east_opt(0)
                    .unwrap()
                    .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                    .unwrap(),
                content_hash: content_hash_of(body),
                agents: Vec::new(),
                locked_by: None,
                locked_at: None,
                title: None,
                status: None,
                assigned: None,
                kind: None,
                forge: None,
                scope: None,
                operator: None,
            },
            body: body.to_string(),
        }
    }
}
