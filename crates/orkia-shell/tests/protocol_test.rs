// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! End-to-end: synthetic OSC 133 byte sequence emitted by a child
//! into a PTY → BlockParser dispatch → `EventRouter` → unified
//! `OrkiaEvent` stream. Proves the wiring across the
//! `orkia-terminal-core` / `orkia-shell::protocol` boundary works.

use std::sync::Arc;
use std::time::Duration;

use orkia_shell::job::JobController;
use orkia_shell::protocol::{EventPayload, EventRouter, EventSource};
use tempfile::TempDir;

mod common;
use common::{FakeAgent, spawn_fake_agent};

/// Spawn a tiny shell snippet that prints OSC 133 A, B, C, then a
/// line of output, then D;0. `cat` would echo our input — we need
/// the agent itself to *emit* the markers, so we use `printf`.
fn spawn_osc133_emitter(
    jobs: &mut JobController,
    dir: &TempDir,
    router: &EventRouter,
) -> orkia_shell_types::JobId {
    // -c '...' runs the snippet then exits. We sleep briefly between
    // markers so a single reader-thread read() doesn't deliver all
    // four in one chunk (which still works but is less
    // representative of a real interactive agent).
    let script = "printf '\\033]133;A\\007'; \
                  printf '\\033]133;B\\007'; \
                  printf '\\033]133;C\\007'; \
                  printf 'hello\\n'; \
                  printf '\\033]133;D;0\\007'; \
                  sleep 0.5";
    let args = ["-c".to_string(), script.to_string()];
    spawn_fake_agent(
        jobs,
        dir.path(),
        FakeAgent {
            name: "osc133-test",
            cmd: "/bin/sh",
            args: &args,
            event_router: Some(router),
            initial_prompt: None,
        },
    )
}

#[tokio::test]
async fn osc133_markers_reach_event_router() {
    let dir = TempDir::new().expect("tmp");
    let (mut jobs, _events) = JobController::new();
    let router = EventRouter::new();
    let mut rx = router.take_rx().expect("rx");

    let job_id = spawn_osc133_emitter(&mut jobs, &dir, &router);

    // The reader thread drives the BlockParser which calls our
    // OSC 133 callback synchronously. Give it generous time so a
    // slow CI box can finish the printf + sleep + emit cycle.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(evt)) => {
                assert!(
                    matches!(evt.source, EventSource::Osc133),
                    "unexpected source {:?}",
                    evt.source
                );
                assert_eq!(evt.job_id, job_id);
                seen.push(evt.event);
                if matches!(seen.last(), Some(EventPayload::OutputFinished { .. })) {
                    break;
                }
            }
            _ => continue,
        }
    }

    let tags: Vec<&str> = seen.iter().map(EventPayload::tag).collect();
    assert!(
        tags.contains(&"prompt_start"),
        "missing PromptStart in {tags:?}"
    );
    assert!(
        tags.contains(&"prompt_ready"),
        "missing PromptReady in {tags:?}"
    );
    assert!(
        tags.contains(&"output_start"),
        "missing OutputStart in {tags:?}"
    );
    let finished = seen
        .iter()
        .find_map(|p| match p {
            EventPayload::OutputFinished { exit_code } => Some(*exit_code),
            _ => None,
        })
        .expect("OutputFinished arrived");
    assert_eq!(finished, Some(0));

    // Keep references alive until the assertions complete.
    let _ = (jobs, dir, Arc::new(router));
}
