// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! E2E: a scripted agent emits a Stop hook with a transcript_path; the
//! shell's auto-wired `FinalResponseService` extracts the assistant text,
//! persists `final-response.md` under the run dir, and publishes an
//! `AgentFinalResponse` envelope on the journal.
//!
//! Closes GAP-004 from `audits/E2E-FAIL-SOFT.md`: a regression that
//! breaks the boot-time wiring of `FinalResponseService` (e.g. someone
//! comments out the line in `repl.rs` that installs the Stop hook) makes
//! this test fail loudly instead of silently.

use std::path::PathBuf;
use std::time::Duration;

use orkia_test_harness::prelude::*;
use orkia_test_harness::pty::PtyShape;
use orkia_test_harness::script::{AgentScript, ScriptStep};

#[tokio::test]
async fn final_response_capture_on_stop_hook() {
    let _ = tracing_subscriber::fmt::try_init();

    let Some((orkia, fake)) = resolve_or_skip("final_response_capture_on_stop_hook") else {
        return;
    };
    let sandbox = OrkiaSandbox::new().expect("sandbox");

    // Write a Claude-shaped transcript with one final assistant message.
    // It must live under `~/.claude/projects/<slug>/` so the extractor's
    // SEC-029 confinement (`confine_hint`, claude extractor) accepts the
    // `transcript_path` hint — a hint outside that root is fail-closed and
    // falls through to a session-id scan that finds nothing.
    let transcript_dir = sandbox
        .home()
        .join(".claude")
        .join("projects")
        .join("orkia-test");
    std::fs::create_dir_all(&transcript_dir).expect("create transcript dir");
    let transcript_path = transcript_dir.join("transcript.jsonl");
    std::fs::write(
        &transcript_path,
        concat!(
            r#"{"type":"user","message":{"content":[{"type":"text","text":"apply the fix"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"The fix is complete."}]}}"#,
            "\n",
        ),
    )
    .expect("write transcript");

    // Scripted agent: emit a Stop hook from provider `claude` whose
    // payload carries the transcript_path. Then exit cleanly.
    let script = AgentScript {
        name: Some("final-response".into()),
        raw_mode: true,
        steps: vec![
            ScriptStep::Print {
                text: "working...\n".into(),
            },
            ScriptStep::Hook {
                source: "claude".into(),
                payload: serde_json::json!({
                    "event": "Stop",
                    "transcript_path": transcript_path.to_string_lossy(),
                }),
            },
            // Stay alive while the detached runtime's FinalResponseService
            // consumes the Stop hook, extracts the transcript, writes
            // `final-response.md`, and forwards the `AgentFinalResponse`
            // envelope up to the daemon's disk-owning hub. Exiting the
            // instant the Stop hook fires races that teardown and drops
            // the AFR (only AFR is forwarded from a detached runtime).
            ScriptStep::Sleep { ms: 1_500 },
            ScriptStep::Exit { code: 0 },
        ],
    };

    let agent = ScriptedAgent::builder("faye")
        .hooks_provider(Some("claude"))
        .script(script)
        .install(&sandbox, fake.as_path())
        .expect("install agent");
    assert!(agent.dir.join("agent.toml").exists());

    let tail = JournalTail::start(sandbox.journal_path()).expect("tail");

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
        .wait_for_text("❯", Duration::from_secs(10))
        .await
        .expect("shell prompt");

    shell
        .pty
        .type_line("@faye apply the fix")
        .expect("dispatch agent");

    // Wait for the AgentFinalResponse envelope — this is the substantive
    // signal that the FinalResponseService consumed the Stop hook and
    // completed extraction.
    let env = tail
        .wait_for_event(
            Duration::from_secs(15),
            |e| e.event_type() == Some("hook") && e.event() == Some("AgentFinalResponse"),
            "hook:AgentFinalResponse",
        )
        .await
        .expect("AgentFinalResponse envelope");

    // The envelope must carry a non-empty response_path and a positive
    // byte count. A fail-soft regression where extraction silently fails
    // would emit a failure envelope with response_path absent and bytes
    // == 0 — distinguishing those is the whole point of this test.
    let response_path = env
        .get("response_path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .expect("response_path must be present on a successful extraction");
    let response_bytes = env
        .get("response_bytes")
        .and_then(|v| v.as_u64())
        .expect("response_bytes must be present");
    assert!(
        response_bytes > 0,
        "response_bytes must be > 0 for a non-empty assistant message, got {response_bytes}",
    );

    // The file pointed to by the envelope must exist and contain the
    // assistant text. Failing here (rather than silently passing on an
    // absent file) is GAP-004 closure.
    assert!(
        response_path.exists(),
        "final-response file must exist at {}",
        response_path.display(),
    );
    let on_disk = std::fs::read_to_string(&response_path).expect("read final-response.md");
    assert!(
        on_disk.contains("The fix is complete."),
        "final-response.md must contain assistant text, got: {on_disk:?}",
    );
    assert!(
        response_path.file_name().and_then(|n| n.to_str()) == Some("final-response.md"),
        "envelope must point at final-response.md, got {}",
        response_path.display(),
    );

    let _ = shell.pty.write(b"exit\n");
}
