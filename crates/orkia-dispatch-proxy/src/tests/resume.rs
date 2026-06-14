// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file for terms.

//! Reconstruction from [`KernelDispatchProxy::resume_run`] (`SPEC` §4.3 / step
//! 5.D). The issues store left on disk *is* the run state: the kernel's DAG is
//! gone after a restart, so the plan is re-authorized fresh and the actor
//! reconciles each freshly-released wave against its issue — skipping `Done`,
//! re-adopting a live `Spawned`, recovering one whose response the daemon
//! captured while the shell was down, and failing the truly lost ones.
//!
//! Every test seeds a two-task chain `t-a` → `t-b` as a pre-restart run would
//! have left it, then resumes and asserts the reconciliation outcome.

use orkia_shell_types::dispatch_kernel::{DispatchAdvanceResponse, DispatchAuthorizeResponse};

use super::support::*;
use crate::ResumeOutcome;
use crate::issues::Status;

/// `t-a` finished, `t-b` is still running on a live daemon job: skip `t-a`
/// (fast-forward the kernel), re-adopt `t-b`'s job so its response routes, and
/// nothing re-spawns.
#[test]
fn resume_skips_done_and_adopts_live() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    seed_run(pd, None);
    seed_issue(pd, "t-a", "faye", &[], Status::Done, Some(1), Some("A OUT"));
    seed_issue(pd, "t-b", "sage", &["t-a"], Status::Spawned, Some(7), None);

    let f = Fakes::new(
        DispatchAuthorizeResponse::Authorized {
            run_id: "r-002".into(),
            total_tasks: 2,
            wave: vec![plan("r-002", "t-a", "faye", &[])],
        },
        vec![
            DispatchAdvanceResponse::NextWave {
                wave: vec![plan("r-002", "t-b", "sage", &["t-a"])],
            },
            DispatchAdvanceResponse::Completed { elapsed_ms: 0 },
        ],
    );
    f.daemon.add_live(7);
    let proxy = f.proxy();

    let out = proxy.resume_run(request(
        pd,
        vec![task("t-a", "faye", &[]), task("t-b", "sage", &["t-a"])],
    ));
    assert!(matches!(out, ResumeOutcome::Resumed { .. }));

    // The live job's response lands and completes the run; nothing re-spawned.
    let pb = write_response(pd, "b.txt", "B OUT");
    f.responses.fire(done_event(7, "sage", pb));
    assert!(wait_for(|| run_closed(pd).as_deref() == Some("completed")));
    assert!(status_is(pd, "t-a", Status::Done));
    assert!(status_is(pd, "t-b", Status::Done));
    assert_eq!(f.spawner.count(), 0);
    assert_eq!(*f.kernel.advance_log.lock().unwrap(), vec!["t-a", "t-b"]);
}

/// `t-b` was `Spawned` but its daemon job is gone and no response survives:
/// fail it closed (§8) and let the kernel's `on_task_fail` pause the run.
#[test]
fn resume_fails_lost_spawned() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    seed_run(pd, None);
    seed_issue(pd, "t-a", "faye", &[], Status::Done, Some(1), Some("A OUT"));
    seed_issue(pd, "t-b", "sage", &["t-a"], Status::Spawned, Some(7), None);

    let f = Fakes::new(
        DispatchAuthorizeResponse::Authorized {
            run_id: "r-002".into(),
            total_tasks: 2,
            wave: vec![plan("r-002", "t-a", "faye", &[])],
        },
        vec![
            DispatchAdvanceResponse::NextWave {
                wave: vec![plan("r-002", "t-b", "sage", &["t-a"])],
            },
            DispatchAdvanceResponse::Paused {
                failed: vec!["t-b".into()],
            },
        ],
    );
    // Daemon empty, no captured response → t-b is unrecoverable.
    let proxy = f.proxy();

    let out = proxy.resume_run(request(
        pd,
        vec![task("t-a", "faye", &[]), task("t-b", "sage", &["t-a"])],
    ));
    assert!(matches!(out, ResumeOutcome::Resumed { .. }));

    assert!(wait_for(|| run_closed(pd).as_deref() == Some("paused: t-b")));
    assert!(status_is(pd, "t-b", Status::Failed));
    assert_eq!(f.spawner.count(), 0);
}

/// The shell restarted but the daemon kept running and captured `t-b`'s
/// response: `latest_for_job` recovers it without re-spawning.
#[test]
fn resume_recovers_via_latest_for_job() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    seed_run(pd, None);
    seed_issue(pd, "t-a", "faye", &[], Status::Done, Some(1), Some("A OUT"));
    seed_issue(pd, "t-b", "sage", &["t-a"], Status::Spawned, Some(7), None);

    let f = Fakes::new(
        DispatchAuthorizeResponse::Authorized {
            run_id: "r-002".into(),
            total_tasks: 2,
            wave: vec![plan("r-002", "t-a", "faye", &[])],
        },
        vec![
            DispatchAdvanceResponse::NextWave {
                wave: vec![plan("r-002", "t-b", "sage", &["t-a"])],
            },
            DispatchAdvanceResponse::Completed { elapsed_ms: 0 },
        ],
    );
    let pb = write_response(pd, "b.txt", "B RECOVERED");
    f.responses.set_latest(7, done_event(7, "sage", pb));
    let proxy = f.proxy();

    let out = proxy.resume_run(request(
        pd,
        vec![task("t-a", "faye", &[]), task("t-b", "sage", &["t-a"])],
    ));
    assert!(matches!(out, ResumeOutcome::Resumed { .. }));

    assert!(wait_for(|| run_closed(pd).as_deref() == Some("completed")));
    assert!(status_is(pd, "t-b", Status::Done));
    let t_b = read_issue(pd, "t-b").unwrap();
    assert_eq!(t_b.response.as_deref(), Some("B RECOVERED"));
    assert_eq!(f.spawner.count(), 0);
}

/// A run already closed refuses to resume, surfacing why it ended.
#[test]
fn resume_refuses_closed_run() {
    let dir = tempfile::tempdir().unwrap();
    let pd = dir.path();
    seed_run(pd, Some("completed"));
    seed_issue(pd, "t-a", "faye", &[], Status::Done, Some(1), Some("A OUT"));

    let f = Fakes::new(
        DispatchAuthorizeResponse::Authorized {
            run_id: "r-002".into(),
            total_tasks: 1,
            wave: vec![],
        },
        vec![],
    );
    let proxy = f.proxy();

    let out = proxy.resume_run(request(pd, vec![task("t-a", "faye", &[])]));
    let ResumeOutcome::AlreadyClosed { reason } = out else {
        panic!("expected AlreadyClosed");
    };
    assert_eq!(reason, "completed");
}

/// `resume_run` over an RFC that never started finds no run meta to resume.
#[test]
fn resume_no_run_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fakes::new(
        DispatchAuthorizeResponse::Authorized {
            run_id: "r-002".into(),
            total_tasks: 0,
            wave: vec![],
        },
        vec![],
    );
    let proxy = f.proxy();

    let out = proxy.resume_run(request(dir.path(), vec![task("t-a", "faye", &[])]));
    assert!(matches!(out, ResumeOutcome::NoRun));
}
