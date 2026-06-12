// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! MCP stdio server exposing `submit_pipeline_output` for Team agent
//!
//! Spawned by the Team coordinator as a subprocess per pipeline stage.
//! The agent's `mcp-config.json` lists this binary so the agent can call
//! the tool from within its TUI session. The tool delivers the
//! structured hand-off to the next stage; on success the server writes
//! `<run-dir>/pipeline-output.md` and emits a `PipelineOutput` envelope
//! to the journal socket, then returns success to the MCP caller.
//!
//! Protocol: JSON-RPC 2.0 over newline-delimited stdio. Requests on
//! stdin, responses on stdout, one JSON object per line. The minimal
//! MCP surface implemented:
//!
//! - `initialize` — handshake.
//! - `tools/list` — advertises `submit_pipeline_output`.
//! - `tools/call` — invokes the tool.
//!
//! Other methods return `-32601 method not found`. Notifications
//! (no `id` field) are accepted and ignored where unrecognised.
//!
//! First-write-wins lock: a second call to `submit_pipeline_output`
//! within the same process returns an error to the MCP caller and
//! emits no journal event. The coordinator drives the next stage off
//! the first event.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use orkia_shell_types::{EventType, JournalEnvelope};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Matches the FINAL-RESPONSE storage cap so a tool call's payload can
/// never overflow the next stage's `final-response.md` budget.
pub const MAX_PIPELINE_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Required env vars set by the Team coordinator at spawn time. The
/// MCP server refuses to start if any are missing — the agent must
/// never see a half-configured pipe-server that pretends to work.
pub struct ServerEnv {
    pub pipeline_id: String,
    pub stage_index: u32,
    pub job_id: u32,
    pub agent_name: String,
    pub run_dir: PathBuf,
    /// Override for `~/.orkia/run/orkia.sock`. Set in tests; production
    /// resolves to the default path when `None`.
    pub socket_path_override: Option<PathBuf>,
}

impl ServerEnv {
    /// Read from process env. Returns descriptive errors so a
    /// misconfigured spawn fails loudly.
    pub fn from_env() -> Result<Self, String> {
        let pipeline_id = std::env::var("ORKIA_PIPELINE_ID")
            .map_err(|_| "ORKIA_PIPELINE_ID is required".to_string())?;
        let stage_index = std::env::var("ORKIA_STAGE_INDEX")
            .map_err(|_| "ORKIA_STAGE_INDEX is required".to_string())?
            .parse::<u32>()
            .map_err(|e| format!("ORKIA_STAGE_INDEX parse: {e}"))?;
        let job_id = std::env::var("ORKIA_JOB_ID")
            .map_err(|_| "ORKIA_JOB_ID is required".to_string())?
            .parse::<u32>()
            .map_err(|e| format!("ORKIA_JOB_ID parse: {e}"))?;
        let agent_name = std::env::var("ORKIA_AGENT_NAME")
            .map_err(|_| "ORKIA_AGENT_NAME is required".to_string())?;
        let run_dir = std::env::var_os("ORKIA_RUN_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| "ORKIA_RUN_DIR is required".to_string())?;
        let socket_path_override = std::env::var_os("ORKIA_SOCKET_PATH").map(PathBuf::from);
        Ok(Self {
            pipeline_id,
            stage_index,
            job_id,
            agent_name,
            run_dir,
            socket_path_override,
        })
    }

    /// Resolve the journal socket path — uses the override (tests) or
    /// the canonical `~/.orkia/run/orkia.sock` (production).
    pub fn socket_path(&self) -> PathBuf {
        if let Some(p) = &self.socket_path_override {
            return p.clone();
        }
        // When `HOME` is unset (sandbox/container/CI), `unwrap_or_default()`
        // used to yield a silent *relative* `.orkia/run/orkia.sock` whose
        // connection failed with an opaque error. Fall back to an absolute
        // temp-dir path so the socket is always well-defined (BUG-075).
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(std::env::temp_dir);
        base.join(".orkia").join("run").join("orkia.sock")
    }
}

/// JSON-RPC 2.0 request — the subset MCP uses.
#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: Option<serde_json::Value>, code: i32, msg: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: msg.into(),
                data: None,
            }),
        }
    }
}

pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
/// Orkia-specific: `submit_pipeline_output` called twice for the same
/// stage. The agent gets a clear "already submitted" error.
pub const ALREADY_SUBMITTED: i32 = -32010;
/// Orkia-specific: content exceeded `MAX_PIPELINE_OUTPUT_BYTES`.
pub const PAYLOAD_TOO_LARGE: i32 = -32011;

/// State carried across MCP calls in this server instance. Exposed so
/// tests can drive it deterministically without spinning up stdio.
pub struct Server {
    env: ServerEnv,
    submitted: AtomicBool,
}

impl Server {
    pub fn new(env: ServerEnv) -> Self {
        Self {
            env,
            submitted: AtomicBool::new(false),
        }
    }

    pub fn env(&self) -> &ServerEnv {
        &self.env
    }

    /// Dispatch one JSON-RPC line and return the response line to write
    /// back (with trailing `\n` already attached). Notification requests
    /// (no `id`) that don't require a reply return `None`.
    pub async fn handle_line(&self, line: &str) -> Option<String> {
        // Trust-boundary input cap: the MCP peer is an external agent
        // process. Without a cap a misbehaving agent could OOM us by
        // emitting a single multi-MiB line. The cap matches the
        // canonical MCP-frame budget shared with the RFC pipe server.
        if let Err(e) = orkia_shell_types::input_limits::check_len(
            line.as_bytes(),
            orkia_shell_types::input_limits::MCP_FRAME_MAX_BYTES,
            "mcp-pipe-server",
        ) {
            let resp = Response::err(None, -32700, format!("input rejected: {e}"));
            return Some(serialize(&resp));
        }
        let req: Request = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                // Parse failure — return a JSON-RPC parse error with
                // null id (no id was recoverable from the input).
                let resp = Response::err(None, -32700, format!("parse error: {e}"));
                return Some(serialize(&resp));
            }
        };

        let is_notification = req.id.is_none();
        let resp = self.dispatch(req).await;
        // Notifications must not get a response.
        if is_notification {
            None
        } else {
            Some(serialize(&resp))
        }
    }

    async fn dispatch(&self, req: Request) -> Response {
        match req.method.as_str() {
            "initialize" => self.handle_initialize(req.id),
            "tools/list" => self.handle_tools_list(req.id),
            "tools/call" => self.handle_tools_call(req.id, req.params).await,
            // MCP shutdown is optional; we just acknowledge.
            "shutdown" | "ping" => Response::ok(req.id, serde_json::json!({})),
            _ => Response::err(
                req.id,
                METHOD_NOT_FOUND,
                format!("unknown method: {}", req.method),
            ),
        }
    }

    fn handle_initialize(&self, id: Option<serde_json::Value>) -> Response {
        Response::ok(
            id,
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "orkia-mcp-pipe-server",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "tools": { "listChanged": false }
                }
            }),
        )
    }

    fn handle_tools_list(&self, id: Option<serde_json::Value>) -> Response {
        Response::ok(
            id,
            serde_json::json!({
                "tools": [{
                    "name": "submit_pipeline_output",
                    "description": "Submit your deliverable for the next pipeline stage. Call this once when your work for the downstream agent is ready. After calling this, you may continue talking to the user normally — the call is the structured hand-off, separate from your chat output.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "The text to deliver to the next stage. This will be the input the next agent reads, prepended only by its own instruction body."
                            }
                        },
                        "required": ["content"]
                    }
                }]
            }),
        )
    }

    async fn handle_tools_call(
        &self,
        id: Option<serde_json::Value>,
        params: serde_json::Value,
    ) -> Response {
        let name = params.get("name").and_then(|v| v.as_str());
        if name != Some("submit_pipeline_output") {
            return Response::err(
                id,
                METHOD_NOT_FOUND,
                format!("unknown tool: {}", name.unwrap_or("<none>")),
            );
        }
        let content = match params
            .get("arguments")
            .and_then(|a| a.get("content"))
            .and_then(|v| v.as_str())
        {
            Some(s) => s,
            None => {
                return Response::err(id, INVALID_PARAMS, "missing string argument `content`");
            }
        };
        if content.len() > MAX_PIPELINE_OUTPUT_BYTES {
            return Response::err(
                id,
                PAYLOAD_TOO_LARGE,
                format!(
                    "content {} bytes exceeds cap of {} bytes; summarize before resubmitting",
                    content.len(),
                    MAX_PIPELINE_OUTPUT_BYTES
                ),
            );
        }
        // First-write-wins. Atomic CAS — if `submitted` was already
        // true, this returns Err and we reject. Otherwise we hold the
        // exclusive right to deliver this stage's output.
        if self
            .submitted
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Response::err(
                id,
                ALREADY_SUBMITTED,
                "pipeline output already submitted for this stage",
            );
        }
        match self.deliver(content).await {
            Ok(meta) => Response::ok(
                id,
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": format!(
                            "ok: pipeline-output.md written ({} bytes, sha {})",
                            meta.bytes, meta.sha
                        )
                    }],
                    "isError": false
                }),
            ),
            Err(e) => {
                // Roll back the lock so the agent can retry. Errors
                // here are filesystem / socket issues — not "agent
                // misbehaved".
                self.submitted.store(false, Ordering::SeqCst);
                Response::err(id, INTERNAL_ERROR, format!("deliver failed: {e}"))
            }
        }
    }

    async fn deliver(&self, content: &str) -> Result<DeliverMeta, String> {
        let path = self.env.run_dir.join("pipeline-output.md");
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        tokio::fs::write(&path, content.as_bytes())
            .await
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        let sha = short_sha(content.as_bytes());
        let preview = preview_280(content);
        let envelope = JournalEnvelope {
            event_type: EventType::Hook,
            timestamp: chrono::Utc::now().to_rfc3339(),
            job_id: Some(self.env.job_id),
            session_id: None,
            source: Some("orkia-pipe".into()),
            agent: Some(self.env.agent_name.clone()),
            event: Some("PipelineOutput".into()),
            response_path: Some(path.to_string_lossy().into_owned()),
            response_sha256: Some(sha.clone()),
            response_bytes: Some(content.len() as u64),
            response_preview: Some(preview),
            pipeline_id: Some(self.env.pipeline_id.clone()),
            stage_index: Some(self.env.stage_index),
            ..Default::default()
        };
        send_envelope(&self.env.socket_path(), &envelope)
            .await
            .map_err(|e| format!("journal send: {e}"))?;
        Ok(DeliverMeta {
            bytes: content.len() as u64,
            sha,
        })
    }
}

#[derive(Debug, Clone)]
pub struct DeliverMeta {
    pub bytes: u64,
    pub sha: String,
}

/// SHA-256 first 16 hex chars — matches the convention in
/// `orkia-final-response::short_sha`. Manual hex encoding to stay
/// portable across `sha2` 0.10 vs 0.11 (the workspaces use different
/// versions and the `LowerHex` impl on the digest moved).
pub fn short_sha(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// First 280 characters of `content`, char-boundary-safe.
pub fn preview_280(content: &str) -> String {
    content.chars().take(280).collect()
}

fn serialize(resp: &Response) -> String {
    // Serialisation cannot fail for our shapes — but defensively, fall
    // back to a minimal error response if it does.
    match serde_json::to_string(resp) {
        Ok(mut s) => {
            s.push('\n');
            s
        }
        Err(_) => {
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"serialize failed\"}}\n".into()
        }
    }
}

async fn send_envelope(
    socket_path: &std::path::Path,
    envelope: &JournalEnvelope,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;
    use tokio::time::{Duration, timeout};
    let mut stream =
        timeout(Duration::from_millis(500), UnixStream::connect(socket_path)).await??;
    let mut line = serde_json::to_string(envelope)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    timeout(
        Duration::from_millis(500),
        stream.write_all(line.as_bytes()),
    )
    .await??;
    timeout(Duration::from_millis(500), stream.shutdown()).await??;
    Ok(())
}

/// Run the stdio loop until EOF. Used by the `orkia mcp-pipe` subcommand.
pub async fn run_stdio(server: Server) -> std::io::Result<()> {
    use orkia_shell_types::input_limits::MCP_FRAME_MAX_BYTES;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();
    loop {
        line.clear();
        // Bound the read itself at the trust boundary — without
        // `.take(...)` a malicious agent could OOM the process by
        // writing one multi-GiB line before any parser ever sees it.
        let n = (&mut reader)
            .take(MCP_FRAME_MAX_BYTES as u64 + 1)
            .read_line(&mut line)
            .await?;
        if n == 0 {
            return Ok(()); // EOF
        }
        if line.len() > MCP_FRAME_MAX_BYTES {
            // Frame too long. We can't recover the JSON-RPC id from a
            // truncated request, so drain the rest of the line and
            // continue. (Without the drain the next iteration would
            // read the tail as a "new" request, compounding the bug.)
            while !line.ends_with('\n') {
                line.clear();
                let drained = (&mut reader)
                    .take(MCP_FRAME_MAX_BYTES as u64)
                    .read_line(&mut line)
                    .await?;
                if drained == 0 || line.ends_with('\n') {
                    break;
                }
            }
            tracing::warn!(
                cap = MCP_FRAME_MAX_BYTES,
                "mcp-pipe-server: dropped over-cap stdin frame",
            );
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(resp) = server.handle_line(trimmed).await {
            stdout.write_all(resp.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
}
