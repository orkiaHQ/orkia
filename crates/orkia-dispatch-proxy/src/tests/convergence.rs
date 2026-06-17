// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file for terms.

//! The per-task convergence loop (SPEC-CONVERGENCE-LOOP-V1): a task with an
//! `accept` oracle runs the agent, then the oracle; on pass it is `Verified`
//! (kernel sees `Done`), on fail it re-spawns a self-repair attempt up to
//! `max_attempts`, else `Rejected` (kernel sees `Failed`). The kernel sees
//! exactly ONE outcome per task — retries never reach it.

use orkia_rfc_core::DecisionKind;
use orkia_shell_types::dispatch_kernel::{DispatchAdvanceResponse, DispatchAuthorizeResponse};

use super::support::*;
use crate::issues::Status;
use crate::{DispatchStartOutcome, ResumeOutcome};

fn one_task_authorize() -> DispatchAuthorizeResponse {
    DispatchAuthorizeResponse::Authorized {
        run_id: "r-001".into(),
        total_tasks: 1,
        wave: vec![plan("r-001", "t-a", "faye", &[])],
    }
}

/// Oracle passes on the first run → one spawn, `Verified`, a single kernel
/// advance (Done), and a passed verdict sealed.
#[test]
fn pass_first_verifies_without_retry() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    let f = Fakes::new(one_task_authorize(), vec![]); // empty → advance returns Completed
    let proxy = f.proxy();

    let out = proxy.start_run(request(
        pd,
        vec![task_with_accept("t-a", "faye", &[], "exit 0", 3)],
    ));
    assert!(matches!(out, DispatchStartOutcome::Started { .. }));

    assert!(wait_for(|| status_is(pd, "t-a", Status::Spawned)));
    let p = write_response(pd, "a.txt", "done");
    f.responses.fire(done_event(1, "faye", p));

    assert!(wait_for(|| status_is(pd, "t-a", Status::Verified)));
    assert!(wait_for(|| run_closed(pd).is_some()));
    assert_eq!(f.spawner.count(), 1, "no retry");
    assert_eq!(f.kernel.advance_log.lock().unwrap().len(), 1, "one outcome");

    let recs = seal_records(pd);
    let verdicts: Vec<_> = recs
        .iter()
        .filter(|r| r.kind == DecisionKind::AcceptanceVerdict)
        .collect();
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].passed, Some(true));
    assert!(read_issue(pd, "t-a").unwrap().meta.verdict_seal.is_some());
}

/// Oracle fails once then passes (a marker file flips it) → two spawns,
/// `Verified`, still ONE kernel advance, and two sealed verdicts (fail→pass).
#[test]
fn fail_then_pass_retries_then_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    let marker = pd.join("marker");
    // First run: no marker → create it + fail. Second run: marker → pass.
    let accept = format!(
        "test -f {m} && exit 0 || {{ touch {m}; exit 1; }}",
        m = marker.display()
    );
    let f = Fakes::new(one_task_authorize(), vec![]);
    let proxy = f.proxy();

    let out = proxy.start_run(request(
        pd,
        vec![task_with_accept("t-a", "faye", &[], &accept, 3)],
    ));
    assert!(matches!(out, DispatchStartOutcome::Started { .. }));

    // Attempt 0: agent finishes (job 1) → oracle fails → retry as job 2.
    assert!(wait_for(|| status_is(pd, "t-a", Status::Spawned)));
    let p1 = write_response(pd, "a1.txt", "attempt 0");
    f.responses.fire(done_event(1, "faye", p1));
    assert!(wait_for(
        || attempt_is(pd, "t-a", 1) && status_is(pd, "t-a", Status::Spawned)
    ));
    assert_eq!(f.spawner.count(), 2, "one retry spawned");

    // Attempt 1: agent finishes (job 2) → oracle passes → Verified.
    let p2 = write_response(pd, "a2.txt", "attempt 1");
    f.responses.fire(done_event(2, "faye", p2));
    assert!(wait_for(|| status_is(pd, "t-a", Status::Verified)));
    assert!(wait_for(|| run_closed(pd).is_some()));

    // The kernel saw exactly one outcome — the converged Done, not the retry.
    assert_eq!(f.kernel.advance_log.lock().unwrap().len(), 1);

    // Provenance: output(0) → verdict(0,fail) → output(1) → verdict(1,pass).
    let verdicts: Vec<_> = seal_records(pd)
        .into_iter()
        .filter(|r| r.kind == DecisionKind::AcceptanceVerdict)
        .collect();
    assert_eq!(verdicts.len(), 2);
    assert_eq!(verdicts[0].passed, Some(false));
    assert_eq!(verdicts[0].attempt, Some(0));
    assert_eq!(verdicts[1].passed, Some(true));
    assert_eq!(verdicts[1].attempt, Some(1));
}

/// Oracle never passes → spawns `max_attempts` times, then `Rejected` with one
/// `Failed` advance to the kernel.
#[test]
fn fails_max_attempts_then_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    let f = Fakes::new(
        one_task_authorize(),
        vec![DispatchAdvanceResponse::Paused {
            failed: vec!["t-a".into()],
        }],
    );
    let proxy = f.proxy();

    let out = proxy.start_run(request(
        pd,
        vec![task_with_accept("t-a", "faye", &[], "exit 1", 2)],
    ));
    assert!(matches!(out, DispatchStartOutcome::Started { .. }));

    // Attempt 0 fails → retry (attempt 1).
    assert!(wait_for(|| status_is(pd, "t-a", Status::Spawned)));
    let p1 = write_response(pd, "a1.txt", "attempt 0");
    f.responses.fire(done_event(1, "faye", p1));
    assert!(wait_for(|| attempt_is(pd, "t-a", 1)));

    // Attempt 1 fails → exhausted → Rejected.
    let p2 = write_response(pd, "a2.txt", "attempt 1");
    f.responses.fire(done_event(2, "faye", p2));
    assert!(wait_for(|| status_is(pd, "t-a", Status::Rejected)));
    // Wait for the run to close (the Failed advance → Paused → close_run) so the
    // advance has definitely happened before we read its log (no race).
    assert!(wait_for(|| run_closed(pd).is_some()));
    assert_eq!(f.spawner.count(), 2, "max_attempts spawns");
    assert_eq!(f.kernel.advance_log.lock().unwrap().len(), 1, "one Failed advance");

    let verdicts: Vec<_> = seal_records(pd)
        .into_iter()
        .filter(|r| r.kind == DecisionKind::AcceptanceVerdict)
        .collect();
    assert_eq!(verdicts.len(), 2);
    assert!(verdicts.iter().all(|v| v.passed == Some(false)));
}

/// A task WITHOUT an `accept` oracle is unchanged: `Done` on finish, no oracle,
/// no verdict records.
#[test]
fn no_accept_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    let f = Fakes::new(one_task_authorize(), vec![]);
    let proxy = f.proxy();

    let out = proxy.start_run(request(pd, vec![task("t-a", "faye", &[])]));
    assert!(matches!(out, DispatchStartOutcome::Started { .. }));

    assert!(wait_for(|| status_is(pd, "t-a", Status::Spawned)));
    let p = write_response(pd, "a.txt", "done");
    f.responses.fire(done_event(1, "faye", p));

    assert!(wait_for(|| status_is(pd, "t-a", Status::Done)));
    assert_eq!(f.spawner.count(), 1);
    let verdicts = seal_records(pd)
        .into_iter()
        .filter(|r| r.kind == DecisionKind::AcceptanceVerdict)
        .count();
    assert_eq!(verdicts, 0, "no oracle, no verdict");
}

/// Resume safety (Phase 6): a task left `Verifying` (agent finished, verdict
/// never landed before the restart) re-runs the oracle on resume — idempotent,
/// no re-spawn of the finished agent, one converged advance.
#[test]
fn resume_reruns_oracle_for_verifying_task() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    seed_run(pd, None);
    seed_issue(
        pd,
        "impl",
        "faye",
        &[],
        Status::Verifying,
        Some(1),
        Some("agent output"),
    );
    let f = Fakes::new(
        DispatchAuthorizeResponse::Authorized {
            run_id: "r-003".into(),
            total_tasks: 1,
            wave: vec![plan("r-003", "impl", "faye", &[])],
        },
        vec![], // advance → Completed
    );
    let proxy = f.proxy();

    let out = proxy.resume_run(request(
        pd,
        vec![task_with_accept("impl", "faye", &[], "exit 0", 3)],
    ));
    assert!(matches!(out, ResumeOutcome::Resumed { .. }));

    assert!(wait_for(|| status_is(pd, "impl", Status::Verified)));
    assert!(wait_for(|| run_closed(pd).is_some()));
    assert_eq!(f.spawner.count(), 0, "resume must not re-spawn a finished agent");
    assert_eq!(f.kernel.advance_log.lock().unwrap().len(), 1);
}
