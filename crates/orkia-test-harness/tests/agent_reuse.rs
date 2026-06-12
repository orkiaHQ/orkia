// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! E2E: a second `@faye <body>` while faye is alive must NOT spawn a
//! second job — the daemon must route it to the existing live job.
//!
//! Closes GAP-009 from `audits/E2E-FAIL-SOFT.md`: a regression in
//! dispatch that drops the live-agent fast path (e.g. someone removing
//! the by-name reuse short-circuit) makes this test fail because the
//! second dispatch would spawn a SECOND job — visible as a second
//! `agent.spawn` SEAL record under a second `jobs/<N>` dir.
//!
//! Since the daemon-owned-`@name` flip a bare `@faye` dispatch spawns a
//! **detached** job and auto-attaches into its screen. So the second
//! dispatch must be issued from the REPL — we Ctrl-Z detach first —
//! otherwise the keystrokes splice into the agent's PTY instead of
//! reaching the shell. The durable record of "how many jobs were
//! spawned" is the per-job SEAL chain (`agent.spawn` per job dir), not
//! the unified journal (a detached job forwards only its final
//! response upward).

use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, Osc133Marker, ScriptStep};

const CTRL_Z: &[u8] = &[0x1a];

#[tokio::test]
async fn agent_reuse_queues_second_message() {
    let _ = tracing_subscriber::fmt::try_init();

    let Some((orkia, fake)) = resolve_or_skip("agent_reuse_queues_second_message") else {
        return;
    };
    let sandbox = OrkiaSandbox::new().expect("sandbox");

    // Scripted agent that signals prompt-ready once, consumes the first
    // dispatched body, then parks SILENTLY without re-emitting
    // PromptStart. It stays a single long-lived job for the whole test —
    // exactly the live agent the second dispatch must reuse.
    let script = AgentScript {
        name: Some("reuse".into()),
        raw_mode: true,
        steps: vec![
            ScriptStep::Print {
                text: "ready\n".into(),
            },
            ScriptStep::Osc133 {
                marker: Osc133Marker::PromptStart,
                exit_code: None,
            },
            // Consume the first dispatched body (one newline-terminated
            // line injected by the shell prompt loop).
            ScriptStep::AwaitInput {
                bytes: None,
                until: Some("\n".into()),
                timeout_ms: 10_000,
            },
            // Park long enough for the detach, the second dispatch, the
            // settle window, and the assertion. We deliberately do NOT
            // emit another PromptStart.
            ScriptStep::Sleep { ms: 15_000 },
            ScriptStep::Exit { code: 0 },
        ],
    };

    let agent = ScriptedAgent::builder("faye")
        .hooks_provider(Some("claude"))
        .script(script)
        .install(&sandbox, fake.as_path())
        .expect("install agent");
    assert!(agent.dir.join("agent.toml").exists());

    let seal = SealTail::for_agent(sandbox.data_dir(), "faye");

    let mut shell = OrkiaProcess::spawn(
        &orkia,
        &sandbox,
        &[],
        &[("ORKIA_BRIDGE_BIN", orkia.path().to_str().unwrap_or("orkia"))],
        PtyShape::default(),
    )
    .expect("spawn orkia");

    shell
        .pty
        .wait_for_text("❯", Duration::from_secs(20))
        .await
        .expect("shell prompt");

    // First dispatch spawns the agent (and auto-attaches into it).
    shell
        .pty
        .type_line("@faye first message")
        .expect("dispatch 1");

    // Wait until the agent's job exists (its SEAL chain records the spawn).
    seal.wait_for_event_type("agent.spawn", Duration::from_secs(20))
        .await
        .expect("first dispatch must spawn the agent");

    // Detach back to the REPL so the second dispatch reaches the shell
    // rather than splicing into the (auto-attached) agent's PTY.
    shell.pty.write(CTRL_Z).expect("send Ctrl-Z");
    shell
        .pty
        .wait_for_screen_text("❯", Duration::from_secs(10))
        .await
        .expect("prompt must redraw after detach");

    // Second dispatch to the already-live faye. The daemon must route
    // this to the existing job, NOT spawn a new one.
    shell
        .pty
        .type_line("@faye second message")
        .expect("dispatch 2");

    // Give a wrong second spawn ample time to surface (a real spawn takes
    // ~0.5s to materialise its job dir + `agent.spawn` record).
    tokio::time::sleep(Duration::from_secs(3)).await;

    // The substantive assertion: exactly ONE `agent.spawn` across all of
    // faye's job dirs. A regression that spawns on every `@faye` yields 2.
    let all = seal.all();
    let spawn_count = all
        .iter()
        .filter(|e| e.event_type() == Some("agent.spawn"))
        .count();
    assert_eq!(
        spawn_count,
        1,
        "second dispatch must NOT spawn a new job; saw {spawn_count} agent.spawn SEAL records. \
         SEAL events so far: {:#?}",
        all.iter()
            .map(|e| format!("type={:?} seq={:?}", e.event_type(), e.seq()))
            .collect::<Vec<_>>(),
    );

    let _ = shell.pty.write(b"exit\n");
}
