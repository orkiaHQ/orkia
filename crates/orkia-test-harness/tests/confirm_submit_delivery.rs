// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Contract: the injection executor types the body, confirms it
//! landed in the agent's grid, and then SUBMITS it with `\r`.
//!
//! This agent echoes typed input (so the body shows up in orkia's live
//! grid — what `grid_probe` reads to confirm) and fires its hook ONLY
//! after a submit byte (CR/LF) arrives. So the hook fires iff the
//! executor sent the trailing `\r` through the full grid-probe-wired
//! path. If a regression ever drops the submit (e.g. a botched flip to
//! fail-closed with a broken matcher), this agent hangs and the test
//! goes RED.
//!
//! Runs in the `e2e-real-agent` CI job; skips on a plain checkout.

use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, Osc133Marker, ScriptStep};

#[tokio::test]
async fn body_is_submitted_after_grid_confirm() {
    let _ = tracing_subscriber::fmt::try_init();
    let sandbox = OrkiaSandbox::new().expect("sandbox");
    let Some((orkia, fake)) = resolve_or_skip("body_is_submitted_after_grid_confirm") else {
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
            // Echo the typed body (so it lands in orkia's grid and the
            // executor's confirm sees it), and only proceed once the
            // submit (\r) arrives.
            ScriptStep::EchoUntilSubmit { timeout_ms: 20_000 },
            // Fires only after the submit byte was received.
            ScriptStep::Hook {
                source: "claude".into(),
                payload: serde_json::json!({
                    "event": "PreToolUse",
                    "tool_name": "Read",
                    "tool_input": {"file_path": "/tmp/submitted.rs"}
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

    shell
        .pty
        .type_line("@faye CONFIRMTOKEN")
        .expect("dispatch agent");

    // The hook fires only if the executor confirmed the body in the grid
    // and then sent the submit (\r).
    let ev = seal
        .wait_for_hook("PreToolUse", Duration::from_secs(25))
        .await
        .expect("body must be submitted after grid-confirm");
    assert_eq!(ev.tool(), Some("Read"));
}
