// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! E2E regression for FOLLOW-UP prompt delivery.
//!
//! Live observation (real claude): after faye delivered its first body and
//! went idle, a second `@faye <body>` printed `▸ queued for [1]` but the
//! body was never typed into the agent — on re-attach claude still showed
//! the previous turn. This test pins the contract deterministically: a
//! follow-up body queued via `deliver_to_existing_agent` -> `append_body`
//! must be force-injected into the now-idle agent, exactly like the boot
//! body was.
//!
//! The fake agent models claude's relevant behaviour: it ECHOes each typed
//! body (so the echo bytes re-arm the detector's `already_notified` latch
//! and the grid-confirm sees the text) and fires a hook only after the
//! committing `\r`. Two distinguishable hooks (Read = body 1, Write =
//! body 2 / the follow-up) let us assert each delivery independently:
//! if the follow-up is dropped, the `Write` hook never fires and the test
//! goes RED.
//!
//! Runs in the `e2e-real-agent` CI job; skips on a plain checkout.

use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, Osc133Marker, ScriptStep};

#[tokio::test]
async fn followup_body_is_injected_into_idle_agent() {
    let _ = tracing_subscriber::fmt::try_init();
    let sandbox = OrkiaSandbox::new().expect("sandbox");
    let Some((orkia, fake)) = resolve_or_skip("followup_body_is_injected_into_idle_agent") else {
        return;
    };

    let script = AgentScript {
        name: Some("faye".into()),
        raw_mode: true,
        steps: vec![
            ScriptStep::DrainInput { ms: 1_000 },
            ScriptStep::Print {
                text: "faye ready\n".into(),
            },
            ScriptStep::Osc133 {
                marker: Osc133Marker::PromptStart,
                exit_code: None,
            },
            // --- body 1 (the boot body) ---
            ScriptStep::EchoUntilSubmit { timeout_ms: 20_000 },
            ScriptStep::Hook {
                source: "claude".into(),
                payload: serde_json::json!({
                    "event": "PreToolUse",
                    "tool_name": "Read",
                    "tool_input": {"file_path": "/tmp/body1"}
                }),
            },
            // Agent "responds" and re-prompts, then goes idle again — the
            // echo + response bytes re-arm the detector, and the queue
            // drains to Idle so a follow-up `append_body` flips it back to
            // WaitingForReady.
            ScriptStep::Print {
                text: "\nresponse-one\n".into(),
            },
            ScriptStep::Osc133 {
                marker: Osc133Marker::PromptStart,
                exit_code: None,
            },
            // --- body 2 (the FOLLOW-UP) ---
            ScriptStep::EchoUntilSubmit { timeout_ms: 20_000 },
            ScriptStep::Hook {
                source: "claude".into(),
                payload: serde_json::json!({
                    "event": "PreToolUse",
                    "tool_name": "Write",
                    "tool_input": {"file_path": "/tmp/body2"}
                }),
            },
            ScriptStep::Sleep { ms: 500 },
            ScriptStep::Exit { code: 0 },
        ],
    };

    ScriptedAgent::builder("faye")
        .hooks_provider(Some("claude"))
        .script(script)
        .install(&sandbox, fake.as_path())
        .expect("install agent");

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

    // Dispatch 1 spawns faye and delivers the boot body.
    shell.pty.type_line("@faye INITIAL").expect("dispatch 1");
    let h1 = seal
        .wait_for(
            Duration::from_secs(25),
            |e| e.event_type() == Some("hook.PreToolUse") && e.tool() == Some("Read"),
            "boot body delivered (Read hook)",
        )
        .await
        .expect("boot body must be delivered");
    assert_eq!(h1.tool(), Some("Read"));

    // Let the agent print its response, re-prompt, and settle to idle so
    // the queue is empty (state Idle) before the follow-up is appended.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Dispatch 2: a FOLLOW-UP to the already-running, idle faye. This goes
    // through `deliver_to_existing_agent` -> `append_body` (NOT a spawn).
    shell.pty.type_line("@faye FOLLOWUP").expect("dispatch 2");

    // THE assertion: the follow-up body must be force-injected into the
    // idle agent and committed (Write hook). RED if the queued body is
    // never typed in.
    let h2 = seal
        .wait_for(
            Duration::from_secs(30),
            |e| e.event_type() == Some("hook.PreToolUse") && e.tool() == Some("Write"),
            "follow-up body delivered (Write hook)",
        )
        .await
        .expect("FOLLOW-UP body must be injected into the idle agent");
    assert_eq!(h2.tool(), Some("Write"));
}
