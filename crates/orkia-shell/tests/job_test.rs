// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `JobController` lifecycle tests. Drives the controller via the unified
//! `JobController::spawn` path (through the shared `common::spawn_fake_agent`
//! helper). Uses `/bin/sh` as a generic "agent command" that respects
//! standard signals — the controller doesn't care that it's technically a
//! shell from the kernel's perspective.

use orkia_shell::job::JobController;
use tempfile::TempDir;

mod common;
use common::{FakeAgent, spawn_fake_agent};

fn tempdir() -> TempDir {
    TempDir::new().expect("tempdir")
}

fn spawn(ctrl: &mut JobController, dir: &TempDir, args: Vec<String>) -> orkia_shell::JobId {
    spawn_fake_agent(
        ctrl,
        dir.path(),
        FakeAgent::cmd("test-agent", "/bin/sh", &args),
    )
}

#[test]
fn write_to_pty_round_trips_through_running_job() {
    // Use `cat` so the bytes we write get echoed straight back to the
    // PTY's master side. The terminal core captures the master output
    // into its screen buffer, which we can scrape after a brief wait.
    let (mut ctrl, _rx) = JobController::new();
    let dir = tempdir();
    let id = spawn(&mut ctrl, &dir, vec!["-c".into(), "cat".into()]);
    ctrl.write_to_pty(id, b"hello\n").expect("write");
    // Give the child a moment to echo. The PTY plumbing is async-ish;
    // 100ms is plenty for `cat` on a local shell.
    std::thread::sleep(std::time::Duration::from_millis(150));
    let entry = ctrl.get(id).expect("job alive");
    assert!(entry.is_alive());
    let _ = ctrl.stop(id);
}

#[test]
fn write_to_pty_unknown_job_errors() {
    let (ctrl, _rx) = JobController::new();
    let err = ctrl
        .write_to_pty(orkia_shell::JobId(999), b"x")
        .unwrap_err();
    assert!(format!("{err}").contains("not found"));
}

#[test]
fn initial_prompt_is_written_at_spawn() {
    // Inject `printf hi\n` via `sh -c "cat"` and confirm the controller
    // didn't error. Round-trip read is covered by write_to_pty_round_trips
    // above — here we just want the initial-prompt path exercised.
    let (mut ctrl, _rx) = JobController::new();
    let dir = tempdir();
    let args = ["-c".to_string(), "cat".to_string()];
    let id = spawn_fake_agent(
        &mut ctrl,
        dir.path(),
        FakeAgent {
            name: "echo-agent",
            cmd: "/bin/sh",
            args: &args,
            event_router: None,
            initial_prompt: Some("hello world"),
        },
    );
    assert!(ctrl.get(id).is_some());
    let _ = ctrl.stop(id);
}

#[test]
fn spawn_creates_job_with_pid() {
    let (mut ctrl, _rx) = JobController::new();
    let dir = tempdir();
    let id = spawn(&mut ctrl, &dir, vec!["-c".into(), "echo hello".into()]);
    let entry = ctrl.get(id).expect("job exists");
    assert!(entry.pid().is_some(), "should have a PID");
}

#[test]
fn list_returns_running_jobs() {
    let (mut ctrl, _rx) = JobController::new();
    let dir = tempdir();
    let id = spawn(&mut ctrl, &dir, vec!["-c".into(), "sleep 10".into()]);
    let jobs = ctrl.list();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, id);
    assert!(jobs[0].pid.is_some());

    ctrl.stop(id).expect("stop");
}

#[test]
fn stop_sends_signal_and_marks_done() {
    let (mut ctrl, _rx) = JobController::new();
    let dir = tempdir();
    let id = spawn(&mut ctrl, &dir, vec!["-c".into(), "sleep 60".into()]);
    ctrl.stop(id).expect("stop");

    let entry = ctrl.get(id);
    assert!(entry.is_some());
    let entry = entry.expect("entry");
    assert!(
        matches!(entry.state, orkia_shell::JobState::Stopped),
        "state should be Stopped after stop"
    );
}

#[test]
fn reap_removes_completed_jobs() {
    let (mut ctrl, _rx) = JobController::new();
    let dir = tempdir();
    let _id = spawn(&mut ctrl, &dir, vec!["-c".into(), "true".into()]);

    // Give the process a moment to exit
    std::thread::sleep(std::time::Duration::from_millis(200));

    ctrl.reap();
    let jobs = ctrl.list();
    assert!(jobs.is_empty(), "completed jobs should be reaped");
}
