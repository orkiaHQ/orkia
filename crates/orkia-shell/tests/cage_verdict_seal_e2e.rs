// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Proves the full path the cage relies on:
//!
//!   orkia-sh socket write  →  `JournalListener` parse  →  `convert_hook`
//!   (catch-all → `Custom`)  →  `seal::route` (catch-all → job chain)
//!   →  hash-chained record in the job's `seal.jsonl`.
//!
//! The wire line is the exact shape `orkia-sh`'s `verdict::build_envelope`
//! produces (asserted independently in that crate's unit tests): an
//! `event_type = Hook`, `event = "cage.verdict"` envelope whose verdict detail
//! (`command`/`verdict`/`capability`/`rule`) is flattened to the top level.
//!
//! Positive: the record lands with `event_type = "cage.verdict"` and the
//! verdict detail, and the chain still verifies. Negative (gotcha): a verdict
//! stamped with a `job_id` that has no chain is **silently dropped** by
//! `seal_job_with_rfc` — the test asserts the drop so a mis-wired `ORKIA_JOB_ID`
//! can never masquerade as a passing audit.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use orkia_shell::journal::{JournalListener, LiveJournalHandlers};
use orkia_shell::protocol::{EventPayload, EventRouter, EventSource, OrkiaEvent};
use orkia_shell::seal::{JobProjects, ScheduledContext, SealManager, route_event};
use orkia_shell_types::JobId;
use parking_lot::RwLock;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::time::timeout;

/// One `cage.verdict` NDJSON line exactly as `orkia-sh` writes it, stamped with
/// the given routing `job_id`. Mirrors `orkia-sh`'s `verdict::build_envelope`
/// + routing-field stamping (kept literal so a drift in either side fails here).
fn verdict_line(job_id: u32) -> String {
    serde_json::json!({
        "type": "hook",
        "timestamp": "2026-06-04T10:00:00+00:00",
        "job_id": job_id,
        "agent": "faye",
        "source": "generic",
        "event": "cage.verdict",
        "command": "git push origin main",
        "verdict": "deny",
        "capability": "git.push",
        "rule": "git push*",
    })
    .to_string()
}

/// A `cage.verdict` line for an arbitrary verdict. `capability`/`rule` are
/// `None` on a default (unmatched) decision — exactly what `verdict.rs`
/// serializes as JSON `null` (the default-verdict shape).
fn verdict_line_kind(
    job_id: u32,
    command: &str,
    verdict: &str,
    capability: Option<&str>,
    rule: Option<&str>,
) -> String {
    serde_json::json!({
        "type": "hook",
        "timestamp": "2026-06-04T10:00:00+00:00",
        "job_id": job_id,
        "agent": "faye",
        "source": "generic",
        "event": "cage.verdict",
        "command": command,
        "verdict": verdict,
        "capability": capability,
        "rule": rule,
    })
    .to_string()
}

/// Seed the genesis record so the job chain exists — the cage injects
/// `ORKIA_JOB_ID` for a job the REPL already spawned (its `agent.spawn` already
/// created the chain). Without this, the silent-drop swallows every append.
fn seed_job_chain(mgr: &mut SealManager, projects: &JobProjects, job_id: u32) {
    route_event(
        mgr,
        projects,
        &ScheduledContext::default(),
        OrkiaEvent {
            source: EventSource::Internal,
            event: EventPayload::Custom {
                name: "agent.spawn".into(),
                data: serde_json::json!({}),
            },
            confidence: 1.0,
            timestamp: chrono::Utc::now(),
            job_id: JobId(job_id),
            agent_name: "faye".into(),
            rfc_id: None,
        },
    )
    .expect("genesis append");
}

/// Drive the socket → listener → converter leg: write `line`, then return the
/// `OrkiaEvent` the router produced (or fail on timeout).
async fn ingest(
    socket_line: &str,
    listener: &JournalListener,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<OrkiaEvent>,
) -> OrkiaEvent {
    let mut client = UnixStream::connect(listener.socket_path())
        .await
        .expect("connect socket");
    client
        .write_all(format!("{socket_line}\n").as_bytes())
        .await
        .expect("write line");
    client.shutdown().await.ok();
    timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("router event within timeout")
        .expect("router event present")
}

#[tokio::test]
async fn cage_verdict_lands_hash_chained_in_job_seal_jsonl() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Router installed on the listener: every parsed envelope fires
    // `on_hook` → `convert_hook` → an `OrkiaEvent` on `events_rx`.
    let (router, mut events_rx) = EventRouter::new_with_rx();
    let live = LiveJournalHandlers {
        router: Some(
            std::sync::Arc::new(router) as std::sync::Arc<dyn orkia_shell::journal::HookRouter>
        ),
        ..Default::default()
    };
    let (listener, _drain_rx) =
        JournalListener::start_with_handlers(dir.path(), live).expect("listener start");

    let mut mgr = SealManager::new(dir.path().to_path_buf());
    let projects: JobProjects = Arc::new(RwLock::new(HashMap::new()));
    seed_job_chain(&mut mgr, &projects, 7);
    let genesis_len = mgr.job_chain(JobId(7)).expect("seeded chain").len();
    assert_eq!(
        genesis_len, 1,
        "genesis is the only record before the verdict"
    );

    // Socket → listener → converter.
    let event = ingest(&verdict_line(7), &listener, &mut events_rx).await;
    match &event.event {
        EventPayload::Custom { name, .. } => assert_eq!(name, "cage.verdict"),
        other => panic!("expected Custom(cage.verdict), got {other:?}"),
    }
    assert_eq!(event.job_id, JobId(7), "routing job id survived the wire");

    // Converter → consumer → seal.jsonl.
    route_event(&mut mgr, &projects, &ScheduledContext::default(), event).expect("route verdict");

    let chain = mgr.job_chain(JobId(7)).expect("chain present");
    assert_eq!(chain.len(), 2, "verdict appended after genesis");
    let rec = &chain.records()[1];
    assert_eq!(rec.event_type, "cage.verdict");
    assert_eq!(
        rec.detail.get("verdict").and_then(|v| v.as_str()),
        Some("deny")
    );
    assert_eq!(
        rec.detail.get("capability").and_then(|v| v.as_str()),
        Some("git.push"),
    );
    assert_eq!(
        rec.detail.get("command").and_then(|v| v.as_str()),
        Some("git push origin main"),
    );

    // The append is durable + hash-chained.
    let (ok, _tip) = chain.verify();
    assert!(ok, "job chain must still verify after the verdict append");
    let seal_path = dir
        .path()
        .join("agents")
        .join("faye")
        .join("jobs")
        .join("7")
        .join("seal.jsonl");
    let body = std::fs::read_to_string(&seal_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", seal_path.display()));
    assert!(
        body.contains("cage.verdict"),
        "seal.jsonl on disk holds the verdict: {body}",
    );
}

#[tokio::test]
async fn wrong_job_id_is_silently_dropped_and_detected() {
    // the job chain does not exist. A verdict stamped with an unknown job_id is
    // therefore lost — this test asserts the loss so the positive test can never
    // be a false pass from a mis-wired ORKIA_JOB_ID.
    let dir = tempfile::tempdir().expect("tempdir");
    let (router, mut events_rx) = EventRouter::new_with_rx();
    let live = LiveJournalHandlers {
        router: Some(
            std::sync::Arc::new(router) as std::sync::Arc<dyn orkia_shell::journal::HookRouter>
        ),
        ..Default::default()
    };
    let (listener, _drain_rx) =
        JournalListener::start_with_handlers(dir.path(), live).expect("listener start");

    let mut mgr = SealManager::new(dir.path().to_path_buf());
    let projects: JobProjects = Arc::new(RwLock::new(HashMap::new()));
    // Seed job 7 — but the verdict below is stamped for job 999 (no chain).
    seed_job_chain(&mut mgr, &projects, 7);

    let event = ingest(&verdict_line(999), &listener, &mut events_rx).await;
    assert_eq!(event.job_id, JobId(999));
    route_event(&mut mgr, &projects, &ScheduledContext::default(), event).expect("route (no-op)");

    // The mis-stamped verdict reached no chain.
    assert!(
        mgr.job_chain(JobId(999)).is_none(),
        "no chain is created for an unknown job id",
    );
    // And it did not contaminate the real job 7 chain (still genesis-only).
    assert_eq!(
        mgr.job_chain(JobId(7)).expect("chain 7").len(),
        1,
        "verdict for job 999 must not land on job 7",
    );
}

/// Set up listener + seeded job-7 chain; returns the pieces the verdict tests
/// drive. Mirrors the positive test's preamble so each variant stays focused on
/// its own assertion.
async fn fixture() -> (
    tempfile::TempDir,
    JournalListener,
    tokio::sync::mpsc::UnboundedReceiver<OrkiaEvent>,
    SealManager,
    JobProjects,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (router, events_rx) = EventRouter::new_with_rx();
    let live = LiveJournalHandlers {
        router: Some(
            std::sync::Arc::new(router) as std::sync::Arc<dyn orkia_shell::journal::HookRouter>
        ),
        ..Default::default()
    };
    let (listener, _drain_rx) =
        JournalListener::start_with_handlers(dir.path(), live).expect("listener start");
    let mut mgr = SealManager::new(dir.path().to_path_buf());
    let projects: JobProjects = Arc::new(RwLock::new(HashMap::new()));
    seed_job_chain(&mut mgr, &projects, 7);
    (dir, listener, events_rx, mgr, projects)
}

#[tokio::test]
async fn allow_verdict_is_recorded() {
    let (_dir, listener, mut rx, mut mgr, projects) = fixture().await;
    let line = verdict_line_kind(
        7,
        "git commit -m x",
        "allow",
        Some("git.commit"),
        Some("git commit*"),
    );
    let event = ingest(&line, &listener, &mut rx).await;
    route_event(&mut mgr, &projects, &ScheduledContext::default(), event).expect("route allow");

    let chain = mgr.job_chain(JobId(7)).expect("chain");
    assert_eq!(chain.len(), 2, "allow verdict appended");
    let rec = &chain.records()[1];
    assert_eq!(rec.event_type, "cage.verdict");
    assert_eq!(
        rec.detail.get("verdict").and_then(|v| v.as_str()),
        Some("allow")
    );
    let (ok, _tip) = chain.verify();
    assert!(ok, "chain verifies after an allow verdict");
}

#[tokio::test]
async fn ask_defaulted_verdict_is_recorded() {
    // `ask`; the record lands with `verdict:"ask"` and null capability/rule.
    let (_dir, listener, mut rx, mut mgr, projects) = fixture().await;
    let line = verdict_line_kind(7, "some-unmatched-tool --flag", "ask", None, None);
    let event = ingest(&line, &listener, &mut rx).await;
    route_event(&mut mgr, &projects, &ScheduledContext::default(), event).expect("route ask");

    let chain = mgr.job_chain(JobId(7)).expect("chain");
    assert_eq!(chain.len(), 2, "ask verdict appended");
    let rec = &chain.records()[1];
    assert_eq!(rec.event_type, "cage.verdict");
    assert_eq!(
        rec.detail.get("verdict").and_then(|v| v.as_str()),
        Some("ask")
    );
    // Default verdict carries no capability/rule — null, not absent-then-wrong.
    assert!(
        rec.detail
            .get("capability")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "ask-default has null capability, got {:?}",
        rec.detail.get("capability")
    );
    let (ok, _tip) = chain.verify();
    assert!(ok, "chain verifies after an ask verdict");
}

#[tokio::test]
async fn chain_verifies_after_several_verdicts() {
    // hash-linked end to end (genesis → deny → allow → ask) and verifies.
    let (_dir, listener, mut rx, mut mgr, projects) = fixture().await;
    let lines = [
        verdict_line_kind(
            7,
            "git push origin main",
            "deny",
            Some("git.push"),
            Some("git push*"),
        ),
        verdict_line_kind(
            7,
            "git commit -m x",
            "allow",
            Some("git.commit"),
            Some("git commit*"),
        ),
        verdict_line_kind(7, "weird-tool", "ask", None, None),
    ];
    for line in &lines {
        let event = ingest(line, &listener, &mut rx).await;
        route_event(&mut mgr, &projects, &ScheduledContext::default(), event).expect("route");
    }

    let chain = mgr.job_chain(JobId(7)).expect("chain");
    assert_eq!(chain.len(), 4, "genesis + three verdicts");
    let (ok, _tip) = chain.verify();
    assert!(ok, "multi-verdict chain verifies (prev_hash links hold)");
}
