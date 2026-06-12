// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! E2E regression: a NON-ZERO agent exit surfaces `[N]+ Exit N` promptly —
//! NOT a misleading `Done`. Proves the C-robust path: the engine reader
//! reaps the real exit code at the PTY EOF (bounded), the detector carries
//! it on `Closed`, and the state-machine worker prints the exact line off
//! the parked REPL. If the code were unknown/optimistic, this would show
//! `Done` and the `Exit 3` wait would time out (RED).
//!
//! Runs in the `e2e-real-agent` CI job; skips on a plain checkout.

use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, Osc133Marker, ScriptStep};

#[tokio::test]
async fn nonzero_exit_shows_exit_code_not_done() {
    let _ = tracing_subscriber::fmt::try_init();
    let sandbox = OrkiaSandbox::new().expect("sandbox");
    let Some((orkia, fake)) = resolve_or_skip("nonzero_exit_shows_exit_code_not_done") else {
        return;
    };

    // Boots, takes its dispatched body (so the worker learns the name),
    // then exits NON-ZERO on its own — the reader reaps code 3 at EOF.
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
            ScriptStep::Exit { code: 3 },
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

    // No further command: the notice must arrive on its own AND carry the
    // exact non-zero code. `Exit 3` only appears if the reader reaped it;
    // an optimistic `Done` would make this time out.
    shell
        .pty
        .wait_for_text("Exit 3", Duration::from_secs(15))
        .await
        .expect("`[N]+ Exit 3` must surface with the reaped code (not an optimistic `Done`)");
}
