// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! of decision resolutions.
//!
//! Spawns `cat -u` as a fake agent PTY, records its job id against a
//! decision id in the [`ClarificationPtyBridge`], then writes the
//! resolution payload via `JobController::write_to_pty` and asserts the
//! child echoes the payload back. This proves the end-to-end injection
//! mechanism without needing a real claude-code binary — the only thing
//! that varies for a real agent is whether its stdin reader treats the
//! line as a complete prompt (a property of the agent, not orkia).

use std::time::Duration;

use orkia_rfc_core::DecisionId;
use orkia_shell::job::JobController;
use orkia_shell::rfc_state::ClarificationPtyBridge;
use tempfile::TempDir;

mod common;
use common::{FakeAgent, spawn_fake_agent};

fn spawn_cat(jobs: &mut JobController, dir: &TempDir) -> orkia_shell_types::JobId {
    spawn_fake_agent(
        jobs,
        dir.path(),
        FakeAgent::cmd("rfc-cat", "cat", &["-u".to_string()]),
    )
}

#[test]
fn clarification_resolution_round_trips_through_pty() {
    let dir = TempDir::new().expect("tmp");
    let (mut jobs, _events) = JobController::new();
    let job_id = spawn_cat(&mut jobs, &dir);
    std::thread::sleep(Duration::from_millis(100));

    let rx_output = jobs
        .get(job_id)
        .expect("job entry")
        .engine
        .subscribe_output();

    // Mirror what `McpShellDispatcher::dispatch` does when an MCP-connected
    // agent calls `orkia_rfc_ask`: record the asking agent's job id under
    // the decision id returned by the service.
    let bridge = ClarificationPtyBridge::new();
    let did = DecisionId::new("d-001");
    bridge.record(did.clone(), job_id);

    // Now simulate the REPL handling `rfc resolve d-001 --answer "both"`:
    // take the recorded job id and write the resolution payload into its
    // PTY exactly as `handle_rfc_resolve` does.
    let answer = "both platforms";
    let recorded = bridge.take(&did).expect("bridge recorded the asker");
    assert_eq!(recorded, job_id);
    let payload = ClarificationPtyBridge::format_resolution(&did, answer);
    for byte in &payload {
        jobs.write_to_pty(recorded, &[*byte]).expect("write");
        std::thread::sleep(Duration::from_millis(5));
    }

    // Verify cat echoed the payload back. The agent's LLM-side reader
    // would see this on stdin and resume work.
    let deadline = std::time::Instant::now() + Duration::from_millis(800);
    let mut collected: Vec<u8> = Vec::new();
    let needle = b"Decision d-001 resolved: both platforms";
    while std::time::Instant::now() < deadline {
        match rx_output.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => collected.extend_from_slice(&chunk),
            Err(_) => continue,
        }
        if collected.windows(needle.len()).any(|w| w == needle) {
            break;
        }
    }
    let visible: String = collected
        .iter()
        .filter(|b| b.is_ascii_graphic() || **b == b' ' || **b == b'\n' || **b == b'\r')
        .map(|b| *b as char)
        .collect();
    assert!(
        visible.contains("Decision d-001 resolved: both platforms"),
        "expected resolution payload in echo, got: {visible:?}",
    );

    let _ = jobs.stop(job_id);
}

#[test]
fn bridge_is_one_shot_per_decision() {
    let bridge = ClarificationPtyBridge::new();
    let did = DecisionId::new("d-007");
    bridge.record(did.clone(), orkia_shell_types::JobId(7));
    assert!(bridge.take(&did).is_some());
    // After resolution, a second resolve call must not re-inject — the
    // decision is closed.
    assert!(bridge.take(&did).is_none());
}
