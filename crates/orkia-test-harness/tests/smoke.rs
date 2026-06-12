// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Smoke test: drive the real `orkia` shell in a PTY, spawn a scripted
//! fake agent via the agent dispatch path (`@<name>`), and assert that
//! the hook flow lands in `journal.jsonl`.
//!
//! Runs in the `e2e-real-agent` CI job, which guarantees
//! `ORKIA_TEST_BIN` and `ORKIA_TEST_FAKE_AGENT_BIN` are set. Local run:
//!
//!     cargo build --release --bin orkia --bin orkia-fake-agent
//!     export ORKIA_TEST_BIN=$PWD/target/release/orkia
//!     export ORKIA_TEST_FAKE_AGENT_BIN=$PWD/target/release/orkia-fake-agent
//!     cargo test -p orkia-test-harness -- --nocapture

use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, Osc133Marker, ScriptStep};

#[tokio::test]
async fn end_to_end_agent_dispatch_emits_pretooluse_hook() {
    let _ = tracing_subscriber::fmt::try_init();

    // 1. Sandbox: hermetic HOME + scaffold.
    let sandbox = OrkiaSandbox::new().expect("sandbox");

    // 2. Locate compiled binaries. Skip (print + return) on a fresh
    // checkout so `cargo test --workspace` stays green; the
    // `e2e-real-agent` CI job sets both env vars and fully exercises.
    let Some((orkia, fake)) = resolve_or_skip("end_to_end_agent_dispatch_emits_pretooluse_hook")
    else {
        return;
    };

    // 3. Define a script: print, mark prompt, fire a PreToolUse hook,
    // wait for one byte (the harness simulates user approval), exit.
    let script = AgentScript {
        name: Some("smoke".into()),
        raw_mode: true,
        steps: vec![
            ScriptStep::Print {
                text: "fake-agent ready\n".into(),
            },
            ScriptStep::Osc133 {
                marker: Osc133Marker::PromptStart,
                exit_code: None,
            },
            ScriptStep::Hook {
                source: "claude".into(),
                payload: serde_json::json!({
                    "event": "PreToolUse",
                    "tool_name": "Read",
                    "tool_input": {"file_path": "/tmp/a.rs"}
                }),
            },
            ScriptStep::AwaitInput {
                bytes: Some(1),
                until: None,
                timeout_ms: 4_000,
            },
            ScriptStep::Print {
                text: "got approval, exiting\n".into(),
            },
            ScriptStep::Exit { code: 0 },
        ],
    };

    let agent = ScriptedAgent::builder("smoker")
        .hooks_provider(Some("claude"))
        .script(script)
        .install(&sandbox, fake.as_path())
        .expect("install agent");
    assert!(agent.dir.join("agent.toml").exists());

    // 4. Tail the agent's SEAL chain. A bare `@agent` dispatch spawns a
    // detached runtime, which records its `PreToolUse`/`PostToolUse`
    // hooks in the per-job SEAL chain (only `AgentFinalResponse` is
    // forwarded to the unified journal). So the durable observation
    // surface for tool-use hooks is `agents/<name>/jobs/<N>/seal.jsonl`.
    let seal = SealTail::for_agent(sandbox.data_dir(), "smoker");

    // 5. Spawn the shell.
    let mut shell = OrkiaProcess::spawn(
        &orkia,
        &sandbox,
        &[],
        // Ensure the fake-agent's `orkia bridge` invocation resolves
        // to the same binary we just spawned the shell from.
        &[("ORKIA_BRIDGE_BIN", orkia.path().to_str().unwrap_or("orkia"))],
        PtyShape::default(),
    )
    .expect("spawn orkia");

    // 6. Wait for the prompt to be drawn, then dispatch the agent.
    // The exact prompt glyph depends on the shell renderer; we
    // tolerate either the project marker or the generic '$'.
    shell
        .pty
        .wait_for_text("❯", Duration::from_secs(10))
        .await
        .expect("shell prompt");

    shell
        .pty
        .type_line("@smoker hello")
        .expect("dispatch agent");

    // 7. Assert the agent's PreToolUse hook lands in the SEAL chain.
    let ev = seal
        .wait_for_hook("PreToolUse", Duration::from_secs(10))
        .await
        .expect("PreToolUse seal event");
    assert_eq!(ev.tool(), Some("Read"));
    assert_eq!(ev.target(), Some("tmp/a.rs"));

    // Terminal-lifecycle assertions (agent exit / SIGCHLD-driven
    // `completed` envelope) deliberately live in their own focused
    // tests. They depend on `drain_job_events` firing post-SIGCHLD,
    // which today requires a user keystroke to wake `read_line`. See
    // `crates/orkia-test-harness/tests/agent_reuse.rs` for the
    // alive-job assertions this harness can make today.
}
