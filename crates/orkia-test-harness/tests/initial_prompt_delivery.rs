// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Regression: `@agent <task>` must deliver `<task>` to a freshly
//! spawned hook-driven agent.
//!
//! The bug: the first prompt of a hook-driven agent was written to the
//! PTY at spawn (`StdinSource::InitialBytes`), before a TUI agent like
//! claude has entered raw mode / drawn its input box — so the agent's
//! startup swallowed it and the prompt was silently lost. The fix
//! routes the first body through the detector-gated injection path so
//! it lands once the agent is idle at its prompt.
//!
//! The fake agent here discards stdin during a startup window
//! (`DrainInput`) — exactly how a real TUI agent loses bytes written too
//! early — then blocks for the body and fires a PreToolUse hook only
//! after it arrives. The hook's presence in the journal proves
//! delivery. On the pre-fix code the body dies in the startup drain and
//! the hook never fires.
//!
//! Runs in the `e2e-real-agent` CI job (which sets `ORKIA_TEST_BIN` and
//! `ORKIA_TEST_FAKE_AGENT_BIN`); skips on a plain checkout.

use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, Osc133Marker, ScriptStep};

#[tokio::test]
async fn initial_prompt_is_delivered_to_fresh_agent() {
    let _ = tracing_subscriber::fmt::try_init();
    let sandbox = OrkiaSandbox::new().expect("sandbox");
    let Some((orkia, fake)) = resolve_or_skip("initial_prompt_is_delivered_to_fresh_agent") else {
        return;
    };

    let script = AgentScript {
        name: Some("faye".into()),
        raw_mode: true,
        steps: vec![
            // Swallow anything written to the PTY during startup — the
            // racy `InitialBytes` of the old path land (and die) here.
            ScriptStep::DrainInput { ms: 1_000 },
            // Now the agent is "ready" at a plain (Generic) prompt.
            ScriptStep::Print {
                text: "faye ready\n".into(),
            },
            ScriptStep::Osc133 {
                marker: Osc133Marker::PromptStart,
                exit_code: None,
            },
            // Block (in poll) for the injected body. The detector only
            // delivers it once the agent is idle here.
            ScriptStep::AwaitInput {
                bytes: None,
                until: Some("DELIVER-XYZ".into()),
                timeout_ms: 20_000,
            },
            // Fires ONLY after the body arrives — our delivery signal.
            ScriptStep::Hook {
                source: "claude".into(),
                payload: serde_json::json!({
                    "event": "PreToolUse",
                    "tool_name": "Read",
                    "tool_input": {"file_path": "/tmp/delivered.rs"}
                }),
            },
            ScriptStep::Print {
                text: "delivered\n".into(),
            },
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

    // Dispatch with the body that must be delivered.
    shell
        .pty
        .type_line("@faye DELIVER-XYZ")
        .expect("dispatch agent");

    // The PreToolUse hook only fires after the agent received the body.
    let ev = seal
        .wait_for_hook("PreToolUse", Duration::from_secs(25))
        .await
        .expect("initial prompt must be delivered to the fresh agent");
    assert_eq!(ev.tool(), Some("Read"));
}
