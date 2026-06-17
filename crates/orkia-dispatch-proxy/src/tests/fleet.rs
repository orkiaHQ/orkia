// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file for terms.

//! The fleet-level integration oracle (SPEC-FLEET-CONVERGENCE-V2, increment 1).
//! Once the DAG drains, the proxy runs the RFC-level `[dispatch].accept`, seals
//! a `GlobalVerdict`, and closes the run `converged` (pass) or `integration
//! failed` (fail). The re-plan loop (increment 2) is not wired yet — this is the
//! fleet verdict + provenance the brain will consume.

use orkia_rfc_core::DecisionKind;
use orkia_shell_types::dispatch_kernel::DispatchAuthorizeResponse;

use super::support::*;
use crate::DispatchStartOutcome;
use crate::issues::Status;

fn one_task() -> DispatchAuthorizeResponse {
    DispatchAuthorizeResponse::Authorized {
        run_id: "r-100".into(),
        total_tasks: 1,
        wave: vec![plan("r-100", "t-a", "faye", &[])],
    }
}

fn global_verdicts(pd: &std::path::Path) -> Vec<crate::seal::DispatchSealRecord> {
    seal_records(pd)
        .into_iter()
        .filter(|r| r.kind == DecisionKind::GlobalVerdict)
        .collect()
}

/// The DAG drains and the integration oracle passes → run `converged`, one
/// `GlobalVerdict(round 0, passed=true)` sealed onto the chain.
#[test]
fn global_oracle_pass_marks_converged_and_seals() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    let f = Fakes::new(one_task(), vec![]); // advance → Completed
    let proxy = f.proxy();

    let out = proxy.start_run(request_with_global_accept(
        pd,
        "exit 0",
        vec![task("t-a", "faye", &[])],
    ));
    assert!(matches!(out, DispatchStartOutcome::Started { .. }));

    assert!(wait_for(|| status_is(pd, "t-a", Status::Spawned)));
    let p = write_response(pd, "a.txt", "done");
    f.responses.fire(done_event(1, "faye", p));

    assert!(wait_for(|| run_closed(pd).as_deref() == Some("converged")));
    let gv = global_verdicts(pd);
    assert_eq!(gv.len(), 1);
    assert_eq!(gv[0].passed, Some(true));
    assert_eq!(gv[0].round, Some(0));
    assert_eq!(gv[0].exit_code, Some(0));
}

/// The DAG drains but the integration oracle fails → run `integration failed`,
/// one `GlobalVerdict(passed=false)` sealed. (Increment 1: no re-plan yet.)
#[test]
fn global_oracle_fail_marks_integration_failed_and_seals() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    let f = Fakes::new(one_task(), vec![]);
    let proxy = f.proxy();

    let out = proxy.start_run(request_with_global_accept(
        pd,
        "exit 3",
        vec![task("t-a", "faye", &[])],
    ));
    assert!(matches!(out, DispatchStartOutcome::Started { .. }));

    assert!(wait_for(|| status_is(pd, "t-a", Status::Spawned)));
    let p = write_response(pd, "a.txt", "done");
    f.responses.fire(done_event(1, "faye", p));

    assert!(wait_for(
        || run_closed(pd).is_some_and(|r| r.starts_with("integration failed"))
    ));
    let gv = global_verdicts(pd);
    assert_eq!(gv.len(), 1);
    assert_eq!(gv[0].passed, Some(false));
    assert_eq!(gv[0].exit_code, Some(3));
    // Tasks individually finished even though the integration verdict failed.
    assert!(status_is(pd, "t-a", Status::Done));
}

/// No `[dispatch].accept` → unchanged: the run closes `completed`, no
/// `GlobalVerdict` is sealed.
#[test]
fn no_global_accept_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    let f = Fakes::new(one_task(), vec![]);
    let proxy = f.proxy();

    let out = proxy.start_run(request(pd, vec![task("t-a", "faye", &[])]));
    assert!(matches!(out, DispatchStartOutcome::Started { .. }));

    assert!(wait_for(|| status_is(pd, "t-a", Status::Spawned)));
    let p = write_response(pd, "a.txt", "done");
    f.responses.fire(done_event(1, "faye", p));

    assert!(wait_for(|| run_closed(pd).as_deref() == Some("completed")));
    assert_eq!(global_verdicts(pd).len(), 0);
}
