// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Focused tests for the daemon-job dispatch/wait helpers used by the
//! socket-talking helpers (`resolve_daemon_target`, `wait/kill/tell_daemon_job`)
//! need a live daemon and are exercised by the `restart` demos scenario.
//! The daemon-state → `JobState` mapping itself lives in `ps::job_state`

use super::agent_dispatch::daemon_view_is_live;
use super::job_control::daemon_wait_refusal_for;
use orkia_shell_types::DaemonJobView;

fn view(state: &str) -> DaemonJobView {
    DaemonJobView {
        id: 1,
        agent: "sage".into(),
        state: state.into(),
        pid: Some(4242),
        label: "@sage … PONG".into(),
        runtime_secs: 73,
        exit_code: None,
        stages: Vec::new(),
    }
}

#[test]
fn dead_states_are_not_live_for_dispatch() {
    // Corpses (daemon reaps them at list time): `@agent` must spawn
    // fresh, not tell a dead job ("job N is stale").
    for s in ["pid_dead", "control_unavailable", "done", "failed: boom"] {
        assert!(!daemon_view_is_live(&view(s)), "{s} should not be live");
    }
    // Live or anomalous-but-alive sessions stay tellable.
    for s in ["running", "detached", "recovered", "lost_pty"] {
        assert!(daemon_view_is_live(&view(s)), "{s} should be live");
    }
}

#[test]
fn wait_refuses_live_persistent_agent_session() {
    // A persistent session never exits on its own — `wait` must fail
    // fast with hints, not block the REPL indefinitely.
    let refusal = daemon_wait_refusal_for(&view("running"));
    let msg = refusal.unwrap();
    assert!(msg.contains("persistent agent session"), "{msg}");
    assert!(msg.contains("@sage"), "{msg}");
    assert!(msg.contains("--once"), "{msg}");
}

#[test]
fn wait_allows_once_dispatch_and_terminal_states() {
    // `--once` jobs DO terminate after their single turn: waitable.
    let mut once = view("running");
    once.label = "@sage say PONG --once".into();
    assert!(daemon_wait_refusal_for(&once).is_none());
    // Terminal/dead states resolve immediately: waitable.
    for s in ["done", "failed: boom", "pid_dead", "control_unavailable"] {
        assert!(daemon_wait_refusal_for(&view(s)).is_none(), "{s}");
    }
}
