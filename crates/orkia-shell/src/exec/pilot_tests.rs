// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Integration tests for the pilot commands and the conversion boundary —
//! separately).

use std::collections::HashMap;
use std::path::PathBuf;

use bytes::Bytes;
use futures::stream::{self, StreamExt};
use indexmap::IndexMap;
use orkia_shell_types::exec::command::CommandCtx;
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ExecError, ParsedStage, Value};

use crate::exec::convert::value_to_bytes;
use crate::exec::engine::{PipelineInput, run_plan};
use crate::exec::parse::try_parse_exec;
use crate::exec::registry::CommandRegistry;

fn ctx_at(cwd: PathBuf) -> CommandCtx {
    CommandCtx {
        cwd,
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

fn stage(name: &str, args: &[&str]) -> ParsedStage {
    ParsedStage {
        name: name.to_string(),
        raw_args: args.iter().map(|s| s.to_string()).collect(),
    }
}

async fn collect(data: PipelineData) -> Vec<Value> {
    match data.into_value().await.expect("collect") {
        Value::List(items) => items,
        other => vec![other],
    }
}

fn byte_input(text: &str) -> PipelineInput {
    let bytes = Bytes::from(text.to_string());
    PipelineInput {
        data: PipelineData::ByteStream(stream::once(async move { Ok(bytes) }).boxed()),
        label: "echo".to_string(),
    }
}

fn empty_input() -> PipelineInput {
    PipelineInput {
        data: PipelineData::Empty,
        label: "input".to_string(),
    }
}

// ── parse / routing ─────────────────────────────────────────────────────

#[test]
fn posix_pipe_is_not_typed() {
    let registry = CommandRegistry::with_pilots();
    // grep is not a registry command, and bare `ls` stays POSIX.
    assert!(try_parse_exec("ls | grep foo", &registry).is_none());
    assert!(try_parse_exec("cat x | wc -l", &registry).is_none());
}

#[test]
fn namespaced_ls_is_typed() {
    let registry = CommandRegistry::with_pilots();
    let plan = try_parse_exec("orkia ls", &registry).expect("typed");
    assert_eq!(plan.shell_prefix, None);
    assert_eq!(plan.stages.len(), 1);
    assert_eq!(plan.stages[0].name, "ls");

    let plan = try_parse_exec("ork ls | where size > 1mb", &registry).expect("typed");
    assert_eq!(plan.stages.len(), 2);
    assert_eq!(plan.stages[0].name, "ls");
    assert_eq!(plan.stages[1].name, "where");
}

#[test]
fn bare_ps_stays_legacy_namespaced_is_typed() {
    let registry = CommandRegistry::with_pilots();
    // Bare `ps` keeps the legacy rich builtin (no regression).
    assert!(try_parse_exec("ps", &registry).is_none());
    assert!(try_parse_exec("ps -a", &registry).is_none());
    // `orkia ps` reaches the typed, composable table.
    let plan = try_parse_exec("orkia ps | where status == working", &registry).expect("typed");
    assert_eq!(plan.stages[0].name, "ps");
    assert_eq!(plan.stages[1].name, "where");
}

#[test]
fn external_prefix_feeds_typed_stages() {
    let registry = CommandRegistry::with_pilots();
    let plan = try_parse_exec("echo '{}' | where a == 1", &registry).expect("typed");
    assert_eq!(plan.shell_prefix.as_deref(), Some("echo '{}'"));
    assert_eq!(plan.stages.len(), 1);
    assert_eq!(plan.stages[0].name, "where");

    let plan = try_parse_exec("echo data | from json | where x == 1", &registry).expect("typed");
    assert_eq!(plan.shell_prefix.as_deref(), Some("echo data"));
    assert_eq!(plan.stages.len(), 2);
    assert_eq!(plan.stages[0].name, "from");
}

#[test]
fn typed_then_external_captures_suffix() {
    let registry = CommandRegistry::with_pilots();
    // (Value → Bytes hand-off), no longer a fall-through.
    let plan = try_parse_exec("orkia ls | where size > 1mb | grep rs", &registry).expect("typed");
    assert_eq!(plan.shell_prefix, None);
    assert_eq!(plan.stages.len(), 2);
    assert_eq!(plan.stages[0].name, "ls");
    assert_eq!(plan.stages[1].name, "where");
    assert_eq!(plan.external_suffix.as_deref(), Some("grep rs"));
}

#[test]
fn typed_reappearing_after_external_falls_through() {
    let registry = CommandRegistry::with_pilots();
    // external in the *middle* of the typed segment is unsupported → fall back.
    assert!(try_parse_exec("orkia ls | grep x | where size > 1mb", &registry).is_none());
}

// ── boundary: Bytes → Value ─────────────────────────────────────────────

#[tokio::test]
async fn bytes_into_where_is_refused() {
    let registry = CommandRegistry::with_pilots();
    let plan = vec![stage("where", &["a", "==", "1"])];
    let result = run_plan(
        &plan,
        byte_input("{\"a\":1}"),
        &ctx_at(".".into()),
        &registry,
    )
    .await;
    match result {
        Err(ExecError::TypeMismatch {
            command, upstream, ..
        }) => {
            assert_eq!(command, "where");
            assert_eq!(upstream, "echo");
        }
        Err(other) => panic!("expected TypeMismatch, got {other:?}"),
        Ok(_) => panic!("expected TypeMismatch, got Ok"),
    }
}

#[tokio::test]
async fn from_json_bridges_bytes_to_table() {
    let registry = CommandRegistry::with_pilots();
    let plan = vec![
        stage("from", &["json"]),
        stage("where", &["status", "==", "Running"]),
    ];
    let json = r#"[{"status":"Running"},{"status":"Stopped"}]"#;
    let result = run_plan(&plan, byte_input(json), &ctx_at(".".into()), &registry)
        .await
        .expect("runs");
    let rows = collect(result).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get_path("status"),
        Some(&Value::String("Running".into()))
    );
}

#[tokio::test]
async fn from_json_malformed_is_bad_value_not_panic() {
    let registry = CommandRegistry::with_pilots();
    let plan = vec![stage("from", &["json"])];
    let result = run_plan(
        &plan,
        byte_input("not json {"),
        &ctx_at(".".into()),
        &registry,
    )
    .await;
    assert!(matches!(result, Err(ExecError::BadValue { .. })));
}

// ── pilots over a real directory ────────────────────────────────────────

#[tokio::test]
async fn ls_where_sort_first_pipeline() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("small.txt"), b"hi").expect("write");
    std::fs::write(dir.path().join("big.bin"), vec![0u8; 4096]).expect("write");
    std::fs::write(dir.path().join("mid.dat"), vec![0u8; 100]).expect("write");

    let registry = CommandRegistry::with_pilots();
    let ctx = ctx_at(dir.path().to_path_buf());

    // ls → where size > 50 → sort-by size → first 1  (smallest above 50)
    let plan = vec![
        stage("ls", &[]),
        stage("where", &["size", ">", "50"]),
        stage("sort-by", &["size"]),
        stage("first", &["1"]),
    ];
    let result = run_plan(&plan, empty_input(), &ctx, &registry)
        .await
        .expect("runs");
    let rows = collect(result).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get_path("name"),
        Some(&Value::String("mid.dat".into()))
    );
}

#[tokio::test]
async fn ls_filesize_literal_filter() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("tiny"), b"x").expect("write");
    std::fs::write(dir.path().join("huge"), vec![0u8; 2 * 1024 * 1024]).expect("write");

    let registry = CommandRegistry::with_pilots();
    let ctx = ctx_at(dir.path().to_path_buf());

    let plan = vec![stage("ls", &[]), stage("where", &["size", ">", "1mb"])];
    let rows = collect(
        run_plan(&plan, empty_input(), &ctx, &registry)
            .await
            .expect("runs"),
    )
    .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get_path("name"),
        Some(&Value::String("huge".into()))
    );
}

// ── sort-by collects internally (independent of the is_streaming flag) ───

#[tokio::test]
async fn sort_by_collects_all_input_itself() {
    // The engine never reads `is_streaming`; `sort-by` must drain the whole
    // upstream inside its own `run`. Feed an out-of-order multi-row stream
    // directly (not via a producer) and assert a full ascending ordering —
    // which is only possible if every row was collected before sorting.
    let registry = CommandRegistry::with_pilots();
    let ctx = ctx_at(".".into());

    let row = |n: i64| {
        let mut r = IndexMap::new();
        r.insert("n".to_string(), Value::Int(n));
        Ok(Value::Record(r))
    };
    let upstream = stream::iter(vec![row(3), row(1), row(2), row(5), row(4)]).boxed();
    let input = PipelineInput {
        data: PipelineData::ListStream(upstream),
        label: "input".to_string(),
    };

    let rows = collect(
        run_plan(&[stage("sort-by", &["n"])], input, &ctx, &registry)
            .await
            .expect("runs"),
    )
    .await;

    let ns: Vec<i64> = rows
        .iter()
        .filter_map(|r| match r.get_path("n") {
            Some(Value::Int(i)) => Some(*i),
            _ => None,
        })
        .collect();
    assert_eq!(
        ns,
        vec![1, 2, 3, 4, 5],
        "sort-by must collect all rows then order them"
    );
}

// ── ps migration ────────────────────────────────────────────────────────

#[tokio::test]
async fn ps_produces_agent_table_and_filters() {
    use orkia_shell_types::{AgentInfo, AgentStatus};

    let agent = |name: &str, status: AgentStatus| AgentInfo {
        id: uuid::Uuid::nil(),
        name: name.to_string(),
        archetype: "worker".to_string(),
        status,
        model: "claude".to_string(),
        dir: PathBuf::new(),
        description: None,
        command: String::new(),
        args: Vec::new(),
        assigned_projects: Vec::new(),
        max_context_tokens: 0,
    };

    let mut ctx = ctx_at(".".into());
    ctx.agents = vec![
        agent("faye", AgentStatus::Working),
        agent("max", AgentStatus::Idle),
    ];

    let registry = CommandRegistry::with_pilots();

    // Full table.
    let rows = collect(
        run_plan(&[stage("ps", &[])], empty_input(), &ctx, &registry)
            .await
            .expect("runs"),
    )
    .await;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].get_path("name"),
        Some(&Value::String("faye".into()))
    );

    // ps | where status == working
    let rows = collect(
        run_plan(
            &[
                stage("ps", &[]),
                stage("where", &["status", "==", "working"]),
            ],
            empty_input(),
            &ctx,
            &registry,
        )
        .await
        .expect("runs"),
    )
    .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get_path("name"),
        Some(&Value::String("faye".into()))
    );
}

// ── conversion determinism ──────────────────────────────────────────────

#[tokio::test]
async fn value_to_bytes_is_deterministic_tsv() {
    let make = || {
        let mut record = IndexMap::new();
        record.insert("name".to_string(), Value::String("a.rs".into()));
        record.insert("size".to_string(), Value::Filesize(2048));
        PipelineData::Value(Value::List(vec![Value::Record(record)]))
    };

    let drain = |data: PipelineData| async move {
        let mut stream = value_to_bytes(data);
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        out
    };

    let first = drain(make()).await;
    let second = drain(make()).await;
    assert_eq!(first, second);
    assert_eq!(String::from_utf8(first).expect("utf8"), "a.rs\t2048\n");
}
