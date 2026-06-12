// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! End-to-end: spawn a journal listener, pipe a payload through the
//! compiled `orkia bridge` binary, and assert the envelope arrives.

use std::process::{Command, Stdio};
use std::time::Duration;

use orkia_shell::journal::{EventType, JournalListener};
use tempfile::tempdir;
use tokio::time::timeout;

#[tokio::test]
async fn bridge_delivers_payload_to_listener() {
    let dir = tempdir().expect("tempdir");

    // Use a custom HOME so the bridge resolves `~/.orkia/run/orkia.sock`
    // inside the temp dir. The listener uses the same convention via
    // `data_dir.join("run")`, so we point its data_dir at `<home>/.orkia`.
    let home = dir.path();
    let data_dir = home.join(".orkia");
    std::fs::create_dir_all(&data_dir).expect("mkdir .orkia");

    let (listener, mut rx) = JournalListener::start(&data_dir).expect("start listener");
    assert!(listener.socket_path().exists());

    let bin = env!("CARGO_BIN_EXE_orkia");
    let mut child = Command::new(bin)
        .arg("bridge")
        .arg("--source")
        .arg("claude")
        .env("HOME", home)
        .env("ORKIA_JOB_ID", "42")
        .env("ORKIA_AGENT_NAME", "faye")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn orkia bridge");

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin
            .write_all(
                br#"{"event":"PreToolUse","tool_name":"Read","tool_input":{"file_path":"/tmp/a.rs"}}"#,
            )
            .expect("write");
    }
    let status = child.wait().expect("wait");
    assert!(status.success(), "bridge exited non-zero");

    let env = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for envelope")
        .expect("envelope");
    assert_eq!(env.event_type, EventType::Hook);
    assert_eq!(env.source.as_deref(), Some("claude"));
    assert_eq!(env.event.as_deref(), Some("PreToolUse"));
    assert_eq!(env.tool.as_deref(), Some("Read"));
    assert_eq!(env.job_id, Some(42));
    assert_eq!(env.agent.as_deref(), Some("faye"));
}
