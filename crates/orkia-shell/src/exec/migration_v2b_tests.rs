// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Covers: `journal`, `jobs`, and `attention` builtins.

use std::collections::HashMap;
use std::path::PathBuf;

use orkia_shell_types::exec::command::{AttentionRow, CommandCtx};
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{JobInfo, JobKind, JobState, ParsedStage, Value};

use crate::exec::engine::{PipelineInput, run_plan};
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

fn ctx_in(data_dir: PathBuf) -> CommandCtx {
    CommandCtx {
        cwd: PathBuf::from("."),
        env: HashMap::new(),
        data_dir,
        agents: Vec::new(),
        jobs: Vec::new(),
        journal: None,
        auth: None,
        attention: Vec::new(),
        attention_control: None,
        capabilities: orkia_shell_types::CapabilitySet::shell_default(),
    }
}

fn field<'a>(row: &'a Value, key: &str) -> Option<&'a Value> {
    match row {
        Value::Record(m) => m.get(key),
        _ => None,
    }
}

// ── Vague 2: `journal` — structured table from the on-disk mirror ────────

fn journal_env(
    ty: orkia_shell_types::journal::EventType,
    ts: &str,
    agent: Option<&str>,
    message: Option<&str>,
) -> orkia_shell_types::journal::JournalEnvelope {
    orkia_shell_types::journal::JournalEnvelope {
        event_type: ty,
        timestamp: ts.to_string(),
        agent: agent.map(str::to_string),
        message: message.map(str::to_string),
        ..Default::default()
    }
}

fn seed_journal(data_dir: &std::path::Path, envs: &[orkia_shell_types::journal::JournalEnvelope]) {
    let lines: Vec<String> = envs
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .collect();
    std::fs::write(data_dir.join("journal.jsonl"), lines.join("\n")).expect("write journal");
}

async fn run_journal(data_dir: PathBuf, args: &[&str]) -> Vec<Value> {
    let registry = CommandRegistry::with_pilots();
    let plan = vec![ParsedStage {
        name: "journal".to_string(),
        raw_args: args.iter().map(|s| s.to_string()).collect(),
    }];
    let data = run_plan(&plan, empty_input(), &ctx_in(data_dir), &registry)
        .await
        .expect("run");
    match data.into_value().await.expect("collect") {
        Value::List(items) => items,
        other => panic!("expected a table, got {other:?}"),
    }
}

#[tokio::test]
async fn journal_emits_structured_records() {
    use orkia_shell_types::journal::EventType;
    let dir = tempfile::tempdir().expect("tmp");
    seed_journal(
        dir.path(),
        &[
            journal_env(
                EventType::Tell,
                "2026-01-01T00:00:00Z",
                Some("faye"),
                Some("hi there"),
            ),
            journal_env(EventType::Shell, "2026-01-01T00:01:00Z", None, None),
        ],
    );
    let rows = run_journal(dir.path().to_path_buf(), &[]).await;
    assert_eq!(rows.len(), 2, "both envelopes");
    assert_eq!(
        field(&rows[0], "type"),
        Some(&Value::String("tell".to_string()))
    );
    assert_eq!(
        field(&rows[0], "agent"),
        Some(&Value::String("faye".to_string()))
    );
    // The `Tell` summary is the (truncated) message.
    assert_eq!(
        field(&rows[0], "summary"),
        Some(&Value::String("hi there".to_string()))
    );
}

#[tokio::test]
async fn journal_filters_by_type() {
    use orkia_shell_types::journal::EventType;
    let dir = tempfile::tempdir().expect("tmp");
    seed_journal(
        dir.path(),
        &[
            journal_env(
                EventType::Tell,
                "2026-01-01T00:00:00Z",
                Some("faye"),
                Some("hi"),
            ),
            journal_env(EventType::Shell, "2026-01-01T00:01:00Z", None, None),
            journal_env(
                EventType::Tell,
                "2026-01-01T00:02:00Z",
                Some("ivy"),
                Some("yo"),
            ),
        ],
    );
    let rows = run_journal(dir.path().to_path_buf(), &["--type", "tell"]).await;
    assert_eq!(rows.len(), 2, "only the two tell events");
    assert_eq!(
        field(&rows[1], "agent"),
        Some(&Value::String("ivy".to_string()))
    );
}

#[tokio::test]
async fn journal_help_returns_text() {
    let dir = tempfile::tempdir().expect("tmp");
    let rows = run_journal(dir.path().to_path_buf(), &["--help"]).await;
    assert_eq!(rows.len(), 1, "help is a single text blob");
    assert!(
        matches!(&rows[0], Value::String(s) if !s.is_empty()),
        "help text present; got: {:?}",
        rows[0]
    );
}

#[tokio::test]
async fn journal_empty_when_no_mirror() {
    let dir = tempfile::tempdir().expect("tmp");
    let rows = run_journal(dir.path().to_path_buf(), &[]).await;
    assert!(rows.is_empty(), "no journal.jsonl → empty table");
}

// ── Vague 2: `jobs` (CommandCtx snapshot) + `attention` (cheap snapshot) ──

fn shell_job(id: u32, label: &str, state: JobState) -> JobInfo {
    JobInfo {
        id: orkia_shell_types::JobId(id),
        kind: JobKind::Shell {
            cmd: label.to_string(),
        },
        state,
        label: label.to_string(),
        pid: None,
        runtime: std::time::Duration::default(),
        sink: None,
    }
}

/// Run a typed single-stage plan with a `jobs`/`attention` snapshot installed.
async fn run_with_ctx(name: &str, jobs: Vec<JobInfo>, attention: Vec<AttentionRow>) -> Vec<Value> {
    let registry = CommandRegistry::with_pilots();
    let plan = vec![ParsedStage {
        name: name.to_string(),
        raw_args: Vec::new(),
    }];
    let ctx = CommandCtx {
        cwd: PathBuf::from("."),
        env: HashMap::new(),
        data_dir: PathBuf::from("."),
        agents: Vec::new(),
        jobs,
        journal: None,
        auth: None,
        attention,
        attention_control: None,
        capabilities: orkia_shell_types::CapabilitySet::shell_default(),
    };
    let data = run_plan(&plan, empty_input(), &ctx, &registry)
        .await
        .expect("run");
    match data.into_value().await.expect("collect") {
        Value::List(items) => items,
        other => panic!("expected a table, got {other:?}"),
    }
}

#[tokio::test]
async fn jobs_lists_shell_jobs_with_markers() {
    let jobs = vec![
        shell_job(1, "sleep 100", JobState::Running),
        shell_job(2, "vim", JobState::Stopped),
    ];
    let rows = run_with_ctx("jobs", jobs, Vec::new()).await;
    assert_eq!(rows.len(), 2);
    let lines: Vec<&str> = rows
        .iter()
        .map(|v| match v {
            Value::String(s) => s.as_str(),
            _ => panic!("jobs is a report of lines"),
        })
        .collect();
    // bash-style format preserved (equivalence with the legacy builtin):
    // the most recent shell job is `+` (current), the one before it `-`.
    assert!(lines[0].contains("[1]-"), "prev marker; got: {}", lines[0]);
    assert!(lines[0].contains("Running"), "state; got: {}", lines[0]);
    assert!(
        lines[1].contains("[2]+"),
        "current marker; got: {}",
        lines[1]
    );
    assert!(lines[1].contains("Stopped"), "state; got: {}", lines[1]);
}

#[tokio::test]
async fn jobs_empty_when_no_shell_jobs() {
    let rows = run_with_ctx("jobs", Vec::new(), Vec::new()).await;
    assert!(rows.is_empty(), "no shell jobs → empty table");
}

#[tokio::test]
async fn attention_lists_pending_prompts() {
    let pending = vec![AttentionRow {
        id: orkia_shell_types::AttentionId(1),
        job_id: Some(3),
        agent: "faye".to_string(),
        kind: orkia_shell_types::AttentionKind::AgentPrompt,
        severity: orkia_shell_types::AttentionSeverity::Fresh,
        age: "now".to_string(),
        summary: "Do you trust the files in this folder?".to_string(),
        actions: vec![orkia_shell_types::AttentionAction::Pull],
    }];
    let rows = run_with_ctx("attention", Vec::new(), pending).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(field(&rows[0], "job"), Some(&Value::Int(3)));
    assert_eq!(
        field(&rows[0], "agent"),
        Some(&Value::String("faye".to_string()))
    );
    assert_eq!(
        field(&rows[0], "kind"),
        Some(&Value::String("agent_prompt".to_string()))
    );
}

#[tokio::test]
async fn attention_empty_when_nothing_pending() {
    let rows = run_with_ctx("attention", Vec::new(), Vec::new()).await;
    assert!(rows.is_empty(), "no pending prompts → empty table");
}

#[tokio::test]
async fn attention_unknown_sub_is_bad_args() {
    let registry = CommandRegistry::with_pilots();
    let plan = vec![ParsedStage {
        name: "attention".to_string(),
        raw_args: vec!["bogus".to_string()],
    }];
    match run_plan(&plan, empty_input(), &ctx(), &registry).await {
        Err(orkia_shell_types::ExecError::BadArgs { command, .. }) => {
            assert_eq!(command, "attention");
        }
        Err(other) => panic!("expected attention BadArgs, got {other:?}"),
        Ok(_) => panic!("expected BadArgs, got Ok"),
    }
}
