// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! shows orkia's own consent prompt; on `y` orkia records the directory,
//! pre-trusts the provider config, and re-runs the dispatch so the agent
//! spawns and the body is delivered.
//!
//! Runs in the `e2e-real-agent` CI job; skips on a plain checkout.

use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, Osc133Marker, ScriptStep};

#[tokio::test]
async fn untrusted_dir_prompts_then_pretrusts_and_dispatches() {
    let _ = tracing_subscriber::fmt::try_init();
    let sandbox = OrkiaSandbox::new().expect("sandbox");
    // Make the sandbox cwd untrusted so the gate fires.
    sandbox.distrust().expect("distrust");
    let Some((orkia, fake)) =
        resolve_or_skip("untrusted_dir_prompts_then_pretrusts_and_dispatches")
    else {
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
            ScriptStep::AwaitInput {
                bytes: None,
                until: Some("DELIVER-TRUST".into()),
                timeout_ms: 20_000,
            },
            ScriptStep::Hook {
                source: "claude".into(),
                payload: serde_json::json!({
                    "event": "PreToolUse",
                    "tool_name": "Read",
                    "tool_input": {"file_path": "/tmp/trusted.rs"}
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

    // Dispatch in the untrusted dir → consent prompt, NOT a spawn.
    shell
        .pty
        .type_line("@faye DELIVER-TRUST")
        .expect("dispatch agent");
    shell
        .pty
        .wait_for_text("Trust this directory", Duration::from_secs(10))
        .await
        .expect("untrusted dir must show the consent prompt");

    // The agent must NOT have spawned yet (waiting on consent).
    assert!(
        !shell.pty.raw_text().contains("spawned"),
        "agent must not spawn before consent"
    );

    // Consent → orkia pre-trusts + re-dispatches → agent spawns + body
    // is delivered (its PreToolUse hook fires).
    shell.pty.type_line("y").expect("consent");
    let ev = seal
        .wait_for_hook("PreToolUse", Duration::from_secs(30))
        .await
        .expect("after consent the agent must spawn and receive the body");
    assert_eq!(ev.tool(), Some("Read"));

    // Orkia recorded the trust and pre-trusted the claude config.
    assert!(
        sandbox.home().join(".orkia/trusted_dirs.json").exists(),
        "orkia registry must record the trusted dir"
    );
    let claude_cfg = sandbox.home().join(".claude.json");
    assert!(claude_cfg.exists(), "claude config must be pre-trusted");
    let cfg = std::fs::read_to_string(&claude_cfg).unwrap();
    assert!(
        cfg.contains("hasTrustDialogAccepted"),
        "claude config must carry the trust flag: {cfg}"
    );
}
