// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Regression: the initial prompt must be delivered even when the user
//! has ATTACHED to the agent — i.e. while the prompt detector is muted.
//!
//! The pre-fix bug: `on_user_attached` set a per-job mute, and the
//! detector loop did a blanket `continue` on mute that suppressed
//! INJECTION, not just Attention toasts. So `@faye <task>` followed by
//! an immediate `attach` (the user's exact flow) never delivered the
//! body — it sat queued until detach. The fix narrows the mute to
//! Attention only; injection still fires while attached.
//!
//! This test reproduces that exact flow: dispatch, then attach
//! immediately so the detector is muted at injection time, and assert
//! the agent received the body (its PreToolUse hook only fires after
//! the body arrives). RED on the pre-fix binary (mute suppresses
//! injection), GREEN now.
//!
//! Runs in the `e2e-real-agent` CI job (sets `ORKIA_TEST_BIN` +
//! `ORKIA_TEST_FAKE_AGENT_BIN`); skips on a plain checkout.

use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, Osc133Marker, ScriptStep};

#[tokio::test]
async fn initial_prompt_is_delivered_while_attached_and_muted() {
    let _ = tracing_subscriber::fmt::try_init();
    let sandbox = OrkiaSandbox::new().expect("sandbox");
    let Some((orkia, fake)) =
        resolve_or_skip("initial_prompt_is_delivered_while_attached_and_muted")
    else {
        return;
    };

    let script = AgentScript {
        name: Some("faye".into()),
        raw_mode: true,
        steps: vec![
            // Swallow racy startup bytes (old InitialBytes path would die here).
            ScriptStep::DrainInput { ms: 1_000 },
            ScriptStep::Print {
                text: "faye ready\n".into(),
            },
            ScriptStep::Osc133 {
                marker: Osc133Marker::PromptStart,
                exit_code: None,
            },
            // Block (poll) for the injected body — delivered by the
            // detector while the user is attached (muted).
            ScriptStep::AwaitInput {
                bytes: None,
                until: Some("DELIVER-MUTED".into()),
                timeout_ms: 20_000,
            },
            // Fires only after the body arrived — proof of delivery.
            ScriptStep::Hook {
                source: "claude".into(),
                payload: serde_json::json!({
                    "event": "PreToolUse",
                    "tool_name": "Read",
                    "tool_input": {"file_path": "/tmp/muted.rs"}
                }),
            },
            // Stay alive briefly so the attached splice doesn't tear down
            // before the journal write lands.
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

    // Dispatch, then attach IMMEDIATELY so the detector is muted at the
    // moment injection fires — the user's exact failing flow.
    shell
        .pty
        .type_line("@faye DELIVER-MUTED")
        .expect("dispatch agent");
    // Wait for the agent to actually exist before attaching. Since the
    // daemon-owned-`@name` flip the dispatch auto-attaches into the
    // agent's screen rather than printing a "spawned as background"
    // line, so the durable readiness signal is the per-job SEAL chain's
    // `agent.spawn`, not a REPL confirmation string.
    seal.wait_for_event_type("agent.spawn", Duration::from_secs(10))
        .await
        .expect("agent must spawn before attach");
    shell.pty.type_line("attach @faye").expect("attach");

    // The hook fires only if the body was delivered while attached/muted.
    let ev = seal
        .wait_for_hook("PreToolUse", Duration::from_secs(25))
        .await
        .expect("body must be delivered even while attached (detector muted)");
    assert_eq!(ev.tool(), Some("Read"));
}
