// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_builtin::agent::{self, AgentAction};
use orkia_shell_types::{AgentInfo, AgentStatus, BlockContent};
use uuid::Uuid;

fn mk(name: &str) -> AgentInfo {
    AgentInfo {
        id: Uuid::nil(),
        name: name.into(),
        archetype: "engineer".into(),
        status: AgentStatus::Idle,
        model: "claude".into(),
        dir: std::path::PathBuf::new(),
        description: None,
        command: "claude".into(),
        args: Vec::new(),
        assigned_projects: Vec::new(),
        max_context_tokens: 4000,
    }
}

#[test]
fn parse_defaults_to_list() {
    assert!(matches!(agent::parse(&[]).unwrap(), AgentAction::List));
}

#[test]
fn parse_show_requires_name() {
    assert!(agent::parse(&["show".into()]).is_err());
    let parsed = agent::parse(&["show".into(), "faye".into()]).unwrap();
    assert!(matches!(parsed, AgentAction::Show { name } if name == "faye"));
}

#[test]
fn parse_subcommands_require_name() {
    for sub in ["create", "edit", "remove"] {
        let err = agent::parse(&[sub.into()]).unwrap_err();
        assert!(err.contains("missing <name>"), "{err}");
    }
}

#[test]
fn list_empty() {
    let blocks = agent::list(&[]);
    assert!(
        blocks
            .iter()
            .all(|b| matches!(b, BlockContent::SystemInfo(_)))
    );
    assert!(!blocks.is_empty());
}

#[test]
fn list_with_agents() {
    let agents = vec![mk("faye"), mk("sage")];
    let blocks = agent::list(&agents);
    // count line + header + 2 rows
    assert_eq!(blocks.len(), 4);
}

#[test]
fn show_missing_errors() {
    let blocks = agent::show(&[], "ghost");
    assert!(matches!(blocks.as_slice(), [BlockContent::Error(_)]));
}

#[test]
fn show_found_renders_details() {
    let agents = vec![mk("faye")];
    let blocks = agent::show(&agents, "faye");
    assert!(blocks.len() >= 5);
}
