// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::path::Path;

use orkia_shell_types::job::JobId;

use super::*;
use tempfile::tempdir;

fn mgr(dir: &Path) -> SealManager {
    SealManager::new(dir.to_path_buf())
}

#[test]
fn create_job_chain_writes_under_agent_dir() {
    let dir = tempdir().unwrap();
    let mut m = mgr(dir.path());
    let chain = m.create_job_chain(JobId(1), "faye");
    chain
        .append("agent.spawn", serde_json::json!({"tools_count": 3}))
        .expect("append");
    let expected = dir.path().join("agents/faye/jobs/1/seal.jsonl");
    assert!(expected.exists(), "expected file at {}", expected.display());
}

#[test]
fn create_job_chain_archives_previous_job_at_same_path() {
    let dir = tempdir().unwrap();

    // Old process: job 1 runs to terminal agent.complete.
    let mut old = mgr(dir.path());
    old.create_job_chain(JobId(1), "sage")
        .append("agent.spawn", serde_json::json!({}))
        .expect("genesis");
    old.seal_job(
        JobId(1),
        "agent.complete",
        serde_json::json!({"exit_code": 0}),
    )
    .expect("terminal");

    // New process: job ids restart at 1 → same path. Genesis must
    // start a fresh OPEN chain, not load the closed old one.
    let mut new = mgr(dir.path());
    new.create_job_chain(JobId(1), "sage")
        .append("agent.spawn", serde_json::json!({}))
        .expect("genesis on fresh chain must append, not silently drop");
    new.seal_job(JobId(1), "hook.PreToolUse", serde_json::json!({}))
        .expect("seal");

    let path = dir.path().join("agents/sage/jobs/1/seal.jsonl");
    let archive = dir.path().join("agents/sage/jobs/1/seal.jsonl.1");
    assert!(archive.exists(), "old chain archived to seal.jsonl.1");
    let live = SealChain::load(path).expect("load live chain");
    assert_eq!(live.len(), 2, "live file holds only the new chain");
    let archived = SealChain::load(archive).expect("load archived chain");
    assert_eq!(archived.len(), 2, "archived chain complete");
    assert!(archived.verify().0, "archived chain still verifies");
}

#[test]
fn open_job_chain_continues_existing_chain_without_archiving() {
    let dir = tempdir().unwrap();

    // Genesis in one manager (one process / one event)…
    let mut first = mgr(dir.path());
    first
        .create_job_chain(JobId(1), "daemon")
        .append("detached.spawn", serde_json::json!({}))
        .expect("genesis");

    // …continued by a FRESH manager per follow-up event, like the
    // daemon audit trail does.
    let mut second = mgr(dir.path());
    second.open_job_chain(JobId(1), "daemon");
    second
        .seal_job(JobId(1), "detached.tell", serde_json::json!({}))
        .expect("seal");

    let path = dir.path().join("agents/daemon/jobs/1/seal.jsonl");
    let archive = dir.path().join("agents/daemon/jobs/1/seal.jsonl.1");
    assert!(!archive.exists(), "open must not archive the live chain");
    let live = SealChain::load(path).expect("load live chain");
    assert_eq!(live.len(), 2, "both events on one chain");
    assert!(live.verify().0, "continued chain verifies");
}

#[test]
fn origin_tag_lands_on_appended_records() {
    let dir = tempdir().unwrap();
    let mut m = SealManager::with_origin(dir.path().to_path_buf(), Some("scheduled".to_string()));
    m.create_job_chain(JobId(42), "faye");
    m.seal_job(
        JobId(42),
        "hook.PreToolUse",
        serde_json::json!({ "tool": "Read" }),
    )
    .expect("seal");
    let chain = m.job_chain(JobId(42)).expect("chain present");
    let detail = &chain.records()[0].detail;
    assert_eq!(
        detail.get("origin").and_then(|v| v.as_str()),
        Some("scheduled"),
        "origin tag missing from detail JSON",
    );
    assert_eq!(detail.get("tool").and_then(|v| v.as_str()), Some("Read"));
}

#[test]
fn no_origin_tag_when_disabled() {
    let dir = tempdir().unwrap();
    let mut m = SealManager::with_origin(dir.path().to_path_buf(), None);
    m.create_job_chain(JobId(43), "faye");
    m.seal_job(JobId(43), "hook.PreToolUse", serde_json::json!({}))
        .expect("seal");
    let chain = m.job_chain(JobId(43)).expect("chain");
    assert!(chain.records()[0].detail.get("origin").is_none());
}

#[test]
fn seal_job_after_close_is_noop() {
    let dir = tempdir().unwrap();
    let mut m = mgr(dir.path());
    m.create_job_chain(JobId(2), "faye");
    m.seal_job(JobId(2), "hook.PreToolUse", serde_json::json!({}))
        .expect("seal");
    let tip_a = m.close_job_chain(JobId(2)).expect("tip");
    // After close, further seals must not change the chain.
    m.seal_job(JobId(2), "hook.late", serde_json::json!({}))
        .expect("noop seal after close");
    let chain = m.job_chain(JobId(2)).expect("still in mem");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain.tip_hash().unwrap(), tip_a);
}

#[test]
fn close_then_job_reference_links_scopes() {
    let dir = tempdir().unwrap();
    let mut m = mgr(dir.path());
    m.create_job_chain(JobId(3), "faye");
    m.seal_job(JobId(3), "hook.PreToolUse", serde_json::json!({}))
        .expect("seal");
    m.seal_job(
        JobId(3),
        "agent.complete",
        serde_json::json!({"exit_code": 0}),
    )
    .expect("seal");
    let tip = m.close_job_chain(JobId(3)).expect("tip");

    m.seal_job_reference("orkia-shell", JobId(3), "faye", &tip)
        .expect("ref");
    let project = m
        .project_chain("orkia-shell")
        .expect("project chain exists");
    assert_eq!(project.len(), 1);
    let last = &project.records()[0];
    assert_eq!(last.event_type, "job.reference");
    assert_eq!(
        last.detail
            .get("job_chain_hash")
            .and_then(|v| v.as_str())
            .unwrap(),
        tip
    );
}

#[test]
fn deep_verify_passes_for_clean_chains() {
    let dir = tempdir().unwrap();
    let mut m = mgr(dir.path());

    // Build a project with two delegated jobs.
    m.seal_project(
        "orkia-shell",
        "rfc.create",
        serde_json::json!({"slug": "x"}),
    )
    .expect("seal");

    m.create_job_chain(JobId(10), "faye");
    m.seal_job(JobId(10), "hook.PreToolUse", serde_json::json!({}))
        .expect("seal");
    m.seal_job(
        JobId(10),
        "agent.complete",
        serde_json::json!({"exit_code": 0}),
    )
    .expect("seal");
    let tip_a = m.close_job_chain(JobId(10)).unwrap();
    m.seal_job_reference("orkia-shell", JobId(10), "faye", &tip_a)
        .expect("ref");
    m.evict_job_chain(JobId(10));

    m.create_job_chain(JobId(11), "sage");
    m.seal_job(
        JobId(11),
        "agent.complete",
        serde_json::json!({"exit_code": 0}),
    )
    .expect("seal");
    let tip_b = m.close_job_chain(JobId(11)).unwrap();
    m.seal_job_reference("orkia-shell", JobId(11), "sage", &tip_b)
        .expect("ref");
    m.evict_job_chain(JobId(11));

    let result = m.verify_project_deep("orkia-shell");
    assert!(result.project_ok);
    assert!(result.project_broken_at.is_none());
    assert_eq!(result.job_results.len(), 2);
    for jr in &result.job_results {
        assert!(
            jr.chain_ok,
            "job {} chain broken at {:?}",
            jr.job_id, jr.broken_at
        );
        assert!(jr.tip_matches, "job {} tip mismatch", jr.job_id);
    }
}

#[test]
fn deep_verify_detects_in_chain_tamper() {
    // Direct mutation of a record's `detail` flips the
    // recomputed hash → `chain_ok` fails. `tip_matches`
    // doesn't necessarily flip because we only changed the
    // detail, not the stored `hash` field — that's fine, the
    // project-side proof for this kind of tamper is the
    // chain-internal verify failing.
    let dir = tempdir().unwrap();
    let mut m = mgr(dir.path());

    m.create_job_chain(JobId(20), "faye");
    m.seal_job(
        JobId(20),
        "agent.complete",
        serde_json::json!({"exit_code": 0}),
    )
    .expect("seal");
    let tip = m.close_job_chain(JobId(20)).unwrap();
    m.seal_job_reference("orkia-shell", JobId(20), "faye", &tip)
        .expect("ref");
    m.evict_job_chain(JobId(20));

    let job_path = dir.path().join("agents/faye/jobs/20/seal.jsonl");
    let content = std::fs::read_to_string(&job_path).unwrap();
    let tampered = content.replace("\"exit_code\":0", "\"exit_code\":1");
    std::fs::write(&job_path, tampered).unwrap();

    let result = m.verify_project_deep("orkia-shell");
    assert!(result.project_ok);
    let jr = &result.job_results[0];
    assert!(!jr.chain_ok, "tampered chain must fail internal verify");
}

#[test]
fn deep_verify_detects_wholesale_chain_replacement() {
    // The more devious tamper: someone replaces the entire
    // job chain file with a *different* internally-consistent
    // chain. Internal verify passes (the new chain is
    // self-consistent), but the new tip hash differs from
    // what the project chain recorded → `tip_matches` is
    // the load-bearing signal.
    let dir = tempdir().unwrap();
    let mut m = mgr(dir.path());

    m.create_job_chain(JobId(21), "faye");
    m.seal_job(
        JobId(21),
        "hook.PreToolUse",
        serde_json::json!({"tool": "Read"}),
    )
    .expect("seal");
    m.seal_job(
        JobId(21),
        "agent.complete",
        serde_json::json!({"exit_code": 0}),
    )
    .expect("seal");
    let original_tip = m.close_job_chain(JobId(21)).unwrap();
    m.seal_job_reference("orkia-shell", JobId(21), "faye", &original_tip)
        .expect("ref");
    m.evict_job_chain(JobId(21));

    // Replace with a chain that has different content but is
    // still internally valid.
    let job_path = dir.path().join("agents/faye/jobs/21/seal.jsonl");
    std::fs::remove_file(&job_path).unwrap();
    let mut replacement = SealChain::create(job_path.clone()).unwrap();
    replacement
        .append("hook.PreToolUse", serde_json::json!({"tool": "Write"}))
        .expect("append");
    replacement
        .append("agent.complete", serde_json::json!({"exit_code": 0}))
        .expect("append");
    let new_tip = replacement.tip_hash().unwrap().to_string();
    assert_ne!(new_tip, original_tip);

    let result = m.verify_project_deep("orkia-shell");
    assert!(result.project_ok);
    let jr = &result.job_results[0];
    assert!(jr.chain_ok, "replacement chain is internally consistent");
    assert!(
        !jr.tip_matches,
        "but its tip diverges from the project reference"
    );
}

#[test]
fn verify_unknown_project_returns_empty_failure() {
    let dir = tempdir().unwrap();
    let m = mgr(dir.path());
    let (ok, broken, len) = m.verify_project("never-existed");
    // Missing file → load returns empty chain → verify passes.
    // record_count is 0 — that's fine, the caller decides if
    // empty counts as success.
    assert!(ok);
    assert_eq!(broken, None);
    assert_eq!(len, 0);
}
