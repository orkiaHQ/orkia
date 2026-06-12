// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! End-to-end test for the scoped SEAL pipeline.
//!
//! Drives synthetic `OrkiaEvent`s through `seal::route_event` (the
//! same function the consumer task runs) and asserts the resulting
//! on-disk chain layout, content, and verifiability. This is the
//! integration counterpart to the per-module unit tests in
//! `seal::chain`, `seal::manager`, `seal::consumer`.

use std::collections::HashMap;
use std::sync::Arc;

use orkia_shell::protocol::{EventPayload, EventSource, OrkiaEvent};
use orkia_shell::seal::{JobProjects, SealChain, SealManager, route_event};
use orkia_shell_types::job::JobId;
use parking_lot::RwLock;
use tempfile::TempDir;

fn evt(job: JobId, agent: &str, source: EventSource, payload: EventPayload) -> OrkiaEvent {
    OrkiaEvent {
        source,
        event: payload,
        confidence: 1.0,
        timestamp: chrono::Utc::now(),
        job_id: job,
        agent_name: agent.to_string(),
        rfc_id: None,
    }
}

fn projects() -> JobProjects {
    Arc::new(RwLock::new(HashMap::new()))
}

#[test]
fn full_delegated_flow_writes_both_chains_and_verifies_deep() {
    let dir = TempDir::new().expect("tmp");
    let mut mgr = SealManager::new(dir.path().to_path_buf());
    let projs = projects();

    // 1. RFC creation lands on the project chain via the
    //    `data.project` field — no job context required.
    route_event(
        &mut mgr,
        &projs,
        &orkia_shell::seal::ScheduledContext::default(),
        evt(
            JobId(0),
            "",
            EventSource::Internal,
            EventPayload::Custom {
                name: "rfc.create".into(),
                data: serde_json::json!({
                    "slug": "implement-x",
                    "project": "orkia-shell",
                    "content_hash": "abc1234567890def",
                }),
            },
        ),
    )
    .expect("route");

    // 2. RFC delegate records project membership BEFORE spawn, the
    //    way the REPL does in dispatch_rfc_delegate.
    projs.write().insert(JobId(42), "orkia-shell".to_string());

    // 3. agent.spawn creates the job chain with system-prompt /
    //    memory hashes as the genesis record.
    route_event(
        &mut mgr,
        &projs,
        &orkia_shell::seal::ScheduledContext::default(),
        evt(
            JobId(42),
            "faye",
            EventSource::Internal,
            EventPayload::Custom {
                name: "agent.spawn".into(),
                data: serde_json::json!({
                    "job_id": 42,
                    "agent": "faye",
                    "system_prompt_hash": "a4f1b3e29c8d7e1f",
                    "memory_hash": "7c9d2e1f4b3aa6e8",
                    "tools_count": 5,
                }),
            },
        ),
    )
    .expect("route");

    // 4. Live tool flow.
    for payload in [
        EventPayload::ToolUse {
            tool: "Read".into(),
            target: Some("src/auth/test.rs".into()),
            input_summary: None,
        },
        EventPayload::ToolResult {
            tool: "Read".into(),
            target: Some("src/auth/test.rs".into()),
            exit_code: Some(0),
            output_summary: None,
        },
        EventPayload::PermissionRequest {
            tool: Some("Write".into()),
            description: "write src/auth/test.rs".into(),
            risk: Some("medium".into()),
        },
    ] {
        route_event(
            &mut mgr,
            &projs,
            &orkia_shell::seal::ScheduledContext::default(),
            evt(
                JobId(42),
                "faye",
                EventSource::Hook {
                    provider: "claude".into(),
                },
                payload,
            ),
        )
        .expect("route");
    }

    // 5. Session end closes the job chain and bridges to project.
    route_event(
        &mut mgr,
        &projs,
        &orkia_shell::seal::ScheduledContext::default(),
        evt(
            JobId(42),
            "faye",
            EventSource::Hook {
                provider: "claude".into(),
            },
            EventPayload::SessionEnd { exit_code: Some(0) },
        ),
    )
    .expect("route");

    // ─── Assert job chain ──────────────────────────────────────
    let job_chain_path = dir.path().join("agents/faye/jobs/42/seal.jsonl");
    assert!(job_chain_path.exists(), "job chain file missing");
    let job_chain = SealChain::load(job_chain_path).expect("load job");
    let job_types: Vec<&str> = job_chain
        .records()
        .iter()
        .map(|r| r.event_type.as_str())
        .collect();
    assert_eq!(
        job_types,
        vec![
            "agent.spawn",
            "hook.PreToolUse",
            "hook.PostToolUse",
            "hook.PermissionRequest",
            "agent.complete",
        ]
    );
    assert!(
        job_chain.is_closed(),
        "job chain must be closed after SessionEnd"
    );
    let job_tip = job_chain.tip_hash().expect("job has tip").to_string();
    let (job_ok, _) = job_chain.verify();
    assert!(job_ok, "job chain integrity check");

    // ─── Assert project chain ──────────────────────────────────
    let project_chain_path = dir.path().join("projects/orkia-shell/seal.jsonl");
    assert!(project_chain_path.exists(), "project chain file missing");
    let project_chain = SealChain::load(project_chain_path).expect("load project");
    let proj_types: Vec<&str> = project_chain
        .records()
        .iter()
        .map(|r| r.event_type.as_str())
        .collect();
    assert_eq!(proj_types, vec!["rfc.create", "job.reference"]);

    // The job.reference record must carry the *job chain's tip
    // hash* — that's the Merkle-style cross-scope proof.
    let job_ref = project_chain.records().last().unwrap();
    assert_eq!(
        job_ref
            .detail
            .get("job_chain_hash")
            .and_then(|v| v.as_str())
            .unwrap(),
        job_tip,
        "project's job.reference must embed the job chain's tip hash",
    );
    let (proj_ok, _) = project_chain.verify();
    assert!(proj_ok, "project chain integrity check");

    // ─── Deep verify ───────────────────────────────────────────
    let result = mgr.verify_project_deep("orkia-shell");
    assert!(result.project_ok);
    assert_eq!(result.job_results.len(), 1, "one referenced job");
    let jr = &result.job_results[0];
    assert_eq!(jr.agent, "faye");
    assert_eq!(jr.job_id, 42);
    assert!(jr.chain_ok, "referenced job chain verifies");
    assert!(jr.tip_matches, "tip hash matches project record");

    // ─── Side effect: project association cleared on close ─────
    assert!(
        !projs.read().contains_key(&JobId(42)),
        "consumer should evict job from job_projects on close",
    );
}

#[test]
fn ad_hoc_agent_job_writes_only_job_chain() {
    // `@agent foo` (no project) → no project chain touched, no
    // job.reference recorded. The job chain stands on its own.
    let dir = TempDir::new().expect("tmp");
    let mut mgr = SealManager::new(dir.path().to_path_buf());
    let projs = projects();

    route_event(
        &mut mgr,
        &projs,
        &orkia_shell::seal::ScheduledContext::default(),
        evt(
            JobId(7),
            "faye",
            EventSource::Internal,
            EventPayload::Custom {
                name: "agent.spawn".into(),
                data: serde_json::json!({"system_prompt_hash": "x", "memory_hash": "y", "tools_count": 0}),
            },
        ),
    ).expect("route");
    route_event(
        &mut mgr,
        &projs,
        &orkia_shell::seal::ScheduledContext::default(),
        evt(
            JobId(7),
            "faye",
            EventSource::Hook {
                provider: "claude".into(),
            },
            EventPayload::SessionEnd { exit_code: Some(0) },
        ),
    )
    .expect("route");

    assert!(dir.path().join("agents/faye/jobs/7/seal.jsonl").exists());
    assert!(
        !dir.path().join("projects").exists()
            || std::fs::read_dir(dir.path().join("projects"))
                .map(|d| d.count())
                .unwrap_or(0)
                == 0,
        "no project directory should be created for ad-hoc spawns",
    );
}

#[test]
fn failed_session_records_agent_failed_terminal() {
    let dir = TempDir::new().expect("tmp");
    let mut mgr = SealManager::new(dir.path().to_path_buf());
    let projs = projects();

    route_event(
        &mut mgr,
        &projs,
        &orkia_shell::seal::ScheduledContext::default(),
        evt(
            JobId(8),
            "faye",
            EventSource::Internal,
            EventPayload::Custom {
                name: "agent.spawn".into(),
                data: serde_json::json!({}),
            },
        ),
    )
    .expect("route");
    route_event(
        &mut mgr,
        &projs,
        &orkia_shell::seal::ScheduledContext::default(),
        evt(
            JobId(8),
            "faye",
            EventSource::Hook {
                provider: "claude".into(),
            },
            EventPayload::SessionEnd { exit_code: Some(2) },
        ),
    )
    .expect("route");

    let chain = SealChain::load(dir.path().join("agents/faye/jobs/8/seal.jsonl")).unwrap();
    let last = chain.records().last().unwrap();
    assert_eq!(last.event_type, "agent.failed");
    assert_eq!(
        last.detail.get("exit_code").and_then(|v| v.as_i64()),
        Some(2),
    );
}
