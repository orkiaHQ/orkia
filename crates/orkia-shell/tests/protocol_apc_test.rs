// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! V2 end-to-end: synthetic agent emits an APC `Orkia;<json>`
//! sequence into its PTY. BlockParser strips the envelope and
//! delivers the payload to the `on_apc` callback. The callback
//! parses JSON and pushes an `OrkiaEvent` with
//! `EventSource::OrkiaProtocol` through the `EventRouter`.

use std::time::Duration;

use orkia_shell::job::JobController;
use orkia_shell::protocol::{EventPayload, EventRouter, EventSource};
use tempfile::TempDir;

mod common;
use common::{FakeAgent, spawn_fake_agent};

fn spawn_apc_emitter(
    jobs: &mut JobController,
    dir: &TempDir,
    router: &EventRouter,
) -> orkia_shell_types::JobId {
    // Emit two distinct events as APC sequences:
    //   1. PromptReady — minimum JSON
    //   2. ToolUse with target
    // Then sleep so the process stays alive long enough for the
    // reader thread to drain the bytes.
    //
    // `printf '\\033'` produces an ESC byte; `\\\\` becomes `\` in
    // the shell, so `\\033\\\\` is `ESC \` (the APC ST).
    let script = "\
printf '\\033_Orkia;{\"type\":\"PromptReady\"}\\033\\\\'; \
printf '\\033_Orkia;{\"type\":\"ToolUse\",\"tool\":\"Read\",\"target\":\"src/x.rs\"}\\033\\\\'; \
sleep 0.5";
    let args = ["-c".to_string(), script.to_string()];
    spawn_fake_agent(
        jobs,
        dir.path(),
        FakeAgent {
            name: "apc-test",
            cmd: "/bin/sh",
            args: &args,
            event_router: Some(router),
            initial_prompt: None,
        },
    )
}

#[tokio::test]
async fn apc_payloads_reach_event_router() {
    let dir = TempDir::new().expect("tmp");
    let (mut jobs, _events) = JobController::new();
    let router = EventRouter::new();
    let mut rx = router.take_rx().expect("rx");

    let job_id = spawn_apc_emitter(&mut jobs, &dir, &router);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut seen: Vec<EventPayload> = Vec::new();
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(evt)) => {
                assert_eq!(evt.job_id, job_id);
                assert!(
                    matches!(evt.source, EventSource::OrkiaProtocol),
                    "wrong source: {:?}",
                    evt.source
                );
                assert_eq!(evt.confidence, 1.0);
                seen.push(evt.event);
                if seen.len() >= 2 {
                    break;
                }
            }
            _ => continue,
        }
    }

    assert!(
        matches!(seen.first(), Some(EventPayload::PromptReady)),
        "first event not PromptReady: {seen:?}"
    );
    match seen.get(1) {
        Some(EventPayload::ToolUse { tool, target, .. }) => {
            assert_eq!(tool, "Read");
            assert_eq!(target.as_deref(), Some("src/x.rs"));
        }
        other => panic!("second event not ToolUse: {other:?}"),
    }

    // Keep the controller + dir alive until the assertions finish.
    let _ = (jobs, dir);
}
