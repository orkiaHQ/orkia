// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;
use orkia_shell_types::JobId;
use orkia_shell_types::journal::types::EventType;
use tempfile::tempdir;
use tokio::io::AsyncWriteExt;
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn delivers_envelope_over_socket() {
    let dir = tempdir().expect("tempdir");
    let (listener, mut rx) = JournalListener::start(dir.path()).expect("start");
    let path = listener.socket_path().to_path_buf();

    let mut client = UnixStream::connect(&path).await.expect("connect");
    let line = r#"{"type":"hook","timestamp":"2026-05-20T10:00:00+00:00","job_id":1,"event":"PreToolUse"}"#;
    client
        .write_all(format!("{line}\n").as_bytes())
        .await
        .expect("write");
    client.shutdown().await.ok();

    let env = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("envelope");
    assert_eq!(env.event_type, EventType::Hook);
    assert_eq!(env.job_id, Some(1));
    assert_eq!(env.event.as_deref(), Some("PreToolUse"));
}

#[tokio::test]
async fn start_at_binds_explicit_path_and_creates_parent() {
    // somewhere OTHER than `<data_dir>/run/orkia.sock`, with a parent dir that
    // does not exist yet — `start_at` must create it and bind there.
    let dir = tempdir().expect("tempdir");
    let socket_path = dir.path().join("jobs").join("7").join("agent.sock");
    assert!(!socket_path.parent().expect("parent").exists());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let listener =
        JournalListener::start_at(socket_path.clone(), LiveJournalHandlers::default(), tx)
            .expect("start_at");
    assert_eq!(listener.socket_path(), socket_path.as_path());
    assert!(
        socket_path.exists(),
        "socket file bound at the override path"
    );

    let mut client = UnixStream::connect(&socket_path).await.expect("connect");
    let line =
        r#"{"type":"hook","timestamp":"2026-05-20T10:00:00+00:00","job_id":7,"event":"Stop"}"#;
    client
        .write_all(format!("{line}\n").as_bytes())
        .await
        .expect("write");
    client.shutdown().await.ok();

    let env = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("envelope");
    assert_eq!(env.event_type, EventType::Hook);
    assert_eq!(env.job_id, Some(7));
    assert_eq!(env.event.as_deref(), Some("Stop"));
}

#[tokio::test]
async fn multiple_envelopes_one_connection() {
    let dir = tempdir().expect("tempdir");
    let (listener, mut rx) = JournalListener::start(dir.path()).expect("start");
    let mut client = UnixStream::connect(listener.socket_path())
        .await
        .expect("connect");
    let lines = [
        r#"{"type":"hook","timestamp":"2026-05-20T10:00:00+00:00","job_id":1}"#,
        r#"{"type":"lifecycle","timestamp":"2026-05-20T10:00:01+00:00","job_id":1,"event":"spawn"}"#,
        r#"{"type":"hook","timestamp":"2026-05-20T10:00:02+00:00","job_id":1,"event":"Stop"}"#,
    ];
    for l in lines {
        client
            .write_all(format!("{l}\n").as_bytes())
            .await
            .expect("write");
    }
    client.shutdown().await.ok();

    for _ in 0..3 {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("envelope");
    }
}

#[tokio::test]
async fn malformed_line_is_skipped_but_connection_survives() {
    let dir = tempdir().expect("tempdir");
    let (listener, mut rx) = JournalListener::start(dir.path()).expect("start");
    let mut client = UnixStream::connect(listener.socket_path())
        .await
        .expect("connect");
    client.write_all(b"not json\n").await.expect("write");
    client
        .write_all(b"{\"type\":\"hook\",\"timestamp\":\"2026-05-20T10:00:00+00:00\"}\n")
        .await
        .expect("write");
    client.shutdown().await.ok();

    let env = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("envelope");
    assert_eq!(env.event_type, EventType::Hook);
}

/// Stub dispatcher that echoes the line back as the result. Verifies
/// the listener's MCP fast-path routes JSON-RPC frames correctly and
/// writes responses back over the same connection.
struct EchoDispatcher;
impl McpDispatcher for EchoDispatcher {
    fn dispatch(&self, line: &str, _peer_job_id: Option<JobId>) -> McpReply {
        McpReply::plain(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "echoed": line },
            })
            .to_string(),
        )
    }
}

/// Dispatcher that captures the `peer_job_id` it receives. Used to
/// verify that the listener's per-connection state is propagated
/// correctly across multiple frames on the same connection (Gap #1
/// closure).
struct PeerIdRecordingDispatcher {
    observed: std::sync::Arc<std::sync::Mutex<Vec<Option<JobId>>>>,
}
impl McpDispatcher for PeerIdRecordingDispatcher {
    fn dispatch(&self, _line: &str, peer_job_id: Option<JobId>) -> McpReply {
        if let Ok(mut g) = self.observed.lock() {
            g.push(peer_job_id);
        }
        McpReply::plain(serde_json::json!({"jsonrpc":"2.0","id":1,"result":"ok"}).to_string())
    }
}

#[tokio::test]
async fn mcp_jsonrpc_frame_is_dispatched_and_response_written_back() {
    use tokio::io::AsyncBufReadExt;
    let dir = tempdir().expect("tempdir");
    let handlers = LiveJournalHandlers {
        mcp: Some(std::sync::Arc::new(EchoDispatcher)),
        ..LiveJournalHandlers::default()
    };
    let (listener, _rx) =
        JournalListener::start_with_handlers(dir.path(), handlers).expect("start");
    let mut client = UnixStream::connect(listener.socket_path())
        .await
        .expect("connect");
    let req = r#"{"jsonrpc":"2.0","id":1,"method":"orkia_rfc_state","params":{"rfc_id":"x"}}"#;
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write");

    let (read_half, _write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .expect("timeout")
        .expect("read");
    assert!(line.contains("\"result\""));
    assert!(line.contains("echoed"));
}

#[tokio::test]
async fn init_handshake_propagates_peer_job_id_to_subsequent_calls() {
    use tokio::io::AsyncBufReadExt;
    let dir = tempdir().expect("tempdir");
    let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Option<JobId>>::new()));
    let dispatcher = PeerIdRecordingDispatcher {
        observed: observed.clone(),
    };
    let handlers = LiveJournalHandlers {
        mcp: Some(std::sync::Arc::new(dispatcher)),
        ..LiveJournalHandlers::default()
    };
    let (listener, _rx) =
        JournalListener::start_with_handlers(dir.path(), handlers).expect("start");
    let mut client = UnixStream::connect(listener.socket_path())
        .await
        .expect("connect");

    // Frame 1: init handshake. Should be intercepted (never reaches the
    // dispatcher) and acked with `{"result":{"ok":true}}`.
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"orkia_rfc_init","params":{"job_id":42}}"#;
    client
        .write_all(format!("{init}\n").as_bytes())
        .await
        .expect("write init");

    // Read the init ack so we know the handshake has been processed
    // before we send frame 2 (avoids a race where frame 2 lands
    // before peer_job_id is set on the connection).
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .expect("timeout")
        .expect("read init ack");
    assert!(line.contains("\"ok\":true"), "init ack: {line}");

    // Frame 2: any RFC method. Dispatcher should receive Some(JobId(42)).
    let ask = r#"{"jsonrpc":"2.0","id":2,"method":"orkia_rfc_state","params":{"rfc_id":"x"}}"#;
    write_half
        .write_all(format!("{ask}\n").as_bytes())
        .await
        .expect("write ask");
    line.clear();
    timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .expect("timeout")
        .expect("read ask response");

    let captured = observed.lock().expect("lock").clone();
    assert_eq!(
        captured,
        vec![Some(JobId(42))],
        "dispatcher must have observed the init-supplied job_id"
    );
}

#[tokio::test]
async fn init_without_handshake_leaves_peer_id_as_none() {
    use tokio::io::AsyncBufReadExt;
    let dir = tempdir().expect("tempdir");
    let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Option<JobId>>::new()));
    let dispatcher = PeerIdRecordingDispatcher {
        observed: observed.clone(),
    };
    let handlers = LiveJournalHandlers {
        mcp: Some(std::sync::Arc::new(dispatcher)),
        ..LiveJournalHandlers::default()
    };
    let (listener, _rx) =
        JournalListener::start_with_handlers(dir.path(), handlers).expect("start");
    let mut client = UnixStream::connect(listener.socket_path())
        .await
        .expect("connect");
    let req = r#"{"jsonrpc":"2.0","id":1,"method":"orkia_rfc_state","params":{"rfc_id":"x"}}"#;
    client
        .write_all(format!("{req}\n").as_bytes())
        .await
        .expect("write");
    let (read_half, _w) = client.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .expect("timeout")
        .expect("read");
    let captured = observed.lock().expect("lock").clone();
    assert_eq!(captured, vec![None], "no init → no peer_job_id");
}

#[tokio::test]
async fn journal_path_still_works_when_mcp_dispatcher_is_set() {
    let dir = tempdir().expect("tempdir");
    let handlers = LiveJournalHandlers {
        mcp: Some(std::sync::Arc::new(EchoDispatcher)),
        ..LiveJournalHandlers::default()
    };
    let (listener, mut rx) =
        JournalListener::start_with_handlers(dir.path(), handlers).expect("start");
    let mut client = UnixStream::connect(listener.socket_path())
        .await
        .expect("connect");
    let line = r#"{"type":"hook","timestamp":"2026-05-20T10:00:00+00:00","job_id":1,"event":"PreToolUse"}"#;
    client
        .write_all(format!("{line}\n").as_bytes())
        .await
        .expect("write");
    client.shutdown().await.ok();

    let env = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("envelope");
    assert_eq!(env.event_type, EventType::Hook);
}

#[tokio::test]
async fn sender_clones_for_inprocess_emit() {
    let dir = tempdir().expect("tempdir");
    let (listener, mut rx) = JournalListener::start(dir.path()).expect("start");
    let sender = listener.sender();
    let mut env = JournalEnvelope::now(EventType::Shell);
    env.action = Some("ls".into());
    sender.send(env).expect("send");

    let got = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("envelope");
    assert_eq!(got.event_type, EventType::Shell);
    assert_eq!(got.action.as_deref(), Some("ls"));
}

#[tokio::test]
async fn drop_removes_socket_file() {
    let dir = tempdir().expect("tempdir");
    let path;
    {
        let (listener, _rx) = JournalListener::start(dir.path()).expect("start");
        path = listener.socket_path().to_path_buf();
        assert!(path.exists());
    }
    assert!(!path.exists(), "socket should be removed on drop");
}

#[tokio::test]
async fn stale_socket_is_cleaned_up_on_bind() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("run")).expect("mkdir");
    let stale = dir.path().join("run").join("orkia.sock");
    std::fs::write(&stale, b"").expect("write stale");
    assert!(stale.exists());

    let (listener, _rx) = JournalListener::start(dir.path()).expect("start");
    assert!(listener.socket_path().exists());
}

#[tokio::test]
async fn seeded_hub_stamps_monotonic_hub_seq_from_seed() {
    // The daemon hub seeds the counter from the on-disk max; the next stamp is
    // seed + 1, strictly increasing per ingress envelope (socket OR in-process).
    let dir = tempdir().expect("tempdir");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let listener = JournalListener::start_with_channel_seeded(
        dir.path(),
        LiveJournalHandlers::default(),
        tx,
        Some(10),
    )
    .expect("start seeded");

    let emit = listener.sender();
    for _ in 0..3 {
        emit.send(JournalEnvelope::now(EventType::Hook))
            .expect("emit");
    }

    let mut seqs = Vec::new();
    for _ in 0..3 {
        let env = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("envelope");
        seqs.push(env.hub_seq);
    }
    assert_eq!(seqs, vec![Some(11), Some(12), Some(13)]);
}

#[tokio::test]
async fn unseeded_hub_leaves_hub_seq_none() {
    // Relay / per-job LPH / daemon-less fallback do not stamp: single-process,
    // no resubscribe gap to close.
    let dir = tempdir().expect("tempdir");
    let (listener, mut rx) = JournalListener::start(dir.path()).expect("start");
    listener
        .sender()
        .send(JournalEnvelope::now(EventType::Hook))
        .expect("emit");

    let env = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("envelope");
    assert_eq!(env.hub_seq, None);
}
