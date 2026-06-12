// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Unix-socket journal listener.
//!
//! Listens on `<data_dir>/run/orkia.sock` for NDJSON envelopes from
//! external writers (notably `orkia bridge`, which agent hooks invoke).
//! Each accepted connection runs a per-task line reader that parses
//! one envelope per line and forwards it through an mpsc channel for
//! the REPL to drain between prompts.
//!
//! Shell-originated events (lifecycle, shell SEAL, tell) bypass the
//! socket and send directly through the same channel via
//! [`JournalListener::sender`] — same envelope, same downstream, no
//! IPC overhead.

use std::path::{Path, PathBuf};

use std::sync::Arc;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

use orkia_shell_types::journal::types::JournalEnvelope;

/// Capacity of the broadcast bus that fans out every envelope to
/// secondary subscribers (e.g. `orkia-stream`). Sized so a slow
/// subscriber can lag ~one second's worth of bursty hooks before the
/// broadcast layer drops; lagged subscribers receive `RecvError::Lagged`
/// and self-recover.
const BROADCAST_BUS_CAPACITY: usize = 1024;
use crate::error::JournalHubError;
use crate::router::HookRouter;
use orkia_shell_types::{JobId, JournalEnvelopeHook, JournalStopHook};

/// Optional handlers the listener task fires *during* each receive
/// — in real time, even while the REPL main loop is parked in
/// `read_line` or attached to an agent. Without these, hook events
/// would only land visibly after the next prompt was drawn.
///
/// All handlers are `Send + Sync + Clone` (each is `Arc`-backed
/// internally) so the listener task can carry them across `await`
/// points without borrow grief.
#[derive(Clone, Default)]
pub struct LiveJournalHandlers {
    /// Unified protocol router — every parsed `Hook` envelope is
    /// fed through `HookRouter::route_hook` here so downstream
    /// consumers (V3 Surface app, metrics, …) see the event with
    /// zero REPL-main-loop latency. Trait object so the concrete
    /// `EventRouter` (in `orkia-shell`) is injected without a
    /// dependency edge back into the shell.
    pub router: Option<Arc<dyn HookRouter>>,
    /// Real-time toast channel. Pre-built ANSI strings get pushed
    /// here; the renderer's external-printer worker drains and
    /// prints them above the live prompt. `None` falls back to
    /// the next-prompt queue path on the REPL side.
    pub printer: Option<std::sync::mpsc::Sender<String>>,
    /// Set whenever the REPL is currently splicing a foreground
    /// attach to an agent PTY. Any printer push while this is true
    /// would land inside the attached child's TUI display and
    /// corrupt it — so the listener skips the toast path entirely
    /// and the envelope only flows through the journal drain. The
    /// router still fires (downstream consumers care about real-
    /// time hook flow regardless of attach state).
    pub attach_active: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// any line whose JSON object contains a top-level `"jsonrpc"` key is
    /// dispatched through it and the response is written back on the same
    /// stream. Lines without `"jsonrpc"` fall through to the journal parser
    /// — this is how the existing NDJSON-only consumers stay unaffected.
    pub mcp: Option<Arc<dyn McpDispatcher>>,
    /// Optional post-processor invoked after every successfully parsed
    /// envelope. Used by `orkia-final-response` to detect `Stop` events
    /// and spawn a transcript-extraction task off the listener loop
    pub stop_hook: Option<Arc<dyn JournalStopHook>>,
    /// Optional post-processor invoked after every successfully parsed
    /// envelope (not just Stop). Used by the Team pipeline coordinator
    /// to consume `PipelineOutput` envelopes emitted by the MCP pipe
    pub envelope_hook: Option<Arc<dyn JournalEnvelopeHook>>,
}

/// Trait implemented by the shell layer to bridge JSON-RPC frames received
/// on the journal socket into the RFC state service. Lives in this module
/// (not in `orkia-rfc-mcp`) because dispatching requires workspace lookup,
/// project resolution, and bridge recording — all owned by the shell.
pub trait McpDispatcher: Send + Sync {
    /// Process one JSON-RPC line and return the serialized response line plus
    /// any knowledge-node ids served by this call. Implementations must not
    /// panic on malformed input — return a JSON-RPC error response instead.
    fn dispatch(&self, line: &str, peer_job_id: Option<JobId>) -> McpReply;
}

/// What a [`McpDispatcher`] returns: the JSON-RPC response line, plus the ids of
/// any knowledge nodes served on this call. The read path is strictly read-only
/// journal event the REPL-owned store writer applies, never written here.
pub struct McpReply {
    /// Serialized JSON-RPC response line (no trailing newline).
    pub response: String,
    /// Knowledge node ids served by this call (empty for non-KG methods).
    pub accessed_node_ids: Vec<String>,
}

impl McpReply {
    /// A reply that served no knowledge nodes (the common, RFC-method case).
    pub fn plain(response: String) -> Self {
        Self {
            response,
            accessed_node_ids: Vec::new(),
        }
    }
}

/// Background listener owning the Unix socket. Cloning the inner
/// `Sender` is the in-process emit path.
pub struct JournalListener {
    socket_path: PathBuf,
    /// Internal sender — every envelope (socket or in-process) lands
    /// here; a fanout task forwards each one to the caller's external
    /// receiver AND broadcasts to `bus`.
    tx: mpsc::UnboundedSender<JournalEnvelope>,
    bus: broadcast::Sender<JournalEnvelope>,
}

impl JournalListener {
    /// Bind to `<data_dir>/run/orkia.sock` and spawn the accept loop.
    /// Returns the listener (which owns the socket file and removes it
    /// on drop) and the receiver the REPL drains.
    pub fn start(
        data_dir: &Path,
    ) -> Result<(Self, mpsc::UnboundedReceiver<JournalEnvelope>), JournalHubError> {
        Self::start_with_handlers(data_dir, LiveJournalHandlers::default())
    }

    /// Like [`Self::start`] but installs handlers that the listener
    /// task fires *in real time* on every successful parse — before
    /// the envelope is queued for the REPL's main-loop drain. Lets
    /// the protocol router and the real-time toast channel see hook
    /// events while the REPL is busy elsewhere (notably during a
    /// foreground attach).
    pub fn start_with_handlers(
        data_dir: &Path,
        live: LiveJournalHandlers,
    ) -> Result<(Self, mpsc::UnboundedReceiver<JournalEnvelope>), JournalHubError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let listener = Self::start_with_channel(data_dir, live, tx)?;
        Ok((listener, rx))
    }

    /// Bind the socket using a caller-supplied channel. Lets callers
    /// hand a `tx` clone to other components (e.g. the
    /// `FinalResponseService`) *before* the accept loop starts, so the
    /// service can emit envelopes through the same drain path as the
    /// bridge.
    pub fn start_with_channel(
        data_dir: &Path,
        live: LiveJournalHandlers,
        external_tx: mpsc::UnboundedSender<JournalEnvelope>,
    ) -> Result<Self, JournalHubError> {
        Self::start_with_channel_seeded(data_dir, live, external_tx, None)
    }

    /// ingress stamp counter. `Some(n)` ⇒ every envelope passing the fanout is
    /// stamped with a monotonic `hub_seq` starting at `n + 1`; `None` ⇒ no
    /// stamping (current behavior). Only the daemon-resident hub seeds.
    pub fn start_with_channel_seeded(
        data_dir: &Path,
        live: LiveJournalHandlers,
        external_tx: mpsc::UnboundedSender<JournalEnvelope>,
        seq_seed: Option<u64>,
    ) -> Result<Self, JournalHubError> {
        let socket_path = data_dir.join("run").join("orkia.sock");
        Self::start_at_seeded(socket_path, live, external_tx, seq_seed)
    }

    /// Like [`Self::start_with_channel`] but binds an explicit `socket_path`
    /// instead of deriving `<data_dir>/run/orkia.sock`. The socket's parent
    /// directory is created if absent.
    ///
    /// Used by the detached runtime's per-job local hub
    /// runtime hosts its own agent on a per-job socket so it can consume that
    /// agent's hooks locally — without moving `data_dir`, which roots
    /// `reasoning.db`, `agents/`, and trust state that must stay shared with
    /// the main shell.
    pub fn start_at(
        socket_path: PathBuf,
        live: LiveJournalHandlers,
        external_tx: mpsc::UnboundedSender<JournalEnvelope>,
    ) -> Result<Self, JournalHubError> {
        Self::start_at_seeded(socket_path, live, external_tx, None)
    }

    /// counter (see [`Self::start_with_channel_seeded`]). The fanout task — the
    /// single point every envelope (socket or in-process) crosses — stamps each
    /// one with a monotonic `hub_seq` BEFORE the bus send, so the disk-tee
    /// persists it and a re-subscribing REPL can replay the durable backlog
    /// keeps the sequence monotonic across a daemon restart.
    pub fn start_at_seeded(
        socket_path: PathBuf,
        live: LiveJournalHandlers,
        external_tx: mpsc::UnboundedSender<JournalEnvelope>,
        seq_seed: Option<u64>,
    ) -> Result<Self, JournalHubError> {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                JournalHubError::Listener(format!("journal: create socket dir: {e}"))
            })?;
        }
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path).map_err(|e| {
            JournalHubError::Listener(format!("journal: bind {socket_path:?}: {e}"))
        })?;

        // Internal pipe. Every envelope — socket or in-process via
        // `sender()` — flows through `internal_tx`; the fanout task
        // forwards each one to the caller's `external_tx` AND to the
        // broadcast bus that secondary subscribers consume.
        let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<JournalEnvelope>();
        let (bus_tx, _) = broadcast::channel::<JournalEnvelope>(BROADCAST_BUS_CAPACITY);
        let bus_for_fanout = bus_tx.clone();
        tokio::spawn(async move {
            // point when seeded (daemon hub). The fanout task is the one place
            // every envelope crosses, and it is single-threaded, so a plain
            // local counter is lock-free and strictly increasing. Stamp BEFORE
            // the bus send so the disk-tee persists the seq. `None` seed leaves
            // envelopes untouched (relay / per-job LPH / daemon-less fallback).
            let mut next_seq = seq_seed.map(|s| s + 1);
            while let Some(mut env) = internal_rx.recv().await {
                if let Some(seq) = next_seq.as_mut() {
                    env.hub_seq = Some(*seq);
                    *seq += 1;
                }
                // Best-effort: a dropped subscriber is not an error.
                let _ = bus_for_fanout.send(env.clone());
                if external_tx.send(env).is_err() {
                    // REPL drain receiver dropped — listener shutdown.
                    break;
                }
            }
        });

        let accept_tx = internal_tx.clone();
        tokio::spawn(accept_loop(listener, accept_tx, live));

        Ok(Self {
            socket_path,
            tx: internal_tx,
            bus: bus_tx,
        })
    }

    /// Relay constructor for subscribed mode (MIGRATE-AGENT-SPAWN-TO-DAEMON
    /// owns it and hosts the real hub. Instead the bin pumps daemon-streamed
    /// envelopes into the returned `feed_tx`; each one fires the live
    /// handlers (the REPL-resident set: router/printer/attach/envelope — no
    /// MCP dispatch, that is proxied by the daemon; no stop-hook/disk-tee,
    /// those run daemon-side and survive REPL restarts) and fans out to the
    /// drain + broadcast bus exactly like the socket accept loop does.
    ///
    /// No `UnixListener` is bound, so `socket_path` is an empty sentinel and
    /// the `Drop` `remove_file` is a harmless no-op.
    pub fn start_relay(
        live: LiveJournalHandlers,
        external_tx: mpsc::UnboundedSender<JournalEnvelope>,
    ) -> (Self, mpsc::UnboundedSender<JournalEnvelope>) {
        // Same internal pipe + fanout as `start_with_channel`: every
        // envelope flows through `internal_tx`; the fanout task forwards
        // to the REPL drain AND the broadcast bus.
        let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<JournalEnvelope>();
        let (bus_tx, _) = broadcast::channel::<JournalEnvelope>(BROADCAST_BUS_CAPACITY);
        let bus_for_fanout = bus_tx.clone();
        tokio::spawn(async move {
            while let Some(env) = internal_rx.recv().await {
                let _ = bus_for_fanout.send(env.clone());
                if external_tx.send(env).is_err() {
                    break;
                }
            }
        });

        // Feed task: daemon-streamed envelopes arrive on `feed_rx`. Fire the
        // live handlers (same real-time semantics as the socket path) then
        // push into the internal pipe for fanout + drain.
        let (feed_tx, mut feed_rx) = mpsc::unbounded_channel::<JournalEnvelope>();
        let feed_internal = internal_tx.clone();
        tokio::spawn(async move {
            while let Some(env) = feed_rx.recv().await {
                fire_live_handlers(&live, &env);
                if feed_internal.send(env).is_err() {
                    break;
                }
            }
        });

        let listener = Self {
            socket_path: PathBuf::new(),
            tx: internal_tx,
            bus: bus_tx,
        };
        (listener, feed_tx)
    }

    /// Subscribe to the broadcast bus. Every parsed `JournalEnvelope`
    /// — whether it arrives from the Unix socket or via the in-process
    /// `sender()` clone — is fanned out to every subscriber. Slow
    /// subscribers receive `RecvError::Lagged(n)` and may resync.
    pub fn subscribe(&self) -> broadcast::Receiver<JournalEnvelope> {
        self.bus.subscribe()
    }

    /// Path of the bound socket. Useful for diagnostics and tests.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Clone the channel sender. Used by the REPL to emit in-process
    /// events (lifecycle, shell SEAL, tell) without going through the
    /// socket.
    pub fn sender(&self) -> mpsc::UnboundedSender<JournalEnvelope> {
        self.tx.clone()
    }
}

impl Drop for JournalListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

async fn accept_loop(
    listener: UnixListener,
    tx: mpsc::UnboundedSender<JournalEnvelope>,
    live: LiveJournalHandlers,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let conn_tx = tx.clone();
                let conn_live = live.clone();
                tokio::spawn(handle_connection(stream, conn_tx, conn_live));
            }
            Err(e) => {
                tracing::warn!("journal accept error: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}

async fn handle_connection(
    stream: UnixStream,
    tx: mpsc::UnboundedSender<JournalEnvelope>,
    live: LiveJournalHandlers,
) {
    use orkia_shell_types::input_limits::JOURNAL_LINE_MAX_BYTES;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    // Bidirectional split: MCP needs to write responses back, the journal
    // path is read-only. We split once and only the MCP branch writes.
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    // The peer is an agent process (untrusted). We replace
    // `reader.lines()` (which grows the inner buffer without bound)
    // with a manual `read_line` loop that caps each line at the
    // journal-line budget — a malicious or buggy agent emitting one
    // multi-GiB line can no longer OOM the host.

    // Per-connection state for MCP. An agent identifies itself with the
    // pseudo-method `orkia_rfc_init { job_id: <n> }` on its first frame so
    // subsequent `orkia_rfc_ask` calls can be recorded against the right
    // Connections that never `init` simply never get PTY injection — read
    // tools still work.
    let mut peer_job_id: Option<JobId> = None;

    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        let read_result = (&mut reader)
            .take(JOURNAL_LINE_MAX_BYTES as u64 + 1)
            .read_line(&mut line_buf)
            .await;
        let line: String = match read_result {
            Ok(0) => return,
            Ok(_) if line_buf.len() > JOURNAL_LINE_MAX_BYTES => {
                // Drain to the next newline before continuing so the
                // tail of an oversize line isn't interpreted as a new
                // frame on the next iteration.
                while !line_buf.ends_with('\n') {
                    line_buf.clear();
                    let drained = (&mut reader)
                        .take(JOURNAL_LINE_MAX_BYTES as u64)
                        .read_line(&mut line_buf)
                        .await;
                    if !matches!(drained, Ok(n) if n > 0) || line_buf.ends_with('\n') {
                        break;
                    }
                }
                tracing::warn!(
                    cap = JOURNAL_LINE_MAX_BYTES,
                    "journal: dropped over-cap envelope",
                );
                continue;
            }
            Ok(_) => std::mem::take(&mut line_buf),
            Err(e) => {
                tracing::warn!("journal: read error: {e}");
                return;
            }
        };
        // Branches below pattern-match the original `Ok(Some(line))` arm.
        {
            {
                if line.trim().is_empty() {
                    continue;
                }
                // MCP fast-path: lines with a top-level `"jsonrpc"` field
                // are JSON-RPC, not journal envelopes. We don't fully parse
                // here — the substring probe is enough to disambiguate (a
                // journal envelope never carries that field).
                if line.contains("\"jsonrpc\"") {
                    // Intercept `orkia_rfc_init` locally (it's connection
                    // state, not RFC state) before forwarding to the
                    // dispatcher. The intercept is keyed on the method
                    // string so a missing dispatcher still handles it.
                    if let Some(jid) = try_parse_init(&line) {
                        peer_job_id = Some(JobId(jid));
                        let id = extract_request_id(&line);
                        let resp = init_response(id);
                        if write_half.write_all(resp.as_bytes()).await.is_err()
                            || write_half.write_all(b"\n").await.is_err()
                        {
                            return;
                        }
                        let _ = write_half.flush().await;
                        continue;
                    }
                    if let Some(d) = live.mcp.as_ref() {
                        let reply = d.dispatch(&line, peer_job_id);
                        // Served KG nodes ride out as a decay signal on the bus;
                        // the REPL-owned store writer applies the bump.
                        if !reply.accessed_node_ids.is_empty() {
                            let _ = tx.send(JournalEnvelope::knowledge_access(
                                peer_job_id.map(|j| j.0),
                                &reply.accessed_node_ids,
                            ));
                        }
                        if write_half
                            .write_all(reply.response.as_bytes())
                            .await
                            .is_err()
                            || write_half.write_all(b"\n").await.is_err()
                        {
                            return;
                        }
                        let _ = write_half.flush().await;
                        continue;
                    }
                }
                match serde_json::from_str::<JournalEnvelope>(&line) {
                    Ok(env) => {
                        fire_live_handlers(&live, &env);
                        if tx.send(env).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        if let Some(recovered) = crate::normalize::try_recover_hook_line(&line) {
                            tracing::debug!(
                                event = recovered.event.as_deref(),
                                source = recovered.source.as_deref(),
                                "journal: recovered raw hook payload",
                            );
                            fire_live_handlers(&live, &recovered);
                            if tx.send(recovered).is_err() {
                                return;
                            }
                            continue;
                        }
                        tracing::warn!(
                            "journal: parse failed ({e}) on line: {}",
                            truncate_for_log(&line, 200)
                        );
                    }
                }
            }
        }
    }
}

/// Fire every installed live handler. Best-effort: a failed
/// printer send (renderer dropped) or router send (consumer
/// dropped) is silently ignored — the envelope still proceeds
/// down the REPL drain path so observability never degrades to
/// a total loss.
fn fire_live_handlers(live: &LiveJournalHandlers, env: &JournalEnvelope) {
    if let Some(router) = live.router.as_ref() {
        router.route_hook(env);
    }
    if let Some(hook) = live.stop_hook.as_ref() {
        hook.on_stop(env);
    }
    if let Some(hook) = live.envelope_hook.as_ref() {
        hook.on_envelope(env);
    }
    let attached = live
        .attach_active
        .as_ref()
        .is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst));
    if attached {
        // User is currently looking at an attached child's TUI;
        // dropping ANSI toast lines into stdout would scribble
        // over its display. The envelope is still queued for the
        // REPL drain (see caller), so it surfaces above the next
        // prompt once the user detaches.
        return;
    }
    if let Some(printer) = live.printer.as_ref()
        && let Some(line) = crate::notifications::notification_for(env)
    {
        let _ = printer.send(line);
    }
}

/// Probe a JSON-RPC line for the `orkia_rfc_init` handshake. Returns the
/// announced `job_id` on match. Strict — malformed inits return `None` and
/// fall through to the normal dispatch path so the caller gets a proper
/// JSON-RPC error response from the RFC layer instead of a silent ack.
fn try_parse_init(line: &str) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("method").and_then(|m| m.as_str())? != "orkia_rfc_init" {
        return None;
    }
    v.get("params")
        .and_then(|p| p.get("job_id"))
        .and_then(|j| j.as_u64())
        .map(|n| n as u32)
}

fn extract_request_id(line: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("id").cloned()
}

fn init_response(id: Option<serde_json::Value>) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "ok": true },
    })
    .to_string()
}

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
#[path = "listener_tests.rs"]
mod tests;
