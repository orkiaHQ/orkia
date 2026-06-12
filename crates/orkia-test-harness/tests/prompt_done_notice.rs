// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! E2E regression: an agent's `[N]+ Done` completion notice must surface
//! the instant the agent exits, NOT be deferred until the user's next
//! command unparks `read_line`.
//!
//! Live observation: after Ctrl-C'ing a faye session, `[2]+ Done faye`
//! only appeared when the user next typed something (`cd ...`). The
//! SIGCHLD "fast-path" sent a channel nudge that cannot wake a blocked
//! `read_line`, so the reap+print sat until the loop next iterated. The
//! fix routes the notice through the state-machine worker's
//! `ExternalPrinter` (off-REPL) on the detector's `Closed` event.
//!
//! This test dispatches an agent that exits on its own, then asserts the
//! `Done` line appears WITHOUT sending any further command. On the old
//! code the line never arrives within the window (RED); with the fix it
//! appears promptly (GREEN).
//!
//! Runs in the `e2e-real-agent` CI job; skips on a plain checkout.

use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, Osc133Marker, ScriptStep};

#[tokio::test]
async fn agent_done_notice_appears_without_a_followup_command() {
    let _ = tracing_subscriber::fmt::try_init();
    let sandbox = OrkiaSandbox::new().expect("sandbox");
    let Some((orkia, fake)) =
        resolve_or_skip("agent_done_notice_appears_without_a_followup_command")
    else {
        return;
    };

    // Agent boots, accepts its dispatched body (so the worker learns its
    // name), idles briefly, then exits on its own — closing its PTY,
    // which fires the detector `Closed` the worker keys off.
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
            ScriptStep::EchoUntilSubmit { timeout_ms: 20_000 },
            ScriptStep::Sleep { ms: 800 },
            ScriptStep::Exit { code: 0 },
        ],
    };

    ScriptedAgent::builder("faye")
        .hooks_provider(Some("claude"))
        .script(script)
        .install(&sandbox, fake.as_path())
        .expect("install agent");

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

    shell.pty.type_line("@faye TOKEN").expect("dispatch");

    // Crucially: send NOTHING else. The Done notice must arrive on its
    // own when the agent exits, surfaced off-REPL by the state-machine
    // worker — not deferred to the next command that unparks read_line.
    shell
        .pty
        .wait_for_text("Done", Duration::from_secs(15))
        .await
        .expect("`[N]+ Done` must surface the instant the agent exits, with no further command");
}
