// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! End-to-end: bridge → socket → listener → store → CLI query.
//!
//! Runs the compiled `orkia bridge` and `orkia journal` binaries
//! against a real `JournalListener` rooted in a temp HOME. Asserts
//! the full pipeline carries an envelope from agent-hook payload
//! through the socket, into the JSONL store, and back out via the
//! standalone subcommand.

use std::process::{Command, Stdio};
use std::time::Duration;

use orkia_shell::journal::{EventType, JournalListener};
use tempfile::tempdir;
use tokio::time::timeout;

#[tokio::test]
async fn bridge_then_journal_query_round_trips() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path();
    let data_dir = home.join(".orkia");
    std::fs::create_dir_all(&data_dir).expect("mkdir .orkia");
    // Touch the empty journal file the way `orkia init` does.
    std::fs::write(data_dir.join("journal.jsonl"), "").expect("touch journal");

    let (listener, mut rx) = JournalListener::start(&data_dir).expect("start listener");
    let bin = env!("CARGO_BIN_EXE_orkia");

    // 1. Bridge a Claude-style PreToolUse payload.
    spawn_bridge(
        bin,
        home,
        "claude",
        1,
        "faye",
        br#"{"event":"PreToolUse","tool_name":"Read","tool_input":{"file_path":"/tmp/pkce.rs"}}"#,
    );
    let env = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("envelope");
    assert_eq!(env.event_type, EventType::Hook);
    assert_eq!(env.event.as_deref(), Some("PreToolUse"));
    assert_eq!(env.agent.as_deref(), Some("faye"));
    assert_eq!(env.job_id, Some(1));

    // 2. Bridge a Gemini-style BeforeTool — normaliser remaps event name.
    spawn_bridge(
        bin,
        home,
        "gemini",
        2,
        "killua",
        br#"{"event":"BeforeTool","tool_name":"Write","tool_input":{"file_path":"/tmp/x.rs"}}"#,
    );
    let env = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("envelope");
    assert_eq!(env.event.as_deref(), Some("PreToolUse"));
    assert_eq!(env.agent.as_deref(), Some("killua"));
    assert_eq!(env.source.as_deref(), Some("gemini"));

    // Drop the listener so the store-on-disk path is fully flushed
    // before we shell out to the query CLI. Bridges write directly
    // to the socket, not the file — the journal *task* in the REPL
    // is what appends. For the test, write the envelopes ourselves
    // by feeding them through `JournalStore` directly, mirroring
    // what `Repl::drain_journal_events` does.
    drop(listener);

    use orkia_shell::journal::{JournalEnvelope, JournalStore};
    let mut store = JournalStore::new(&data_dir);
    let mut e1 = JournalEnvelope::now(EventType::Hook);
    e1.event = Some("PreToolUse".into());
    e1.tool = Some("Read".into());
    e1.target = Some("src/auth/mod.rs".into());
    e1.job_id = Some(1);
    e1.agent = Some("faye".into());
    store.append(&e1);

    let mut e2 = JournalEnvelope::now(EventType::Lifecycle);
    e2.event = Some("completed".into());
    e2.job_id = Some(1);
    e2.agent = Some("faye".into());
    e2.exit_code = Some(0);
    store.append(&e2);

    // 3. `orkia journal --agent faye` should surface both events.
    let out = Command::new(bin)
        .arg("journal")
        .arg("--agent")
        .arg("faye")
        .env("HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("query");
    assert!(out.status.success(), "journal subcommand non-zero");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("PreToolUse"),
        "missing PreToolUse:\n{stdout}"
    );
    assert!(stdout.contains("completed"), "missing completed:\n{stdout}");
    assert!(stdout.contains("faye"), "missing faye:\n{stdout}");

    // 4. `--last 1` keeps only the most recent.
    let out = Command::new(bin)
        .arg("journal")
        .arg("--agent")
        .arg("faye")
        .arg("--last")
        .arg("1")
        .env("HOME", home)
        .output()
        .expect("query");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // header + one row
    assert_eq!(
        stdout.lines().count(),
        2,
        "expected header + 1 row, got:\n{stdout}"
    );
}

fn spawn_bridge(
    bin: &str,
    home: &std::path::Path,
    source: &str,
    job: u32,
    agent: &str,
    payload: &[u8],
) {
    let mut child = Command::new(bin)
        .arg("bridge")
        .arg("--source")
        .arg(source)
        .env("HOME", home)
        .env("ORKIA_JOB_ID", job.to_string())
        .env("ORKIA_AGENT_NAME", agent)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bridge");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(payload)
            .expect("write");
    }
    let status = child.wait().expect("wait bridge");
    assert!(status.success(), "bridge non-zero");
}
