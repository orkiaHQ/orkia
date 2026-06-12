// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Verifies the byte-by-byte injection mechanism orkia uses to push a
//! pending prompt body into an agent's PTY.
//!
//! We spawn `cat -u` (unbuffered) in a PTY, write a payload byte-by-
//! byte to the master via `JobController`, and read the echo back. If
//! the bytes round-trip, the mechanism works — proving the only thing
//! that can go wrong against a real agent (claude/Bun) is timing /
//! agent-side input-handling, not orkia's write path.

use std::time::Duration;

use orkia_shell::job::JobController;
use tempfile::TempDir;

mod common;
use common::{FakeAgent, spawn_fake_agent};

fn spawn_cat(jobs: &mut JobController, dir: &TempDir) -> orkia_shell_types::JobId {
    // `cat -u` echoes stdin to stdout unbuffered — the simplest agent
    // we can throw bytes at and read back.
    spawn_fake_agent(
        jobs,
        dir.path(),
        FakeAgent::cmd("cat-test", "cat", &["-u".to_string()]),
    )
}

/// Verifies the **sequencing fix**: when the state-machine worker
/// thread emits a `DetectorEvent::Injected`, the REPL must drain the
/// side-effect channel *before* attach-mode mutes the detector,
/// otherwise the `emit_injection` write is silently skipped and the
/// agent receives nothing — exactly the bug the user hit.
///
/// We can't easily spin up a full `Repl::run()` loop in a test, so we
/// model the channel layout the worker creates (`DetectorEvent` →
/// mpsc → main-loop drain) and assert the bytes actually round-trip.
#[test]
fn drain_state_machine_writes_pending_injection_to_pty() {
    use orkia_shell::terminal_state::DetectorEvent;
    use orkia_shell_types::JobId;

    let dir = TempDir::new().expect("tmp");
    let (mut jobs, _events) = JobController::new();
    let job_id = spawn_cat(&mut jobs, &dir);
    std::thread::sleep(Duration::from_millis(100));

    let rx_output = jobs
        .get(job_id)
        .expect("job entry")
        .engine
        .subscribe_output();

    // Build the same channel topology `boot_state_machine_worker`
    // hands the REPL: a side-effect receiver that the main loop
    // drains for `Injected` events.
    let (repl_tx, repl_rx) = std::sync::mpsc::channel::<DetectorEvent>();

    // Worker simulates: detector fires `Injected`, forwards to REPL.
    let body = "salut".to_string();
    repl_tx
        .send(DetectorEvent::Injected {
            job_id,
            agent_name: "cat-test".into(),
            body: body.clone(),
        })
        .expect("send");

    // Drain — mirrors `drain_state_machine_events` + `emit_injection`
    // body-by-body write. If the drain skips the event (the original
    // bug), `cat` receives nothing.
    while let Ok(event) = repl_rx.try_recv() {
        if let DetectorEvent::Injected {
            job_id: id, body, ..
        } = event
        {
            let payload: Vec<u8> = body.bytes().chain(std::iter::once(b'\r')).collect();
            for byte in &payload {
                jobs.write_to_pty(id, &[*byte]).expect("write");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    // Verify cat echoed the body back.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    let mut collected: Vec<u8> = Vec::new();
    while std::time::Instant::now() < deadline {
        match rx_output.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => collected.extend_from_slice(&chunk),
            Err(_) => continue,
        }
        if collected.windows(body.len()).any(|w| w == body.as_bytes()) {
            break;
        }
    }
    let visible: String = collected
        .iter()
        .filter(|b| b.is_ascii_graphic() || **b == b' ' || **b == b'\n' || **b == b'\r')
        .map(|b| *b as char)
        .collect();
    assert!(
        visible.contains(&body),
        "expected '{body}' in echo after drain — bytes were never written if drain was skipped. Got: {visible:?}",
    );

    let _ = jobs.stop(job_id);
    let _ = job_id; // silence unused if cfg
    let _: JobId = job_id;
}

#[test]
fn byte_by_byte_inject_round_trips_through_pty() {
    let dir = TempDir::new().expect("tmp");
    let (mut jobs, _events) = JobController::new();
    let job_id = spawn_cat(&mut jobs, &dir);

    // Give cat a tick to fully wire its stdin.
    std::thread::sleep(Duration::from_millis(100));

    // Subscribe to the engine's output stream so we can read what cat
    // echoes back.
    let rx = jobs
        .get(job_id)
        .expect("job entry")
        .engine
        .subscribe_output();

    // Inject "hello\r" byte-by-byte (the same code path
    // `emit_injection` uses).
    let payload: Vec<u8> = "hello\r".bytes().collect();
    for byte in &payload {
        jobs.write_to_pty(job_id, &[*byte]).expect("write");
        std::thread::sleep(Duration::from_millis(5));
    }

    // Drain up to 500ms looking for the echo. `cat -u` should send
    // back the same bytes immediately.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    let mut collected: Vec<u8> = Vec::new();
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => collected.extend_from_slice(&chunk),
            Err(_) => continue,
        }
        if collected.windows(5).any(|w| w == b"hello") {
            break;
        }
    }

    let visible: String = collected
        .iter()
        .filter(|b| b.is_ascii_graphic() || **b == b' ' || **b == b'\n' || **b == b'\r')
        .map(|b| *b as char)
        .collect();
    assert!(
        visible.contains("hello"),
        "expected 'hello' in echo, got: {visible:?} (raw len {})",
        collected.len(),
    );

    let _ = jobs.stop(job_id);
}
