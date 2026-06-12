// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Regression: attach → Ctrl-Z detach → re-attach must rebuild the
//! agent's screen, not show a black screen.
//!
//! The bug: the alacritty grid was never advanced for an agent engine
//! (an agent TUI never emits the OSC-133 C/D command markers a shell
//! uses), so `render_visible_snapshot` — what the attach pump paints on
//! attach — was blank. The first attach happened to work by replaying
//! the buffered raw-output backlog; the second attach (backlog drained,
//! agent idle) had nothing to replay and painted a blank screen. The
//! fix marks agent engines `persistent_program` so the grid is always
//! live.
//!
//! We assert the agent's marker is on the CURRENT screen after the
//! *second* attach using a screen-only wait — `raw_text` still holds the
//! first attach's replayed backlog and would mask the regression. The
//! agent prints once then blocks silently, so the only thing that can
//! repaint the marker on re-attach is the reconstructed grid.
//!
//! Runs in the `e2e-real-agent` CI job (which sets `ORKIA_TEST_BIN` and
//! `ORKIA_TEST_FAKE_AGENT_BIN`); skips on a plain checkout.

use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, Osc133Marker, ScriptStep};

const CTRL_Z: &[u8] = &[0x1a];

#[tokio::test]
async fn reattach_after_detach_reconstructs_screen() {
    let _ = tracing_subscriber::fmt::try_init();
    let sandbox = OrkiaSandbox::new().expect("sandbox");
    let Some((orkia, fake)) = resolve_or_skip("reattach_after_detach_reconstructs_screen") else {
        return;
    };

    let marker = "SCREEN-MARK-9Q2";
    let script = AgentScript {
        name: Some("faye2".into()),
        raw_mode: true,
        steps: vec![
            ScriptStep::Print {
                text: format!("{marker}\n"),
            },
            ScriptStep::Osc133 {
                marker: Osc133Marker::PromptStart,
                exit_code: None,
            },
            // Stay alive and SILENT across the attach/detach/re-attach
            // dance — `until` never matches, so the agent never prints
            // again. The only way the marker reappears on re-attach is
            // the grid snapshot.
            ScriptStep::AwaitInput {
                bytes: None,
                until: Some("NEVER-SENT".into()),
                timeout_ms: 40_000,
            },
            ScriptStep::Exit { code: 0 },
        ],
    };

    ScriptedAgent::builder("faye2")
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

    // Spawn the agent. Since the daemon-owned-`@name` flip the dispatch
    // auto-attaches into the agent's screen rather than printing a
    // "spawned as background" line, so the boot output replays straight
    // away and the marker shows — this is the (now implicit) first
    // attach, which worked even before the grid-snapshot fix.
    shell.pty.type_line("@faye2 show").expect("dispatch agent");
    shell
        .pty
        .wait_for_text(marker, Duration::from_secs(10))
        .await
        .expect("auto-attach must show the agent screen");

    // Ctrl-Z detach → shell prompt returns.
    shell.pty.write(CTRL_Z).expect("send Ctrl-Z");
    shell
        .pty
        .wait_for_screen_text("❯", Duration::from_secs(10))
        .await
        .expect("prompt must redraw after detach");

    // Re-attach. The backlog is drained and the agent is silent, so the
    // marker can only come back from the reconstructed grid.
    shell.pty.type_line("attach @faye2").expect("re-attach");
    shell
        .pty
        .wait_for_screen_text(marker, Duration::from_secs(10))
        .await
        .expect("re-attach must reconstruct the agent screen (not a black screen)");
}
