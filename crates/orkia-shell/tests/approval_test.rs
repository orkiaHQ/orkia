// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell::approval::{ApprovalRequest, ApprovalSource, ApprovalWatcher, PendingApproval};
use orkia_shell_types::JobId;
use tempfile::TempDir;

fn write_request(dir: &std::path::Path, action: &str) {
    let req = ApprovalRequest {
        action: action.into(),
        description: Some("test description".into()),
        risk: Some("low".into()),
        files_changed: Some(vec!["a.rs".into(), "b.rs".into()]),
        metadata: None,
    };
    let json = serde_json::to_string(&req).expect("serialize");
    std::fs::write(dir.join("approval.request.json"), json).expect("write request");
}

/// The post-P2-001 `ApprovalWatcher::poll` is asynchronous — it
/// enqueues a scan for the janitor thread and returns whatever
/// discoveries are already in the channel. Tests that fabricate a
/// request file and immediately expect the watcher to see it must
/// loop briefly to let the janitor catch up.
fn poll_until(watcher: &mut ApprovalWatcher, ids: &[JobId]) -> Vec<PendingApproval> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let new = watcher.poll(ids);
        if !new.is_empty() || std::time::Instant::now() >= deadline {
            return new;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn wait_until<F: FnMut() -> bool>(mut pred: F) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !pred() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn watcher_creates_per_job_dir() {
    let temp = TempDir::new().expect("tempdir");
    let watcher = ApprovalWatcher::new(temp.path());
    let dir = watcher.create_job_dir(JobId(42));
    assert!(dir.exists());
    assert!(dir.ends_with("run/42"));
}

#[test]
fn watcher_poll_detects_request() {
    let temp = TempDir::new().expect("tempdir");
    let mut watcher = ApprovalWatcher::new(temp.path());
    let id = JobId(7);
    let dir = watcher.create_job_dir(id);
    write_request(&dir, "git push --force");

    let new = poll_until(&mut watcher, &[id]);
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].job_id, id);
    assert_eq!(new[0].request.action, "git push --force");
    assert_eq!(watcher.pending().len(), 1);

    // Polling again should not re-emit the same pending request.
    let again = watcher.poll(&[id]);
    assert!(again.is_empty());
    assert_eq!(watcher.pending().len(), 1);
}

#[test]
fn watcher_resolve_writes_response_and_clears_pending() {
    let temp = TempDir::new().expect("tempdir");
    let mut watcher = ApprovalWatcher::new(temp.path());
    let id = JobId(1);
    let dir = watcher.create_job_dir(id);
    write_request(&dir, "rm -rf /tmp/something");

    let _ = poll_until(&mut watcher, &[id]);
    watcher.resolve(id, true).expect("resolve ok");

    let response_path = dir.join("approval.response.json");
    assert!(response_path.exists());
    let body = std::fs::read_to_string(&response_path).expect("read response");
    assert!(body.contains("\"approved\": true"));
    assert!(watcher.pending().is_empty());
}

#[test]
fn watcher_resolve_unknown_job_errors() {
    let temp = TempDir::new().expect("tempdir");
    let mut watcher = ApprovalWatcher::new(temp.path());
    let err = watcher.resolve(JobId(999), false).expect_err("must error");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn watcher_skips_polling_when_response_already_present() {
    let temp = TempDir::new().expect("tempdir");
    let mut watcher = ApprovalWatcher::new(temp.path());
    let id = JobId(2);
    let dir = watcher.create_job_dir(id);
    write_request(&dir, "noop");
    std::fs::write(dir.join("approval.response.json"), "{}").expect("write response");

    let new = watcher.poll(&[id]);
    assert!(new.is_empty(), "must skip when response already exists");
}

#[test]
fn push_from_hook_queues_approval_with_hook_source() {
    let temp = TempDir::new().expect("tempdir");
    let mut watcher = ApprovalWatcher::new(temp.path());
    let id = JobId(11);
    let request = ApprovalRequest {
        action: "git push".into(),
        description: Some("force-push to main".into()),
        risk: Some("high".into()),
        files_changed: None,
        metadata: None,
    };
    assert!(watcher.push_from_hook(id, request));
    assert_eq!(watcher.pending().len(), 1);
    assert_eq!(watcher.pending()[0].source, ApprovalSource::Hook);
}

#[test]
fn push_from_hook_dedupes_per_job() {
    let temp = TempDir::new().expect("tempdir");
    let mut watcher = ApprovalWatcher::new(temp.path());
    let id = JobId(12);
    let r = || ApprovalRequest {
        action: "a".into(),
        description: None,
        risk: None,
        files_changed: None,
        metadata: None,
    };
    assert!(watcher.push_from_hook(id, r()));
    assert!(
        !watcher.push_from_hook(id, r()),
        "second push must be dropped"
    );
    assert_eq!(watcher.pending().len(), 1);
}

#[test]
fn resolve_hook_approval_does_not_write_response_file() {
    let temp = TempDir::new().expect("tempdir");
    let mut watcher = ApprovalWatcher::new(temp.path());
    let id = JobId(13);
    watcher.push_from_hook(
        id,
        ApprovalRequest {
            action: "x".into(),
            description: None,
            risk: None,
            files_changed: None,
            metadata: None,
        },
    );
    let resolved = watcher.resolve(id, true).expect("resolve");
    assert_eq!(resolved.source, ApprovalSource::Hook);
    // Hook-sourced approvals must not create the response file —
    // resolution flows back through the agent PTY instead.
    assert!(!resolved.response_path.exists());
    assert!(watcher.pending().is_empty());
}

#[test]
fn watcher_cleanup_removes_dir_and_pending() {
    let temp = TempDir::new().expect("tempdir");
    let mut watcher = ApprovalWatcher::new(temp.path());
    let id = JobId(3);
    let dir = watcher.create_job_dir(id);
    write_request(&dir, "anything");
    let _ = poll_until(&mut watcher, &[id]);

    watcher.cleanup_job(id);
    // Directory removal happens on the janitor thread; wait briefly.
    wait_until(|| !dir.exists());
    assert!(!dir.exists());
    assert!(watcher.pending().is_empty());
}
