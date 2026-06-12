// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia mcp-bridge`: stdio MCP bridge for Orkia socket tools.
//!
//! The journal socket speaks Orkia's internal line-delimited JSON-RPC
//! protocol. CLI agents speak standard MCP (`initialize`, `tools/list`,
//! `tools/call`). This bridge translates public Orkia tools into socket
//! methods without giving the listener any premium-gate state.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

pub(crate) struct BridgeEnv {
    job_id: u32,
    socket_path: PathBuf,
}

impl BridgeEnv {
    pub(crate) fn from_env() -> Result<Self, String> {
        let job_id = std::env::var("ORKIA_JOB_ID")
            .map_err(|_| "ORKIA_JOB_ID is required".to_string())?
            .parse::<u32>()
            .map_err(|e| format!("ORKIA_JOB_ID parse: {e}"))?;
        let socket_path = std::env::var_os("ORKIA_SOCKET_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(default_socket_path);
        Ok(Self {
            job_id,
            socket_path,
        })
    }
}

fn default_socket_path() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(std::env::temp_dir);
    base.join(".orkia").join("run").join("orkia.sock")
}

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

impl Response {
    fn ok(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Option<serde_json::Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

pub(crate) struct Server {
    env: BridgeEnv,
}

impl Server {
    pub(crate) fn new(env: BridgeEnv) -> Self {
        Self { env }
    }

    async fn handle_line(&self, line: &str) -> Option<String> {
        if let Err(e) = orkia_shell_types::input_limits::check_len(
            line.as_bytes(),
            orkia_shell_types::input_limits::MCP_FRAME_MAX_BYTES,
            "mcp-bridge",
        ) {
            return Some(serialize(&Response::err(
                None,
                -32700,
                format!("input rejected: {e}"),
            )));
        }
        let req: Request = match serde_json::from_str(line) {
            Ok(req) => req,
            Err(e) => {
                return Some(serialize(&Response::err(
                    None,
                    -32700,
                    format!("parse error: {e}"),
                )));
            }
        };
        let is_notification = req.id.is_none();
        let response = self.dispatch(req).await;
        (!is_notification).then(|| serialize(&response))
    }

    async fn dispatch(&self, req: Request) -> Response {
        match req.method.as_str() {
            "initialize" => self.initialize(req.id),
            "tools/list" => Response::ok(req.id, tools_list()),
            "tools/call" => self.tools_call(req.id, req.params).await,
            "shutdown" | "ping" => Response::ok(req.id, serde_json::json!({})),
            other => Response::err(req.id, METHOD_NOT_FOUND, format!("unknown method: {other}")),
        }
    }

    fn initialize(&self, id: Option<serde_json::Value>) -> Response {
        Response::ok(
            id,
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "orkia-mcp-bridge",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "tools": { "listChanged": false }
                }
            }),
        )
    }

    async fn tools_call(
        &self,
        id: Option<serde_json::Value>,
        params: serde_json::Value,
    ) -> Response {
        let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
            return Response::err(id, INVALID_PARAMS, "missing string params.name");
        };
        if !is_known_tool(name) {
            return Response::err(id, METHOD_NOT_FOUND, format!("unknown tool: {name}"));
        }
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        match self.call_socket(name, arguments).await {
            Ok(result) => Response::ok(id, mcp_tool_result(result, false)),
            Err(e) => Response::err(id, INTERNAL_ERROR, e),
        }
    }

    async fn call_socket(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        use tokio::io::BufReader;
        use tokio::net::UnixStream;
        use tokio::time::{Duration, timeout};

        let stream = timeout(
            Duration::from_millis(500),
            UnixStream::connect(&self.env.socket_path),
        )
        .await
        .map_err(|_| "connect to orkia socket timed out".to_string())?
        .map_err(|e| format!("connect {}: {e}", self.env.socket_path.display()))?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "init",
            "method": "orkia_rfc_init",
            "params": { "job_id": self.env.job_id },
        });
        write_json_line(&mut write_half, &init).await?;
        let _ = read_json_line(&mut reader).await?;

        let call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "call",
            "method": method,
            "params": params,
        });
        write_json_line(&mut write_half, &call).await?;
        let response = read_json_line(&mut reader).await?;
        if let Some(error) = response.get("error") {
            return Err(error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("socket JSON-RPC error")
                .to_string());
        }
        Ok(response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }
}

async fn write_json_line(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    value: &serde_json::Value,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut line = serde_json::to_string(value).map_err(|e| format!("serialize: {e}"))?;
    line.push('\n');
    write_half
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("socket write: {e}"))?;
    write_half
        .flush()
        .await
        .map_err(|e| format!("socket flush: {e}"))
}

async fn read_json_line(
    reader: &mut tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<serde_json::Value, String> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    use tokio::time::{Duration, timeout};

    let mut line = String::new();
    let read = timeout(
        Duration::from_millis(500),
        reader
            .take(orkia_shell_types::input_limits::MCP_FRAME_MAX_BYTES as u64 + 1)
            .read_line(&mut line),
    )
    .await
    .map_err(|_| "socket read timed out".to_string())?
    .map_err(|e| format!("socket read: {e}"))?;
    if read == 0 {
        return Err("socket closed before response".into());
    }
    if line.len() > orkia_shell_types::input_limits::MCP_FRAME_MAX_BYTES {
        return Err("socket response exceeded MCP frame cap".into());
    }
    serde_json::from_str(line.trim()).map_err(|e| format!("socket response parse: {e}"))
}

fn tools_list() -> serde_json::Value {
    serde_json::json!({
        "tools": [
            {
                "name": "recall",
                "description": "Recall relevant Orkia Knowledge Graph context for a topic before starting work or making a decision.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "project": { "type": "string" },
                        "domain": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 10 }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "knowledge_search",
                "description": "Search Knowledge Graph node summaries.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "node_type": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 20 }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "knowledge_node",
                "description": "Read one Knowledge Graph node by id.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "orkia_rfc_get_context",
                "description": "Read the current context for an Orkia RFC.",
                "inputSchema": rfc_schema()
            },
            {
                "name": "orkia_rfc_state",
                "description": "Read the current Orkia RFC state.",
                "inputSchema": rfc_schema()
            },
            {
                "name": "orkia_rfc_list_decisions",
                "description": "List decisions recorded on an Orkia RFC.",
                "inputSchema": rfc_schema()
            },
            {
                "name": "orkia_rfc_ask",
                "description": "Ask the human to resolve an RFC clarification.",
                "inputSchema": rfc_agent_schema()
            },
            {
                "name": "orkia_rfc_log_decision",
                "description": "Log a decision against an Orkia RFC.",
                "inputSchema": rfc_agent_schema()
            },
            {
                "name": "orkia_rfc_propose_edit",
                "description": "Propose an edit to an Orkia RFC.",
                "inputSchema": rfc_agent_schema()
            },
            {
                "name": "orkia_rfc_propose_promote",
                "description": "Propose promoting an Orkia RFC to the next state.",
                "inputSchema": rfc_agent_schema()
            }
        ]
    })
}

fn is_known_tool(name: &str) -> bool {
    matches!(
        name,
        "recall"
            | "knowledge_search"
            | "knowledge_node"
            | "orkia_rfc_get_context"
            | "orkia_rfc_state"
            | "orkia_rfc_list_decisions"
            | "orkia_rfc_ask"
            | "orkia_rfc_log_decision"
            | "orkia_rfc_propose_edit"
            | "orkia_rfc_propose_promote"
    )
}

fn rfc_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "rfc_id": { "type": "string" }
        },
        "required": ["rfc_id"]
    })
}

fn rfc_agent_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "rfc_id": { "type": "string" },
            "agent": { "type": "string" },
            "question": { "type": "string" },
            "rationale": { "type": "string" },
            "decision": { "type": "string" },
            "section": { "type": "string" },
            "new_body": { "type": "string" }
        },
        "required": ["rfc_id"]
    })
}

fn mcp_tool_result(result: serde_json::Value, is_error: bool) -> serde_json::Value {
    let text = match result {
        serde_json::Value::String(s) => s,
        other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| "null".into()),
    };
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn serialize(resp: &Response) -> String {
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

pub(crate) async fn run() -> i32 {
    let env = match BridgeEnv::from_env() {
        Ok(env) => env,
        Err(e) => {
            eprintln!("orkia mcp-bridge: {e}");
            return 2;
        }
    };
    match run_stdio(Server::new(env)).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("orkia mcp-bridge: {e}");
            1
        }
    }
}

async fn run_stdio(server: Server) -> std::io::Result<()> {
    use orkia_shell_types::input_limits::MCP_FRAME_MAX_BYTES;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut reader)
            .take(MCP_FRAME_MAX_BYTES as u64 + 1)
            .read_line(&mut line)
            .await?;
        if n == 0 {
            return Ok(());
        }
        if line.len() > MCP_FRAME_MAX_BYTES {
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
                "mcp-bridge: dropped over-cap frame"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn env(socket: PathBuf) -> BridgeEnv {
        BridgeEnv {
            job_id: 42,
            socket_path: socket,
        }
    }

    #[tokio::test]
    async fn tools_list_advertises_knowledge_tools() {
        let server = Server::new(env(PathBuf::from("/tmp/nope")));
        let out = server
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .await
            .expect("response");
        assert!(out.contains("\"recall\""));
        assert!(out.contains("\"knowledge_search\""));
        assert!(out.contains("\"knowledge_node\""));
        assert!(out.contains("\"orkia_rfc_state\""));
        assert!(out.contains("\"orkia_rfc_propose_promote\""));
    }

    #[tokio::test]
    async fn recall_forwards_init_then_call_to_socket() {
        let (out, seen) = call_fake_socket_tool(
            "recall",
            serde_json::json!({ "query": "auth" }),
            b"{\"jsonrpc\":\"2.0\",\"id\":\"call\",\"result\":{\"results\":[{\"context_block\":\"ctx\"}]}}\n",
        )
        .await;
        assert!(out.contains("\"isError\":false"), "got {out}");
        assert!(out.contains("context_block"), "got {out}");

        assert_eq!(seen.len(), 2);
        assert!(seen[0].contains("\"orkia_rfc_init\""));
        assert!(seen[0].contains("\"job_id\":42"));
        assert!(seen[1].contains("\"method\":\"recall\""));
        assert!(seen[1].contains("\"query\":\"auth\""));
    }

    #[tokio::test]
    async fn rfc_tool_forwards_to_socket() {
        let (_, seen) = call_fake_socket_tool(
            "orkia_rfc_state",
            serde_json::json!({ "rfc_id": "rfc-1" }),
            b"{\"jsonrpc\":\"2.0\",\"id\":\"call\",\"result\":{\"state\":\"draft\"}}\n",
        )
        .await;
        assert!(seen[1].contains("\"method\":\"orkia_rfc_state\""));
        assert!(seen[1].contains("\"rfc_id\":\"rfc-1\""));
    }

    async fn call_fake_socket_tool(
        tool: &str,
        arguments: serde_json::Value,
        response: &'static [u8],
    ) -> (String, Vec<String>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().expect("tmp");
        let socket = tmp.path().join("orkia.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut seen = Vec::new();
            if let Ok((stream, _)) = listener.accept().await {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    seen.push(line.trim().to_string());
                    let _ = write_half
                        .write_all(
                            b"{\"jsonrpc\":\"2.0\",\"id\":\"init\",\"result\":{\"ok\":true}}\n",
                        )
                        .await;
                }
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    seen.push(line.trim().to_string());
                    let _ = write_half.write_all(response).await;
                }
            }
            let _ = tx.send(seen);
        });

        let server = Server::new(env(socket));
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": arguments
            }
        });
        let out = server
            .handle_line(&req.to_string())
            .await
            .expect("response");
        assert!(out.contains("\"isError\":false"), "got {out}");
        (out, rx.await.expect("socket task"))
    }
}
