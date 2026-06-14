// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file for terms.

//! A clean run from [`KernelDispatchProxy::start_run`]: every issue begins
//! `Pending`, so the actor spawns each wave and folds responses back in.

use orkia_shell_types::dispatch_kernel::{DispatchAdvanceResponse, DispatchAuthorizeResponse};

use super::support::*;
use crate::DispatchStartOutcome;
use crate::issues::Status;

/// The happy path: a two-task chain (`t-a` → `t-b`) spawns wave by wave, each
/// response advances the kernel, and `t-b`'s prompt embeds `t-a`'s output.
#[test]
fn linear_run_spawns_completes_and_records() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    let f = Fakes::new(
        DispatchAuthorizeResponse::Authorized {
            run_id: "r-001".into(),
            total_tasks: 2,
            wave: vec![plan("r-001", "t-a", "faye", &[])],
        },
        vec![
            DispatchAdvanceResponse::NextWave {
                wave: vec![plan("r-001", "t-b", "sage", &["t-a"])],
            },
            DispatchAdvanceResponse::Completed { elapsed_ms: 0 },
        ],
    );
    let proxy = f.proxy();

    let out = proxy.start_run(request(
        pd,
        vec![task("t-a", "faye", &[]), task("t-b", "sage", &["t-a"])],
    ));
    let DispatchStartOutcome::Started { total_tasks, .. } = out else {
        panic!("expected Started");
    };
    assert_eq!(total_tasks, 2);

    // t-a spawns as job 1; feed its response and watch t-b cascade in as job 2.
    assert!(wait_for(|| status_is(pd, "t-a", Status::Spawned)));
    let pa = write_response(pd, "a.txt", "ALPHA DONE");
    f.responses.fire(done_event(1, "faye", pa));

    assert!(wait_for(|| status_is(pd, "t-b", Status::Spawned)));
    let pb = write_response(pd, "b.txt", "BETA DONE");
    f.responses.fire(done_event(2, "sage", pb));

    assert!(wait_for(|| run_closed(pd).as_deref() == Some("completed")));
    assert!(status_is(pd, "t-a", Status::Done));
    assert!(status_is(pd, "t-b", Status::Done));
    assert_eq!(f.spawner.count(), 2);

    // The dependency's captured output is composed into the dependent's prompt.
    let t_b = read_issue(pd, "t-b").unwrap();
    assert!(t_b.prompt.contains("ALPHA DONE"));
    let t_a = read_issue(pd, "t-a").unwrap();
    assert_eq!(t_a.response.as_deref(), Some("ALPHA DONE"));

    // `done` is backed by the dispatch SEAL chain, not just the markdown flag:
    // each completed issue carries the hash of its chain record, and the chain
    // verifies link-by-link with one record per completed task.
    assert!(t_a.meta.seal.is_some());
    assert!(t_b.meta.seal.is_some());
    let chain = crate::seal::DispatchSeal::new(pd);
    assert!(chain.verify().unwrap());
    let records = chain.records().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(t_a.meta.seal.as_deref(), Some(records[0].hash.as_str()));
    assert_eq!(t_b.meta.seal.as_deref(), Some(records[1].hash.as_str()));
}

/// An agent the shell can't resolve refuses the run before the kernel is even
/// contacted — and nothing spawns (§8 fail-closed).
#[test]
fn unresolvable_agent_refuses_before_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fakes::new(
        DispatchAuthorizeResponse::Authorized {
            run_id: "r-001".into(),
            total_tasks: 1,
            wave: vec![],
        },
        vec![],
    );
    let proxy = f.proxy();

    let out = proxy.start_run(request(dir.path(), vec![task("t-a", "ghost", &[])]));
    let DispatchStartOutcome::Refused { reason } = out else {
        panic!("expected Refused");
    };
    assert!(reason.contains("ghost"), "reason was {reason:?}");
    assert_eq!(f.spawner.count(), 0);
}

/// A kernel refusal (policy / not entitled) is surfaced verbatim, not swallowed.
#[test]
fn kernel_refusal_is_surfaced() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fakes::new(
        DispatchAuthorizeResponse::Refused {
            reason: "team plan required".into(),
        },
        vec![],
    );
    let proxy = f.proxy();

    let out = proxy.start_run(request(dir.path(), vec![task("t-a", "faye", &[])]));
    let DispatchStartOutcome::Refused { reason } = out else {
        panic!("expected Refused");
    };
    assert!(
        reason.contains("team plan required"),
        "reason was {reason:?}"
    );
    assert_eq!(f.spawner.count(), 0);
}

/// A `Stop` that captured no output fails the task rather than recording an
/// empty `Done` — and the kernel's pause closes the run.
#[test]
fn stop_without_response_fails_the_task() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    let f = Fakes::new(
        DispatchAuthorizeResponse::Authorized {
            run_id: "r-001".into(),
            total_tasks: 1,
            wave: vec![plan("r-001", "t-a", "faye", &[])],
        },
        vec![DispatchAdvanceResponse::Paused {
            failed: vec!["t-a".into()],
        }],
    );
    let proxy = f.proxy();

    let out = proxy.start_run(request(pd, vec![task("t-a", "faye", &[])]));
    assert!(matches!(out, DispatchStartOutcome::Started { .. }));

    assert!(wait_for(|| status_is(pd, "t-a", Status::Spawned)));
    f.responses.fire(failed_event(1, "faye", "agent crashed"));

    assert!(wait_for(|| run_closed(pd).as_deref() == Some("paused: t-a")));
    assert!(status_is(pd, "t-a", Status::Failed));
}

/// Aborting the handle tears the run down and tells the kernel to drop state.
#[test]
fn abort_handle_tears_run_down() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    let f = Fakes::new(
        DispatchAuthorizeResponse::Authorized {
            run_id: "r-001".into(),
            total_tasks: 1,
            wave: vec![plan("r-001", "t-a", "faye", &[])],
        },
        vec![],
    );
    let proxy = f.proxy();

    let DispatchStartOutcome::Started { handle, .. } =
        proxy.start_run(request(pd, vec![task("t-a", "faye", &[])]))
    else {
        panic!("expected Started");
    };
    assert!(wait_for(|| status_is(pd, "t-a", Status::Spawned)));

    handle.abort();
    assert!(wait_for(|| *f.kernel.aborts.lock().unwrap() >= 1));
    assert_eq!(run_closed(pd).as_deref(), Some("cancelled"));
}
