//! Integration tests for the MCP pipe server. Split out of `lib.rs` to
//! keep the module under the 600-line limit; all exercised surface
//! (`Server`, `ServerEnv`, `MAX_PIPELINE_OUTPUT_BYTES`) is public.

use orkia_mcp_pipe_server::{MAX_PIPELINE_OUTPUT_BYTES, Server, ServerEnv};

fn env(tmp: &std::path::Path, socket: &std::path::Path) -> ServerEnv {
    ServerEnv {
        pipeline_id: "pipe-test-1".into(),
        stage_index: 0,
        job_id: 42,
        agent_name: "test-agent".into(),
        run_dir: tmp.to_path_buf(),
        socket_path_override: Some(socket.to_path_buf()),
    }
}

/// Spawn a fake journal listener at `socket` that accepts ONE
/// connection, reads it to EOF, and returns the lines via a
/// oneshot channel. The bridge `send_envelope` opens a connection,
/// writes one NDJSON line, then shuts the stream down — so reading
/// to EOF deterministically yields exactly that one line.
fn fake_journal_once(socket: std::path::PathBuf) -> tokio::sync::oneshot::Receiver<Vec<String>> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixListener;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        let mut received = Vec::new();
        if let Ok((stream, _)) = listener.accept().await {
            let mut br = BufReader::new(stream);
            let mut buf = String::new();
            while br.read_line(&mut buf).await.unwrap_or(0) > 0 {
                received.push(buf.trim().to_string());
                buf.clear();
            }
        }
        let _ = tx.send(received);
    });
    rx
}

#[tokio::test]
async fn initialize_returns_protocol_version() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("orkia.sock");
    let server = Server::new(env(tmp.path(), &socket));
    let out = server
        .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await
        .unwrap();
    assert!(out.contains("\"protocolVersion\""));
    assert!(out.contains("orkia-mcp-pipe-server"));
}

#[tokio::test]
async fn tools_list_advertises_submit() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("orkia.sock");
    let server = Server::new(env(tmp.path(), &socket));
    let out = server
        .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
        .await
        .unwrap();
    assert!(out.contains("submit_pipeline_output"));
    assert!(out.contains("\"content\""));
    assert!(out.contains("required"));
}

#[tokio::test]
async fn submit_writes_file_and_emits_envelope() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("orkia.sock");
    let journal_rx = fake_journal_once(socket.clone());
    let server = Server::new(env(tmp.path(), &socket));

    let req = serde_json::json!({
        "jsonrpc":"2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "submit_pipeline_output",
            "arguments": { "content": "the-deliverable" }
        }
    });
    let out = server.handle_line(&req.to_string()).await.unwrap();
    assert!(out.contains("\"isError\":false"), "got: {out}");

    // File written?
    let written = std::fs::read_to_string(tmp.path().join("pipeline-output.md")).unwrap();
    assert_eq!(written, "the-deliverable");

    // Wait for the journal listener to receive + drain the line.
    // `send_envelope` shuts the stream down after writing, so the
    // listener's read loop ends and the oneshot fires.
    let received = tokio::time::timeout(std::time::Duration::from_secs(2), journal_rx)
        .await
        .expect("journal listener timed out")
        .expect("journal oneshot dropped");
    assert!(
        received.iter().any(|l| l.contains("\"PipelineOutput\"")),
        "expected PipelineOutput envelope in: {received:?}"
    );
    assert!(
        received
            .iter()
            .any(|l| l.contains("\"pipeline_id\":\"pipe-test-1\"")),
        "expected pipeline_id in envelope: {received:?}"
    );
}

#[tokio::test]
async fn submit_twice_returns_already_submitted() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("orkia.sock");
    let _journal_rx = fake_journal_once(socket.clone());
    let server = Server::new(env(tmp.path(), &socket));

    let req = serde_json::json!({
        "jsonrpc":"2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "submit_pipeline_output",
            "arguments": { "content": "first" }
        }
    });
    let first = server.handle_line(&req.to_string()).await.unwrap();
    assert!(first.contains("\"isError\":false"));

    let req2 = serde_json::json!({
        "jsonrpc":"2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "submit_pipeline_output",
            "arguments": { "content": "second" }
        }
    });
    let second = server.handle_line(&req2.to_string()).await.unwrap();
    assert!(second.contains("already submitted"), "got: {second}");
    // First write wins — the file still contains "first".
    let written = std::fs::read_to_string(tmp.path().join("pipeline-output.md")).unwrap();
    assert_eq!(written, "first");
}

#[tokio::test]
async fn oversized_submit_is_rejected_at_frame_boundary() {
    // In this workspace MCP_FRAME_MAX_BYTES (256 KiB) is far below the
    // 8 MiB payload cap, so an over-large submit is rejected at the
    // trust-boundary frame check (-32700) before the payload-size branch
    // is reached. That frame guard is the operative protection here; no
    // file must be written for a rejected frame.
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("orkia.sock");
    let server = Server::new(env(tmp.path(), &socket));

    let huge = "x".repeat(MAX_PIPELINE_OUTPUT_BYTES + 1);
    let req = serde_json::json!({
        "jsonrpc":"2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "submit_pipeline_output",
            "arguments": { "content": huge }
        }
    });
    let out = server.handle_line(&req.to_string()).await.unwrap();
    assert!(
        out.contains("-32700"),
        "expected frame rejection, got: {out}"
    );
    assert!(!tmp.path().join("pipeline-output.md").exists());
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("orkia.sock");
    let server = Server::new(env(tmp.path(), &socket));
    let out = server
        .handle_line(r#"{"jsonrpc":"2.0","id":9,"method":"nope"}"#)
        .await
        .unwrap();
    assert!(out.contains("-32601"));
}

#[tokio::test]
async fn notification_returns_no_response() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("orkia.sock");
    let server = Server::new(env(tmp.path(), &socket));
    // No `id` field → notification → no response.
    let out = server
        .handle_line(r#"{"jsonrpc":"2.0","method":"initialize"}"#)
        .await;
    assert!(out.is_none(), "notifications must not be replied to");
}

#[tokio::test]
async fn missing_content_argument_returns_invalid_params() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("orkia.sock");
    let server = Server::new(env(tmp.path(), &socket));
    let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"submit_pipeline_output","arguments":{}}}"#;
    let out = server.handle_line(req).await.unwrap();
    assert!(out.contains("-32602"), "got: {out}");
}

#[tokio::test]
async fn malformed_json_returns_parse_error() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("orkia.sock");
    let server = Server::new(env(tmp.path(), &socket));
    let out = server.handle_line("{not json").await.unwrap();
    assert!(out.contains("-32700"));
}
