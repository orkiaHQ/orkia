// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! (a provider orkia can't pre-trust, e.g. gemini/kimi), orkia
//! auto-answers it — but only in the boot window — because the directory
//! was already consented to (here: the sandbox is pre-trusted).
//!
//! The fake agent renders a yes/no menu, waits for the accept keystroke
//! (which orkia sends automatically), then proceeds to receive the body
//! and fire its hook. If orkia did NOT auto-answer, the agent would hang
//! on the menu and the hook would never fire.
//!
//! Runs in the `e2e-real-agent` CI job; skips on a plain checkout.

use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, Osc133Marker, ScriptStep};

#[tokio::test]
async fn agent_boot_trust_modal_is_auto_answered() {
    let _ = tracing_subscriber::fmt::try_init();
    // Sandbox is pre-trusted by default → the agent spawns directly, then
    // shows its own modal which orkia must auto-confirm.
    let sandbox = OrkiaSandbox::new().expect("sandbox");
    let Some((orkia, fake)) = resolve_or_skip("agent_boot_trust_modal_is_auto_answered") else {
        return;
    };

    let script = AgentScript {
        name: Some("faye".into()),
        raw_mode: true,
        steps: vec![
            ScriptStep::DrainInput { ms: 500 },
            // A yes/no trust menu — classifies as MultipleChoice, so the
            // detector flags it as a boot prompt (WaitingForApproval).
            ScriptStep::Print {
                text: "Do you trust this folder?\n  1. Yes, trust\n  2. No, exit\n".into(),
            },
            // Wait for orkia's auto-answer keystroke (Enter = 1 byte).
            ScriptStep::AwaitInput {
                bytes: Some(1),
                until: None,
                timeout_ms: 15_000,
            },
            ScriptStep::Print {
                text: "trusted, ready\n".into(),
            },
            ScriptStep::Osc133 {
                marker: Osc133Marker::PromptStart,
                exit_code: None,
            },
            // Now the real body comes through the detector-gated injection.
            ScriptStep::AwaitInput {
                bytes: None,
                until: Some("DELIVER-BODY".into()),
                timeout_ms: 20_000,
            },
            ScriptStep::Hook {
                source: "claude".into(),
                payload: serde_json::json!({
                    "event": "PreToolUse",
                    "tool_name": "Read",
                    "tool_input": {"file_path": "/tmp/autotrust.rs"}
                }),
            },
            ScriptStep::Sleep { ms: 500 },
            ScriptStep::Exit { code: 0 },
        ],
    };

    ScriptedAgent::builder("faye")
        // gemini-style provider: orkia can't pre-trust its config, so the
        // agent's own modal appears and the auto-answer path is exercised.
        .hooks_provider(Some("gemini"))
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
        .type_line("@faye DELIVER-BODY")
        .expect("dispatch agent");

    // The hook only fires if orkia auto-answered the boot modal AND then
    // delivered the body — i.e. the agent didn't hang on the menu.
    let ev = seal
        .wait_for_hook("PreToolUse", Duration::from_secs(35))
        .await
        .expect("orkia must auto-answer the boot modal so the agent proceeds");
    assert_eq!(ev.tool(), Some("Read"));
}
