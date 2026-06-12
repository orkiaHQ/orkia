// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Vague 1: the four context-free display builtins (`help`, `version`,
//! `route`, `briefing`) were migrated from legacy `BuiltinCmd` arms returning
//! `Vec<BlockContent>` to native `Command`s returning `PipelineData`. Each test
//! proves equivalence with the legacy generator and that the command routes
//! through the registry-backed engine (the `tick` seam), returning structured
//! `Value` — never `BlockContent` (constraint C1).

use std::collections::HashMap;
use std::path::PathBuf;

use orkia_shell_types::exec::command::CommandCtx;
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ParsedStage, Value};

use crate::exec::commands::blocks_adapter::blocks_to_value;
use crate::exec::engine::{PipelineInput, run_plan};
use crate::exec::parse::try_parse_exec;
use crate::exec::registry::CommandRegistry;

fn ctx() -> CommandCtx {
    CommandCtx {
        cwd: PathBuf::from("."),
        env: HashMap::new(),
        data_dir: PathBuf::from("."),
        agents: Vec::new(),
        jobs: Vec::new(),
        journal: None,
        auth: None,
        attention: Vec::new(),
        attention_control: None,
        capabilities: orkia_shell_types::CapabilitySet::shell_default(),
    }
}

fn empty_input() -> PipelineInput {
    PipelineInput {
        data: PipelineData::Empty,
        label: "input".to_string(),
    }
}

/// Run a single-stage typed plan for `name` through the engine and collect the
/// resulting lines of text.
async fn run_one(name: &str) -> Vec<String> {
    let registry = CommandRegistry::with_pilots();
    let plan = vec![ParsedStage {
        name: name.to_string(),
        raw_args: Vec::new(),
    }];
    let data = run_plan(&plan, empty_input(), &ctx(), &registry)
        .await
        .expect("run");
    match data.into_value().await.expect("collect") {
        Value::List(items) => items
            .into_iter()
            .map(|v| match v {
                Value::String(s) => s,
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("expected a list of lines, got {other:?}"),
    }
}

/// The migrated command, run through the engine, reproduces the legacy
/// generator's content exactly (string-for-string).
fn legacy_lines(blocks: Vec<orkia_shell_types::BlockContent>) -> Vec<String> {
    match blocks_to_value(blocks) {
        Value::List(items) => items
            .into_iter()
            .map(|v| match v {
                Value::String(s) => s,
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("adapter must yield a list, got {other:?}"),
    }
}

#[tokio::test]
async fn help_matches_legacy() {
    assert_eq!(
        run_one("help").await,
        legacy_lines(orkia_builtin::help::help())
    );
}

#[tokio::test]
async fn version_matches_legacy() {
    assert_eq!(
        run_one("version").await,
        legacy_lines(orkia_builtin::help::version())
    );
}

#[tokio::test]
async fn route_matches_legacy() {
    assert_eq!(
        run_one("route").await,
        legacy_lines(orkia_builtin::route::route(&[]))
    );
}

#[tokio::test]
async fn briefing_matches_legacy() {
    assert_eq!(
        run_one("briefing").await,
        legacy_lines(orkia_builtin::briefing::briefing())
    );
}

/// Content spot-checks: the engine output carries the recognizable legacy text.
#[tokio::test]
async fn migrated_commands_carry_expected_text() {
    let help = run_one("help").await.join("\n");
    assert!(help.contains("ORKIA SHELL"), "help text; got: {help}");
    assert!(
        help.contains("AUGMENTED COMMANDS"),
        "help sections; got: {help}"
    );

    let version = run_one("version").await.join("\n");
    assert!(version.contains("orkia v"), "version; got: {version}");

    let route = run_one("route").await.join("\n");
    assert!(route.contains("ROUTING TABLE"), "route; got: {route}");

    let briefing = run_one("briefing").await.join("\n");
    assert!(
        briefing.contains("no sessions recorded"),
        "briefing; got: {briefing}"
    );
}

/// The migrated names route through the registry (the `tick` seam intercepts
/// them before legacy `parse_builtin`). Bare invocation is typed except for
/// `orkia ` namespace.
#[test]
fn migrated_names_are_typed_via_registry() {
    let registry = CommandRegistry::with_pilots();
    for name in [
        "help",
        "version",
        "briefing",
        "whoami",
        "plan",
        "history",
        "journal",
        "jobs",
        "attention",
    ] {
        let plan = try_parse_exec(name, &registry)
            .unwrap_or_else(|| panic!("`{name}` must parse as a typed command"));
        assert_eq!(plan.stages.len(), 1, "`{name}` is a single stage");
        assert_eq!(plan.stages[0].name, name);
        assert!(registry.contains(name), "`{name}` is registered");
    }
    for name in ["route", "log"] {
        assert!(
            try_parse_exec(name, &registry).is_none(),
            "bare `{name}` belongs to the system binary"
        );
        let namespaced = format!("orkia {name}");
        let plan = try_parse_exec(&namespaced, &registry)
            .unwrap_or_else(|| panic!("`{namespaced}` must parse as a typed command"));
        assert_eq!(plan.stages[0].name, name);
        assert!(registry.contains(name), "`{name}` is registered");
    }
}
