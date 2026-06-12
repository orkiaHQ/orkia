// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell::builtin_resolve::{KillAction, resolve_job_target, resolve_kill, split_kill_args};
use orkia_shell_types::{JobId, JobInfo, JobKind, JobState};
use std::time::Duration;
use uuid::Uuid;

fn shell_job(id: u32, pid: Option<u32>, label: &str) -> JobInfo {
    JobInfo {
        id: JobId(id),
        kind: JobKind::Shell { cmd: label.into() },
        state: JobState::Running,
        label: label.into(),
        pid,
        runtime: Duration::from_secs(1),
        sink: None,
    }
}

fn agent_job(id: u32, pid: Option<u32>, name: &str) -> JobInfo {
    JobInfo {
        id: JobId(id),
        kind: JobKind::Agent {
            agent_id: Uuid::nil(),
            agent_name: name.into(),
        },
        state: JobState::Running,
        label: name.into(),
        pid,
        runtime: Duration::from_secs(1),
        sink: None,
    }
}

#[test]
fn kill_by_job_id() {
    let jobs = vec![shell_job(1, Some(8000), "echo")];
    assert_eq!(
        resolve_kill("1", None, &jobs),
        KillAction::StopJob(JobId(1))
    );
}

#[test]
fn kill_by_agent_name_returns_latest_job() {
    let jobs = vec![agent_job(1, None, "faye"), agent_job(2, None, "faye")];
    assert_eq!(
        resolve_kill("faye", None, &jobs),
        KillAction::StopJob(JobId(2))
    );
}

#[test]
fn kill_by_pid_matches_job() {
    let jobs = vec![shell_job(7, Some(8201), "cargo")];
    assert_eq!(
        resolve_kill("8201", None, &jobs),
        KillAction::StopJob(JobId(7))
    );
}

#[test]
fn kill_unknown_falls_through_to_system() {
    let jobs: Vec<JobInfo> = vec![];
    assert_eq!(
        resolve_kill("9999", None, &jobs),
        KillAction::SystemKill {
            target: "9999".into(),
            signal: "TERM".into(),
        }
    );
}

#[test]
fn kill_with_signal_always_passthrough() {
    let jobs = vec![shell_job(1, Some(8000), "echo")];
    assert_eq!(
        resolve_kill("1", Some("9"), &jobs),
        KillAction::SystemKill {
            target: "1".into(),
            signal: "9".into(),
        }
    );
}

#[test]
fn job_target_resolves_by_id_and_name() {
    let jobs = vec![agent_job(1, None, "faye"), agent_job(2, None, "killua")];
    assert_eq!(resolve_job_target("1", &jobs), Some(JobId(1)));
    assert_eq!(resolve_job_target("killua", &jobs), Some(JobId(2)));
    assert_eq!(resolve_job_target("sage", &jobs), None);
}

#[test]
fn split_kill_args_separates_signal() {
    assert_eq!(
        split_kill_args(&["-9".into(), "1234".into()]).unwrap(),
        (Some("9".into()), "1234".into())
    );
    assert_eq!(
        split_kill_args(&["-TERM".into(), "faye".into()]).unwrap(),
        (Some("TERM".into()), "faye".into())
    );
    assert_eq!(
        split_kill_args(&["1234".into()]).unwrap(),
        (None, "1234".into())
    );
}

#[test]
fn split_kill_args_errors_on_missing_target() {
    assert!(split_kill_args(&[]).is_err());
    assert!(split_kill_args(&["-9".into()]).is_err());
}
