// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Vague 2: privileged builtins via the enriched `CommandCtx`. `log`, `history`,
//! and `journal` read on-disk state through the `data_dir` field
//! (`history`/`journal` emit structured `Value` tables). `whoami`/`plan` read
//! identity state through the `AuthView` service
//! handle (a stub proves the wiring; absent handle → empty output). `jobs` and
//! `attention` read cheap `CommandCtx` snapshots (`jobs`/`attention` fields).

use std::collections::HashMap;
use std::path::PathBuf;

use orkia_shell_types::exec::command::CommandCtx;
use orkia_shell_types::exec::pipeline_data::PipelineData;
use orkia_shell_types::{ParsedStage, Value};

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

// ── Vague 2: `log` — first privileged builtin (reads `data_dir`) ─────────

/// A context rooted at `data_dir` (the field added for Vague 2).
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

/// Write `<data_dir>/jobs/<id>/output.log` with the given lines.
fn seed_job_log(data_dir: &std::path::Path, id: u32, body: &str) {
    let dir = data_dir.join("jobs").join(id.to_string());
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("output.log"), body).expect("write log");
}

/// Run a typed `log <args...>` plan against a context rooted at `data_dir`.
async fn run_log(data_dir: PathBuf, args: &[&str]) -> Result<String, orkia_shell_types::ExecError> {
    let registry = CommandRegistry::with_pilots();
    let plan = vec![ParsedStage {
        name: "log".to_string(),
        raw_args: args.iter().map(|s| s.to_string()).collect(),
    }];
    let data = run_plan(&plan, empty_input(), &ctx_in(data_dir), &registry).await?;
    match data.into_value().await? {
        Value::String(s) => Ok(s),
        other => panic!("expected a string, got {other:?}"),
    }
}

#[tokio::test]
async fn log_reads_job_output_from_data_dir() {
    let dir = tempfile::tempdir().expect("tmp");
    seed_job_log(dir.path(), 7, "line one\nline two\nline three");
    let out = run_log(dir.path().to_path_buf(), &["7"])
        .await
        .expect("log");
    assert_eq!(out, "line one\nline two\nline three");
}

#[tokio::test]
async fn log_tail_keeps_last_n_lines() {
    let dir = tempfile::tempdir().expect("tmp");
    seed_job_log(dir.path(), 7, "a\nb\nc\nd\ne");
    let out = run_log(dir.path().to_path_buf(), &["7", "--tail", "2"])
        .await
        .expect("log");
    assert_eq!(out, "d\ne");
}

#[tokio::test]
async fn log_missing_output_is_a_runtime_error() {
    let dir = tempfile::tempdir().expect("tmp");
    let err = run_log(dir.path().to_path_buf(), &["999"])
        .await
        .expect_err("no such log");
    assert!(
        matches!(err, orkia_shell_types::ExecError::Runtime { ref command, .. } if command == "log"),
        "expected a log Runtime error, got {err:?}"
    );
}

#[tokio::test]
async fn log_invalid_target_is_bad_args() {
    let dir = tempfile::tempdir().expect("tmp");
    let err = run_log(dir.path().to_path_buf(), &["not-a-job"])
        .await
        .expect_err("invalid target");
    assert!(
        matches!(err, orkia_shell_types::ExecError::BadArgs { ref command, .. } if command == "log"),
        "expected a log BadArgs error, got {err:?}"
    );
}

/// filesystem read **without** the `fs_read` capability fails closed via the
/// verified `CommandCtx::require_fs_read` accessor — even when the file exists.
#[tokio::test]
async fn log_without_fs_read_capability_fails_closed() {
    let dir = tempfile::tempdir().expect("tmp");
    seed_job_log(dir.path(), 7, "secret output"); // file exists; the capability gate must still deny.
    let registry = CommandRegistry::with_pilots();
    let plan = vec![ParsedStage {
        name: "log".to_string(),
        raw_args: vec!["7".to_string()],
    }];
    let ctx = CommandCtx {
        cwd: PathBuf::from("."),
        env: HashMap::new(),
        data_dir: dir.path().to_path_buf(),
        agents: Vec::new(),
        jobs: Vec::new(),
        journal: None,
        auth: None,
        attention: Vec::new(),
        attention_control: None,
        // Total sandbox: no fs_read granted.
        capabilities: orkia_shell_types::CapabilitySet::sandbox(),
    };
    match run_plan(&plan, empty_input(), &ctx, &registry).await {
        Err(orkia_shell_types::ExecError::CapabilityDenied {
            command,
            capability,
            ..
        }) => {
            assert_eq!(command, "log");
            assert_eq!(capability, "fs_read");
        }
        Err(other) => panic!("expected CapabilityDenied, got {other:?}"),
        Ok(_) => panic!("expected fail-closed CapabilityDenied, got Ok"),
    }
}

// ── Vague 2: `whoami`/`plan` — privileged builtins via the AuthView handle ──

/// A stub identity view, so the test exercises the `CommandCtx.auth` wiring
/// (the service-handle mechanism) without the heavy auth/capability crates.
struct StubAuth;

impl orkia_shell_types::AuthView for StubAuth {
    fn whoami_lines(&self) -> Vec<String> {
        vec![
            "@faye · faye@orkia.dev".to_string(),
            "  plan:    pro".to_string(),
        ]
    }
    fn plan_lines(&self) -> Vec<String> {
        vec!["plan: pro (account @faye)".to_string()]
    }
}

/// Run a typed single-stage plan with an `AuthView` installed on the context.
async fn run_with_auth(
    name: &str,
    auth: std::sync::Arc<dyn orkia_shell_types::AuthView>,
) -> Vec<String> {
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
        jobs: Vec::new(),
        journal: None,
        auth: Some(auth),
        attention: Vec::new(),
        attention_control: None,
        capabilities: orkia_shell_types::CapabilitySet::shell_default(),
    };
    let data = run_plan(&plan, empty_input(), &ctx, &registry)
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

#[tokio::test]
async fn whoami_surfaces_auth_view_lines() {
    let out = run_with_auth("whoami", std::sync::Arc::new(StubAuth)).await;
    // prints); the auth lines follow.
    assert_eq!(out.len(), 3, "username + 2 auth lines, got {out:?}");
    assert!(!out[0].is_empty(), "system username line must be present");
    assert_eq!(
        out[1..],
        [
            "@faye · faye@orkia.dev".to_string(),
            "  plan:    pro".to_string()
        ]
    );
}

#[tokio::test]
async fn plan_surfaces_auth_view_lines() {
    let out = run_with_auth("plan", std::sync::Arc::new(StubAuth)).await;
    assert_eq!(out, vec!["plan: pro (account @faye)".to_string()]);
}

/// With no `AuthView` installed, the commands degrade gracefully (fail-soft
/// for a read-only introspection): `whoami` still answers with the system
#[tokio::test]
async fn whoami_without_auth_view_is_empty() {
    let registry = CommandRegistry::with_pilots();
    let plan = vec![ParsedStage {
        name: "whoami".to_string(),
        raw_args: Vec::new(),
    }];
    let data = run_plan(&plan, empty_input(), &ctx(), &registry)
        .await
        .expect("run");
    match data.into_value().await.expect("collect") {
        Value::List(items) => {
            assert_eq!(
                items.len(),
                1,
                "no auth view → username only, got {items:?}"
            );
        }
        other => panic!("expected a list, got {other:?}"),
    }
}

// ── Vague 2: `history` — structured table from the on-disk mirror ─────────

/// Write history entries as JSONL to `<data_dir>/history.jsonl`.
fn seed_history(data_dir: &std::path::Path, entries: &[orkia_shell_types::HistoryEntry]) {
    let lines: Vec<String> = entries
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .collect();
    std::fs::write(data_dir.join("history.jsonl"), lines.join("\n")).expect("write history");
}

fn hist_entry(
    seq: u64,
    ty: orkia_shell_types::HistoryType,
    line: &str,
) -> orkia_shell_types::HistoryEntry {
    orkia_shell_types::HistoryEntry::new(seq, ty, line)
}

/// Run a typed `history <args...>` plan and return the table rows.
async fn run_history(data_dir: PathBuf, args: &[&str]) -> Vec<Value> {
    let registry = CommandRegistry::with_pilots();
    let plan = vec![ParsedStage {
        name: "history".to_string(),
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

fn field<'a>(row: &'a Value, key: &str) -> Option<&'a Value> {
    match row {
        Value::Record(m) => m.get(key),
        _ => None,
    }
}

#[tokio::test]
async fn history_emits_structured_records() {
    use orkia_shell_types::HistoryType;
    let dir = tempfile::tempdir().expect("tmp");
    seed_history(
        dir.path(),
        &[
            hist_entry(1, HistoryType::Shell, "ls -la"),
            hist_entry(2, HistoryType::Builtin, "ps"),
            hist_entry(3, HistoryType::Shell, "cargo build"),
        ],
    );
    let rows = run_history(dir.path().to_path_buf(), &[]).await;
    assert_eq!(rows.len(), 3, "all three entries, chronological");
    // Structured Value, not pre-rendered text: fields are typed and addressable.
    assert_eq!(field(&rows[0], "seq"), Some(&Value::Int(1)));
    assert_eq!(
        field(&rows[2], "command"),
        Some(&Value::String("cargo build".to_string()))
    );
}

#[tokio::test]
async fn history_limit_keeps_last_n() {
    use orkia_shell_types::HistoryType;
    let dir = tempfile::tempdir().expect("tmp");
    seed_history(
        dir.path(),
        &[
            hist_entry(1, HistoryType::Shell, "one"),
            hist_entry(2, HistoryType::Shell, "two"),
            hist_entry(3, HistoryType::Shell, "three"),
        ],
    );
    let rows = run_history(dir.path().to_path_buf(), &["--limit", "1"]).await;
    assert_eq!(rows.len(), 1, "only the last entry");
    assert_eq!(
        field(&rows[0], "command"),
        Some(&Value::String("three".to_string()))
    );
}

#[tokio::test]
async fn history_search_filters_by_substring() {
    use orkia_shell_types::HistoryType;
    let dir = tempfile::tempdir().expect("tmp");
    seed_history(
        dir.path(),
        &[
            hist_entry(1, HistoryType::Shell, "git status"),
            hist_entry(2, HistoryType::Shell, "cargo test"),
            hist_entry(3, HistoryType::Shell, "git push"),
        ],
    );
    let rows = run_history(dir.path().to_path_buf(), &["--search", "git"]).await;
    assert_eq!(rows.len(), 2, "only the two git entries");
    assert_eq!(field(&rows[0], "seq"), Some(&Value::Int(1)));
    assert_eq!(field(&rows[1], "seq"), Some(&Value::Int(3)));
}

#[tokio::test]
async fn history_empty_when_no_mirror() {
    let dir = tempfile::tempdir().expect("tmp");
    let rows = run_history(dir.path().to_path_buf(), &[]).await;
    assert!(rows.is_empty(), "no history.jsonl → empty table");
}
