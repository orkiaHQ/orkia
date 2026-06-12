// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Local IPC client for the `orkia-kernel` daemon.
//!
//! Wire protocol: JSON-RPC 2.0, newline-delimited messages, over a
//! Unix stream socket at `~/.orkia/run/kernel.sock`. The kernel
//! daemon is an optional component supplied separately; when it is
//! not running, callers fall back to the in-process heuristic.
//!
//! Each `classify_with_timeout` call opens a fresh connection, sends
//! one request, reads one response, closes. Pooling is an unrelated
//! optimization deferred for now — local-Unix connect cost
//! is in the tens of microseconds, well below the millisecond
//! budget that matters for classification.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use orkia_shell_types::{
    AssembleRequest, ForgeBuildRequest, ForgeBuildResponse, ForgeUsageRequest, ForgeUsageResponse,
    IntentGuess, KernelBenchmarkOutcome, KernelCancelOutcome, KernelContributeOutcome,
    KernelContributeStatus, KernelEvictOutcome, KernelModelStatus, KernelPullOutcome, KernelRpc,
    KernelRpcError, KernelVersion, METHOD_ABORT, METHOD_ADVANCE, METHOD_AUTHORIZE,
    METHOD_FORGE_BUILD, METHOD_FORGE_USAGE, METHOD_LLM_COMPLETE, METHOD_SEAL_ASSEMBLE,
    METHOD_SEAL_VERIFY, NativeCompletionRequest, NativeCompletionResponse, PipelineAbortRequest,
    PipelineAbortResponse, PipelineAdvanceRequest, PipelineAdvanceResponse,
    PipelineAuthorizeRequest, PipelineAuthorizeResponse, SealAssembleResponse, SealVerifyRequest,
    SealVerifyResponse,
};
use serde::Serialize;
use serde_json::{Value, json};

mod codec;

pub use codec::{ClassifyRequestParams, ClassifyResponse, HandshakeResponse};

/// Default location of the kernel control socket. Expanded from
/// `$HOME/.orkia/run/kernel.sock` so test fixtures can override.
pub fn default_socket_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".orkia").join("run").join("kernel.sock")
}

/// Current wire-protocol revision spoken by the OSS shell. Bump
/// alongside the kernel daemon's `SERVER_PROTOCOL` whenever the JSON-RPC
/// schema changes in a way the other side cannot ignore.
pub const CURRENT_KERNEL_PROTOCOL: u32 = 1;

/// Lowest daemon revision this shell will talk to. Anything below this
/// fails handshake fail-closed — the shell drops back to the in-process
/// heuristic and surfaces the mismatch in `$kernel`.
pub const MIN_KERNEL_PROTOCOL: u32 = 1;

/// Attempt to connect to the kernel at the default socket path,
/// perform a handshake, and return a ready-to-use client. Returns
/// `None` when no kernel is reachable — callers (the REPL) treat
/// this as "stay on heuristic, no error."
pub fn discover() -> Option<Arc<dyn KernelRpc>> {
    discover_at(default_socket_path())
}

/// Same as [`discover`] but for a caller-supplied socket path. Used
/// by tests with `tempfile::TempDir` sockets.
pub fn discover_at(path: PathBuf) -> Option<Arc<dyn KernelRpc>> {
    match UnixKernelClient::connect(path) {
        Ok(c) => Some(Arc::new(c)),
        Err(err) => {
            tracing::debug!(error = %err, "kernel-client: no kernel reachable");
            None
        }
    }
}

/// Concrete `KernelRpc` implementation talking to the daemon over a
/// Unix stream socket. Cheap to clone (path + cached version).
pub struct UnixKernelClient {
    socket_path: PathBuf,
    version: KernelVersion,
}

impl UnixKernelClient {
    /// Open a one-shot connection just long enough to handshake and
    /// cache the kernel version. The handshake uses a fixed 250ms
    /// budget — far more than a healthy local kernel needs, far less
    /// than would feel like a stall on `$login`.
    pub fn connect(socket_path: PathBuf) -> Result<Self, KernelRpcError> {
        let mut stream = open_stream(&socket_path, Duration::from_millis(250))?;
        let req = codec::request(
            1,
            "kernel.v1.handshake",
            json!({
                "client": "orkia-shell",
                "protocol": CURRENT_KERNEL_PROTOCOL,
            }),
        );
        send_line(&mut stream, &req)?;
        let resp = recv_line(&mut stream)?;
        let parsed: HandshakeResponse = codec::extract_result(&resp)?;
        // Fail-closed on either direction of skew: kernel too old for us,
        // or kernel demands a newer shell than we are. Both surface as
        // `KernelRpcError::Protocol`; the REPL drops to the heuristic.
        if parsed.protocol < MIN_KERNEL_PROTOCOL {
            return Err(KernelRpcError::Protocol(format!(
                "kernel protocol {} below shell minimum {}",
                parsed.protocol, MIN_KERNEL_PROTOCOL
            )));
        }
        if let Some(floor) = parsed.min_client
            && CURRENT_KERNEL_PROTOCOL < floor
        {
            return Err(KernelRpcError::Protocol(format!(
                "kernel requires client protocol >= {floor}, this shell speaks {CURRENT_KERNEL_PROTOCOL}"
            )));
        }
        Ok(Self {
            socket_path,
            version: KernelVersion {
                protocol: parsed.protocol,
                kernel: parsed.kernel,
                min_client: parsed.min_client,
                capabilities: parsed.capabilities,
            },
        })
    }
}

impl KernelRpc for UnixKernelClient {
    fn version(&self) -> KernelVersion {
        self.version.clone()
    }

    fn classify_with_timeout(
        &self,
        line: &str,
        timeout: Duration,
    ) -> Result<IntentGuess, KernelRpcError> {
        let mut stream = open_stream(&self.socket_path, timeout)?;
        let req = codec::request(
            1,
            "kernel.v1.classify",
            serde_json::to_value(ClassifyRequestParams {
                line: line.to_string(),
            })
            .map_err(|e| KernelRpcError::Protocol(e.to_string()))?,
        );
        send_line(&mut stream, &req)?;
        let resp = recv_line(&mut stream)?;
        let parsed: ClassifyResponse = codec::extract_result(&resp)?;
        Ok(parsed.into_intent_guess())
    }

    fn shutdown(&self) -> Result<(), KernelRpcError> {
        let mut stream = open_stream(&self.socket_path, Duration::from_millis(500))?;
        let req = codec::request(1, "kernel.v1.shutdown", Value::Null);
        send_line(&mut stream, &req)?;
        // The kernel may or may not reply before tearing down. Read
        // best-effort, ignore EOF.
        let _ = recv_line(&mut stream);
        Ok(())
    }

    fn list_models(&self) -> Result<Vec<KernelModelStatus>, KernelRpcError> {
        let mut stream = open_stream(&self.socket_path, Duration::from_millis(2_000))?;
        let req = codec::request(1, "kernel.v1.models.list", json!({}));
        send_line(&mut stream, &req)?;
        let resp = recv_line(&mut stream)?;
        #[derive(serde::Deserialize)]
        struct Wrap {
            models: Vec<KernelModelStatus>,
        }
        let parsed: Wrap = codec::extract_result(&resp)?;
        Ok(parsed.models)
    }

    fn pull_model(&self, id: &str) -> Result<KernelPullOutcome, KernelRpcError> {
        let mut stream = open_stream(&self.socket_path, Duration::from_millis(5_000))?;
        let req = codec::request(1, "kernel.v1.models.pull", json!({ "id": id }));
        send_line(&mut stream, &req)?;
        let resp = recv_line(&mut stream)?;
        codec::extract_result(&resp)
    }

    fn benchmark(&self, rounds: u32) -> Result<KernelBenchmarkOutcome, KernelRpcError> {
        let mut stream = open_stream(&self.socket_path, Duration::from_secs(60))?;
        let req = codec::request(1, "kernel.v1.benchmark", json!({ "rounds": rounds }));
        send_line(&mut stream, &req)?;
        let resp = recv_line(&mut stream)?;
        codec::extract_result(&resp)
    }

    fn contribute_status(&self) -> Result<KernelContributeStatus, KernelRpcError> {
        let mut stream = open_stream(&self.socket_path, Duration::from_millis(1_000))?;
        let req = codec::request(1, "kernel.v1.contribute.status", json!({}));
        send_line(&mut stream, &req)?;
        let resp = recv_line(&mut stream)?;
        codec::extract_result(&resp)
    }

    fn contribute_set(
        &self,
        on: bool,
        phrase: Option<&str>,
    ) -> Result<KernelContributeOutcome, KernelRpcError> {
        let mut stream = open_stream(&self.socket_path, Duration::from_millis(2_000))?;
        let req = codec::request(
            1,
            "kernel.v1.contribute.set",
            json!({ "on": on, "phrase": phrase }),
        );
        send_line(&mut stream, &req)?;
        let resp = recv_line(&mut stream)?;
        codec::extract_result(&resp)
    }

    fn contribute_purge(&self) -> Result<KernelContributeOutcome, KernelRpcError> {
        let mut stream = open_stream(&self.socket_path, Duration::from_secs(10))?;
        let req = codec::request(1, "kernel.v1.contribute.purge", json!({}));
        send_line(&mut stream, &req)?;
        let resp = recv_line(&mut stream)?;
        codec::extract_result(&resp)
    }

    fn cancel_pull(&self, id: &str) -> Result<KernelCancelOutcome, KernelRpcError> {
        let mut stream = open_stream(&self.socket_path, Duration::from_millis(1_000))?;
        let req = codec::request(1, "kernel.v1.models.cancel", json!({ "id": id }));
        send_line(&mut stream, &req)?;
        let resp = recv_line(&mut stream)?;
        codec::extract_result(&resp)
    }

    fn evict_loaded(&self) -> Result<KernelEvictOutcome, KernelRpcError> {
        let mut stream = open_stream(&self.socket_path, Duration::from_millis(2_000))?;
        let req = codec::request(1, "kernel.v1.models.evict", json!({}));
        send_line(&mut stream, &req)?;
        let resp = recv_line(&mut stream)?;
        codec::extract_result(&resp)
    }

    fn pipeline_authorize(
        &self,
        req: PipelineAuthorizeRequest,
    ) -> Result<PipelineAuthorizeResponse, KernelRpcError> {
        self.pipeline_call(METHOD_AUTHORIZE, &req)
    }

    fn pipeline_advance(
        &self,
        req: PipelineAdvanceRequest,
    ) -> Result<PipelineAdvanceResponse, KernelRpcError> {
        self.pipeline_call(METHOD_ADVANCE, &req)
    }

    fn pipeline_abort(
        &self,
        req: PipelineAbortRequest,
    ) -> Result<PipelineAbortResponse, KernelRpcError> {
        self.pipeline_call(METHOD_ABORT, &req)
    }

    fn forge_build(&self, req: ForgeBuildRequest) -> Result<ForgeBuildResponse, KernelRpcError> {
        // The kernel relays a backend LLM build that can run for tens of
        // seconds; give it a generous read ceiling, unlike the fast
        // pipeline decision calls.
        self.rpc_call(METHOD_FORGE_BUILD, &req, Duration::from_secs(180))
    }

    fn forge_usage(&self, req: ForgeUsageRequest) -> Result<ForgeUsageResponse, KernelRpcError> {
        self.rpc_call(METHOD_FORGE_USAGE, &req, Duration::from_secs(30))
    }

    fn seal_assemble(&self, req: AssembleRequest) -> Result<SealAssembleResponse, KernelRpcError> {
        // Local CPU + fs: collect audit ledgers, canonicalize, hash-chain,
        // ECDSA sign, write. Bounded but can touch many ledger files at RFC
        // closure — a 60s ceiling is generous without risking a wedge.
        self.rpc_call(METHOD_SEAL_ASSEMBLE, &req, Duration::from_secs(60))
    }

    fn seal_verify(&self, req: SealVerifyRequest) -> Result<SealVerifyResponse, KernelRpcError> {
        self.rpc_call(METHOD_SEAL_VERIFY, &req, Duration::from_secs(30))
    }

    fn llm_complete(
        &self,
        req: NativeCompletionRequest,
    ) -> Result<NativeCompletionResponse, KernelRpcError> {
        // One relayed model completion: a long agentic turn against a slow
        // provider can legitimately run for minutes. Distinct from the
        // 250ms handshake budget — the native session actor calls this
        // from its own task, never the REPL thread.
        self.rpc_call(METHOD_LLM_COMPLETE, &req, Duration::from_secs(300))
    }
}

impl UnixKernelClient {
    /// Shared transport for the `kernel.v1.pipeline.*` RPCs. These are
    /// pure decision calls (validate / compose / read a capped output
    /// file) — no agent ever runs inside the kernel — so a 5s ceiling is
    /// generous. The PTY work happens shell-side between calls.
    fn pipeline_call<P: Serialize, R: for<'de> serde::Deserialize<'de>>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, KernelRpcError> {
        self.rpc_call(method, params, Duration::from_secs(5))
    }

    /// One-shot JSON-RPC round-trip with an explicit read/write timeout.
    /// `timeout` bounds how long we wait for the kernel's reply — short for
    /// decision calls, long for relayed backend work (Forge build).
    fn rpc_call<P: Serialize, R: for<'de> serde::Deserialize<'de>>(
        &self,
        method: &str,
        params: &P,
        timeout: Duration,
    ) -> Result<R, KernelRpcError> {
        let mut stream = open_stream(&self.socket_path, timeout)?;
        let params =
            serde_json::to_value(params).map_err(|e| KernelRpcError::Protocol(e.to_string()))?;
        let req = codec::request(1, method, params);
        send_line(&mut stream, &req)?;
        let resp = recv_line(&mut stream)?;
        codec::extract_result(&resp)
    }
}

fn open_stream(path: &Path, timeout: Duration) -> Result<UnixStream, KernelRpcError> {
    let stream = UnixStream::connect(path).map_err(|e| classify_io(e, "connect"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| KernelRpcError::Io(e.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| KernelRpcError::Io(e.to_string()))?;
    Ok(stream)
}

fn classify_io(err: std::io::Error, op: &str) -> KernelRpcError {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => {
            KernelRpcError::Unavailable(format!("{op}: {err}"))
        }
        ErrorKind::TimedOut | ErrorKind::WouldBlock => KernelRpcError::Timeout,
        _ => KernelRpcError::Io(format!("{op}: {err}")),
    }
}

fn send_line<T: Serialize>(stream: &mut UnixStream, msg: &T) -> Result<(), KernelRpcError> {
    let mut bytes = serde_json::to_vec(msg).map_err(|e| KernelRpcError::Protocol(e.to_string()))?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .map_err(|e| classify_io(e, "write"))
}

fn recv_line(stream: &mut UnixStream) -> Result<Value, KernelRpcError> {
    use std::io::Read as _;
    // SEC-074: bound the read to prevent a runaway daemon from OOM-ing the shell.
    // 1 MiB is far larger than any well-formed JSON-RPC response; anything bigger
    // is treated as a protocol error (fail-closed).
    const MAX_LINE_BYTES: u64 = 1024 * 1024;
    // Wrap stream in a type-erased reader to resolve Read/Write by_ref ambiguity.
    let capped = (stream as &mut dyn std::io::Read).take(MAX_LINE_BYTES);
    let mut buf = String::new();
    let n = BufReader::new(capped)
        .read_line(&mut buf)
        .map_err(|e| classify_io(e, "read"))?;
    if n == 0 {
        return Err(KernelRpcError::Unavailable("peer closed".into()));
    }
    // If the buffer hit the cap without a newline, the response is malformed.
    if buf.len() as u64 >= MAX_LINE_BYTES && !buf.ends_with('\n') {
        return Err(KernelRpcError::Protocol(format!(
            "response exceeded {MAX_LINE_BYTES} byte cap without a newline"
        )));
    }
    serde_json::from_str(buf.trim_end_matches('\n'))
        .map_err(|e| KernelRpcError::Protocol(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use tempfile::TempDir;

    fn spawn_stub_kernel(
        path: PathBuf,
        handler: impl Fn(Value) -> Value + Send + Sync + 'static,
    ) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut buf = String::new();
                if reader.read_line(&mut buf).unwrap_or(0) == 0 {
                    continue;
                }
                let req: Value = serde_json::from_str(buf.trim_end()).unwrap();
                let result = handler(req);
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": result,
                });
                let mut bytes = serde_json::to_vec(&resp).unwrap();
                bytes.push(b'\n');
                stream.write_all(&bytes).unwrap();
            }
        })
    }

    #[test]
    fn discover_returns_none_when_socket_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nope.sock");
        assert!(discover_at(path).is_none());
    }

    #[test]
    fn handshake_populates_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("k.sock");
        let _h = spawn_stub_kernel(
            path.clone(),
            |_req| json!({ "protocol": 1, "kernel": "0.1.0-stub" }),
        );
        let client = UnixKernelClient::connect(path).unwrap();
        assert_eq!(client.version().protocol, 1);
        assert_eq!(client.version().kernel, "0.1.0-stub");
    }

    #[test]
    fn classify_round_trip_agent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("k.sock");
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let calls_c = calls.clone();
        let _h = spawn_stub_kernel(path.clone(), move |req| {
            let mut n = calls_c.lock().unwrap();
            *n += 1;
            let method = req["method"].as_str().unwrap_or("");
            match method {
                "kernel.v1.handshake" => json!({ "protocol": 1, "kernel": "stub" }),
                "kernel.v1.classify" => json!({ "intent": "agent" }),
                _ => json!({}),
            }
        });
        let client = UnixKernelClient::connect(path).unwrap();
        let g = client
            .classify_with_timeout("@faye hi", Duration::from_millis(500))
            .unwrap();
        assert_eq!(g, IntentGuess::Agent);
        // 2 calls = 1 handshake + 1 classify.
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[test]
    fn timeout_surfaces_as_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("k.sock");
        let _h = spawn_stub_kernel(path.clone(), |req| {
            // Stall on classify; honor handshake.
            let method = req["method"].as_str().unwrap_or("");
            if method == "kernel.v1.classify" {
                std::thread::sleep(Duration::from_millis(200));
            }
            json!({ "protocol": 1, "kernel": "stub", "intent": "agent" })
        });
        let client = UnixKernelClient::connect(path).unwrap();
        let err = client
            .classify_with_timeout("hi", Duration::from_millis(20))
            .unwrap_err();
        assert!(matches!(
            err,
            KernelRpcError::Timeout | KernelRpcError::Io(_)
        ));
    }
}
