// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

//!
//! The daemon owns `orkia.sock` and hosts the `orkia-journal-hub`, so agent
//! hooks, the FinalResponseService stop-hook capture, and the disk mirror all
//! survive a REPL restart. The REPL connects as the single subscriber
//! (`Request::Subscribe`) and receives every envelope as a
//! [`StreamFrame::Envelope`] for its REPL-resident consumers (EventRouter →
//! SEAL, approval, attention, oneshot, sink, knowledge/stream/intelligence).
//!
//! MCP/RFC JSON-RPC frames land here too (the socket multiplexes both
//! protocols), but their dispatch needs REPL-resident state. We bridge them
//! with [`ProxyMcpDispatcher`]: each frame is forwarded to the REPL as a
//! [`StreamFrame::McpProxy`] keyed by `corr_id`; the REPL replies with
//! `Request::McpProxyReply`, which [`JournalHost::resolve_mcp`] feeds back to
//! the blocked dispatch call. No subscriber ⇒ fail-closed JSON-RPC error.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender as StdSender, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};

use orkia_shell::ShellConfig;
use orkia_shell::journal::{
    JournalEnvelope, JournalHub, JournalHubConfig, JournalStore, LiveJournalHandlers,
    McpDispatcher, McpReply,
};
use orkia_shell_types::JobId;
use tokio::runtime::Handle;

use super::protocol::{DaemonJobEvent, StreamFrame, write_stream_frame};

/// Handle to the daemon-resident journal hub and its single REPL subscriber.
///
/// Lives for the whole daemon process. The `Subscribe` handler hands the
/// accepted REPL stream to [`Self::attach_subscriber`]; control frames
/// (`JournalEmit`, `McpProxyReply`) route through [`Self::emit`] /
/// [`Self::resolve_mcp`].
pub(super) struct JournalHost {
    /// In-process emit path into the hub's INGRESS (REPL `JournalEmit`
    /// envelopes). This is `hub.sender()` — the same channel the socket
    /// listener feeds — so every emit is hub_seq-stamped and fanned out to
    /// the bus (disk tee, stop-hook) and the subscriber drain. Sending to
    /// the drain directly would skip the stamp, the disk mirror, and the
    hub_tx: tokio::sync::mpsc::UnboundedSender<JournalEnvelope>,
    /// The current REPL subscriber's outbound sink. `None` whenever no REPL
    /// is connected — every stream write checks this first, so a missing
    /// subscriber simply drops frames (the disk mirror still persists them).
    subscriber: Arc<Mutex<Option<StdSender<StreamFrame>>>>,
    /// In-flight MCP proxy calls, keyed by `corr_id`. A short-lived RPC
    /// correlation map (not a core data structure): each entry is one
    /// agent connection blocked in [`ProxyMcpDispatcher::dispatch`] awaiting
    /// the REPL's reply. Justified `Mutex` per CLAUDE.md — the alternative
    /// (a channel + resolver task) buys nothing for a request/reply table.
    pending: Arc<Mutex<HashMap<u64, SyncSender<McpReply>>>>,
    /// Kept alive so the hub's listener socket is not dropped (its `Drop`
    /// removes `orkia.sock`). Held for the daemon's lifetime.
    _hub: JournalHub,
    /// Kept alive so the disk-mirror writer thread keeps running.
    _store: JournalStore,
    /// Generation counter: bumped on every attach so a stale rx-drain
    /// forwarder (from a previous subscriber) stops writing to a replaced
    /// sink.
    generation: Arc<AtomicU64>,
    /// Data dir — root of the durable `journal.jsonl` the disk-tee writes.
    data_dir: PathBuf,
    /// journal to a subscriber. Owned by the daemon (one owner per resource,
    /// #2): the streaming position is journal state, not REPL state.
    cursor: StreamCursor,
}

/// The daemon's view of how far a subscriber has been fed the durable journal.
/// Cloned into the rx-forwarder so it can advance/skip without re-locking the
/// host. Both counters are `hub_seq` values (0 = "before any stamped event").
#[derive(Clone)]
struct StreamCursor {
    /// High-water of `hub_seq` actually handed to a subscriber sink. Seeded
    /// from the on-disk max so a fresh daemon assumes the prior session already
    /// consumed what is persisted (a daemon restart tears its agents down, so
    /// there is nothing live to replay). Stays put while no subscriber is
    /// attached — that is exactly the gap the next attach replays.
    streamed_through: Arc<AtomicU64>,
    /// Max `hub_seq` injected as backlog on the CURRENT attach. The forwarder
    /// skips live `Envelope` frames with `hub_seq <= replay_high` so the
    /// backlog/live join never double-delivers (the backlog already covered
    /// them). Re-armed on every attach.
    replay_high: Arc<AtomicU64>,
}

impl JournalHost {
    /// Build the disk store + FinalResponseService, start the hub (binds
    /// `orkia.sock`), and return the host. Errors only if the socket bind
    /// fails — same surface as the REPL's `boot_journal`.
    pub(super) fn start(config: &ShellConfig, runtime: Handle) -> Result<Self, String> {
        let store = JournalStore::new(&config.data_dir);
        let disk_writer = store.writer_handle();

        // cursor) from the max `hub_seq` already on disk, so the sequence stays
        // monotonic across a daemon restart and a fresh daemon does not replay
        // a dead session's journal to the next REPL.
        let disk_max = max_hub_seq_on_disk(&config.data_dir);

        // The hub's external drain (`outbound_tx`/rx) is how we feed the REPL
        // subscriber: every envelope the hub fans out lands on `drain_rx`, and
        // the forwarder task below pushes it to the current subscriber sink.
        // The broadcast bus drives the disk-tee + stop-hook (FRS) independently.
        // The drain is POST-fanout: nothing may emit into it directly, or the
        // envelope skips the hub_seq stamp, the bus, and the replay backlog.
        let (drain_tx, drain_rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();

        let subscriber: Arc<Mutex<Option<StdSender<StreamFrame>>>> = Arc::new(Mutex::new(None));
        let pending: Arc<Mutex<HashMap<u64, SyncSender<McpReply>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let proxy = ProxyMcpDispatcher {
            next: Arc::new(AtomicU64::new(1)),
            subscriber: Arc::clone(&subscriber),
            pending: Arc::clone(&pending),
        };

        // FRS emits `AgentFinalResponse` envelopes back through the hub's
        // INGRESS so they are hub_seq-stamped, disk-mirrored, and streamed to
        // the REPL. The hub does not exist yet (FRS is one of its handlers),
        // so FRS gets a bridge channel that a relay task pipes into
        // `hub.sender()` once the hub is up.
        let (frs_tx, mut frs_rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
        let frs = orkia_final_response::FinalResponseService::new(config.data_dir.clone(), frs_tx)
            .into_arc();

        let handlers = LiveJournalHandlers {
            // router/printer/attach_active/envelope_hook are REPL-resident —
            // the REPL fires them on the streamed envelopes (Option A).
            router: None,
            printer: None,
            attach_active: None,
            mcp: Some(Arc::new(proxy) as Arc<dyn McpDispatcher>),
            stop_hook: Some(frs as Arc<dyn orkia_shell_types::JournalStopHook>),
            envelope_hook: None,
        };

        // The hub spawns its internal tasks (socket accept, fanout, disk-tee)
        // via ambient `tokio::spawn`. `run_server` executes inside the
        // binary's MAIN runtime, so without entering the daemon runtime here
        // those tasks — which hold disk-writer sender clones — would land on
        // the main runtime, survive the daemon runtime's shutdown, and
        // deadlock `JournalStore::drop`'s writer join at teardown (the
        // `pty-daemon-stop` zombie). Enter the daemon handle so shutdown
        // actually reaps them.
        let hub = {
            let _guard = runtime.enter();
            JournalHub::start(JournalHubConfig {
                data_dir: config.data_dir.clone(),
                socket_path_override: None,
                handlers,
                outbound_tx: drain_tx,
                // Stop-hook already installed via `handlers.stop_hook`; the bus
                // stop-hook subscriber is for callers that wire FRS post-boot.
                stop_hook: None,
                disk_writer,
                seq_seed: Some(disk_max),
            })
            .map_err(|e| format!("journal hub: {e}"))?
        };

        // Relay the FRS bridge into the hub's ingress, now that it exists.
        // FRS's own `on_stop` filters on `event == "Stop"`, so the
        // `AgentFinalResponse` envelopes re-entering via the bus never loop.
        let frs_ingress = hub.sender();
        runtime.spawn(async move {
            while let Some(env) = frs_rx.recv().await {
                if frs_ingress.send(env).is_err() {
                    break;
                }
            }
        });

        let generation = Arc::new(AtomicU64::new(0));
        let cursor = StreamCursor {
            streamed_through: Arc::new(AtomicU64::new(disk_max)),
            replay_high: Arc::new(AtomicU64::new(disk_max)),
        };
        spawn_rx_forwarder(&runtime, drain_rx, Arc::clone(&subscriber), cursor.clone());

        Ok(Self {
            hub_tx: hub.sender(),
            subscriber,
            pending,
            _hub: hub,
            _store: store,
            generation,
            data_dir: config.data_dir.clone(),
            cursor,
        })
    }

    /// Register the REPL as the subscriber. Replaces any previous subscriber
    /// (only one REPL connects at a time): the old sink is dropped, so its
    /// forwarder stops and its in-flight MCP calls fail-closed. Spawns a
    /// dedicated OS thread that writes [`StreamFrame`]s to the REPL stream
    /// (blocking I/O, kept off the tokio runtime).
    pub(super) fn attach_subscriber(&self, stream: UnixStream) {
        let (frame_tx, frame_rx) = std::sync::mpsc::channel::<StreamFrame>();

        // journaled since it was last fed, so its EventRouter seals the gap
        // window before any live frame arrives. Order is load-bearing for the
        // SEAL prev_hash chain, so we QUEUE the backlog into the new channel
        // BEFORE the forwarder can see the sink (install happens after), and
        // arm `replay_high` first so the forwarder dedups any live frame that
        // races the disk read. `since` is the high-water of what was actually
        // WRITTEN to the previous subscriber's socket (advanced by its
        // `stream_writer`, not on channel send) — so anything the prior REPL
        // never received, including frames pushed into its dead channel during
        // the death window, falls into this backlog. That is the whole gap.
        let since = self.cursor.streamed_through.load(Ordering::SeqCst);
        let backlog: Vec<JournalEnvelope> = JournalStore::load_entries(&self.data_dir)
            .into_iter()
            .filter(|e| e.hub_seq.is_some_and(|s| s > since))
            .collect();
        let backlog_high = backlog
            .iter()
            .filter_map(|e| e.hub_seq)
            .max()
            .unwrap_or(since);
        // Arm the dedup gate BEFORE installing the sink so the forwarder skips
        // any live frame in the backlog range the instant it can see the sink.
        // The cursor itself is advanced by the new `stream_writer` as it writes
        // each backlog frame, so a subscriber that dies mid-backlog leaves the
        // unsent tail behind the high-water for the next attach to replay.
        self.cursor
            .replay_high
            .store(backlog_high, Ordering::SeqCst);
        // Queue the backlog into the not-yet-installed channel (unbounded std
        // mpsc — never blocks); the writer thread spawned below drains it
        // first, then the forwarder's live frames that land after install.
        for env in backlog {
            if frame_tx
                .send(StreamFrame::Envelope { envelope: env })
                .is_err()
            {
                break;
            }
        }

        // Bump generation and install the new sink before tearing down the
        // old one, so a frame produced during the swap is never lost.
        let my_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let dropped = {
            let mut guard = self.subscriber.lock().unwrap_or_else(|e| e.into_inner());
            guard.replace(frame_tx)
        };
        // Dropping the previous sink wakes its forwarder (channel closed) and
        // any MCP dispatchers blocked on it (see `fail_pending`).
        drop(dropped);
        self.fail_pending();

        let pending = Arc::clone(&self.pending);
        let subscriber = Arc::clone(&self.subscriber);
        let generation = Arc::clone(&self.generation);
        let streamed_through = Arc::clone(&self.cursor.streamed_through);
        let _ = std::thread::Builder::new()
            .name("orkia-journal-stream".to_string())
            .spawn(move || {
                stream_writer(stream, frame_rx, streamed_through);
                // Writer exited (REPL disconnected). Clear the sink iff it is
                // still ours, then fail any in-flight MCP calls so blocked
                // agents get an error instead of hanging forever.
                if generation.load(Ordering::SeqCst) == my_gen {
                    let mut guard = subscriber.lock().unwrap_or_else(|e| e.into_inner());
                    *guard = None;
                }
                fail_pending_map(&pending);
            });
    }

    /// Inject a REPL-originated in-process envelope into the hub's ingress
    /// (the `Request::JournalEmit` path). Same destination as the FRS emit:
    /// hub_seq stamp, then disk mirror + bus + stream back to the REPL's
    /// local consumers.
    pub(super) fn emit(&self, envelope: JournalEnvelope) {
        let _ = self.hub_tx.send(envelope);
    }

    /// Relay a detached runtime's `JobEvent` (the `Request::JobEventEmit` path)
    /// to the REPL subscriber as a [`StreamFrame::JobEvent`]. No subscriber ⇒
    /// drop the frame: the REPL re-derives job state from `List`/`Inspect` on
    /// its next poll, and a missing event is never fatal (#1 — never block on a
    /// gone subscriber). Mirrors the envelope fanout in [`spawn_rx_forwarder`].
    pub(super) fn push_job_event(&self, event: DaemonJobEvent) {
        let guard = self.subscriber.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sink) = guard.as_ref() {
            let _ = sink.send(StreamFrame::JobEvent { event });
        }
    }

    /// Resolve a blocked MCP proxy call with the REPL's reply
    /// (`Request::McpProxyReply`). No-op if the `corr_id` is unknown (the
    /// dispatcher already timed out / the subscriber was replaced).
    pub(super) fn resolve_mcp(&self, corr_id: u64, reply: McpReply) {
        let tx = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&corr_id);
        if let Some(tx) = tx {
            let _ = tx.send(reply);
        }
    }

    /// Drop every pending MCP reply sender so the blocked dispatch calls wake
    /// with a disconnect error. Called when the subscriber is replaced.
    fn fail_pending(&self) {
        fail_pending_map(&self.pending);
    }
}

/// Drop all pending reply senders: each blocked [`ProxyMcpDispatcher::dispatch`]
/// recv then returns `Err` and synthesises a JSON-RPC error.
fn fail_pending_map(pending: &Arc<Mutex<HashMap<u64, SyncSender<McpReply>>>>) {
    pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// Forward every hub envelope to the current subscriber sink. Runs on the
/// tokio runtime (async broadcast/mpsc recv); the actual socket write happens
/// on the dedicated [`stream_writer`] thread via the std channel.
fn spawn_rx_forwarder(
    runtime: &Handle,
    mut hub_rx: tokio::sync::mpsc::UnboundedReceiver<JournalEnvelope>,
    subscriber: Arc<Mutex<Option<StdSender<StreamFrame>>>>,
    cursor: StreamCursor,
) {
    runtime.spawn(async move {
        while let Some(env) = hub_rx.recv().await {
            let seq = env.hub_seq.unwrap_or(0);
            // delivered as backlog (`hub_seq <= replay_high`) is skipped, so
            // the backlog/live join never double-delivers. Live envelopes
            // carry `hub_seq > disk_max >= replay_high`, so this only fires on
            // the disk-tee eventual-consistency window.
            if seq != 0 && seq <= cursor.replay_high.load(Ordering::SeqCst) {
                continue;
            }
            let guard = subscriber.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(sink) = guard.as_ref() {
                // A closed channel just means the REPL detached; the next
                // attach installs a fresh sink. Drop the frame either way.
                // The high-water is NOT advanced here — `sink.send` only proves
                // the frame reached the channel, not the REPL's socket. The
                // `stream_writer` advances `streamed_through` after a SUCCESSFUL
                // socket write, so a frame pushed into a dying subscriber's
                // channel (write then fails) stays behind the cursor and is
                // replayed on the next attach (#8 fail-closed: never lose it).
                let _ = sink.send(StreamFrame::Envelope { envelope: env });
            }
        }
    });
}

/// Max `hub_seq` persisted in the durable journal mirror, or `0` if none.
/// Seeds the hub's ingress stamp counter and the streaming cursor so both
fn max_hub_seq_on_disk(data_dir: &std::path::Path) -> u64 {
    JournalStore::load_entries(data_dir)
        .iter()
        .filter_map(|e| e.hub_seq)
        .max()
        .unwrap_or(0)
}

/// Drain [`StreamFrame`]s and write them as NDJSON to the REPL stream. Blocks
/// on a dedicated OS thread so socket back-pressure never stalls the runtime.
/// Returns when the channel closes (sink dropped) or a write fails (REPL gone).
///
/// Advances `streamed_through` only after an `Envelope` frame has been
/// successfully written AND flushed to the socket — proof the REPL actually
/// received it. A frame whose write/flush fails (REPL gone) leaves the cursor
/// behind it, so the next attach replays it as backlog (#8 fail-closed). This
/// is why the high-water lives here and not in the forwarder: `sink.send`
/// succeeds against a still-open channel even when the socket has already died.
fn stream_writer(
    mut stream: UnixStream,
    frame_rx: std::sync::mpsc::Receiver<StreamFrame>,
    streamed_through: Arc<AtomicU64>,
) {
    while let Ok(frame) = frame_rx.recv() {
        // Capture the seq before the frame is moved into the writer.
        let seq = match &frame {
            StreamFrame::Envelope { envelope } => envelope.hub_seq,
            _ => None,
        };
        if write_stream_frame(&mut stream, &frame).is_err() {
            break;
        }
        if stream.flush().is_err() {
            break;
        }
        if let Some(seq) = seq {
            streamed_through.fetch_max(seq, Ordering::SeqCst);
        }
    }
}

/// [`McpDispatcher`] that proxies every JSON-RPC frame to the REPL and blocks
/// (off the runtime via `block_in_place`) until the REPL replies. RFC asks are
/// interactive, so there is no wall-clock timeout — the wait is bounded only by
/// the REPL replying or the subscriber being torn down (fail-closed).
struct ProxyMcpDispatcher {
    next: Arc<AtomicU64>,
    subscriber: Arc<Mutex<Option<StdSender<StreamFrame>>>>,
    pending: Arc<Mutex<HashMap<u64, SyncSender<McpReply>>>>,
}

impl McpDispatcher for ProxyMcpDispatcher {
    fn dispatch(&self, line: &str, peer_job_id: Option<JobId>) -> McpReply {
        let corr_id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = sync_channel::<McpReply>(1);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(corr_id, reply_tx);

        let frame = StreamFrame::McpProxy {
            corr_id,
            line: line.to_string(),
            peer_job_id: peer_job_id.map(|j| j.0),
        };
        let sent = {
            let guard = self.subscriber.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(sink) => sink.send(frame).is_ok(),
                None => false,
            }
        };
        if !sent {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&corr_id);
            return McpReply::plain(mcp_error_response(line, "orkia shell offline"));
        }

        // The reply (or a disconnect) arrives on `reply_rx`. `block_in_place`
        // tells tokio to offload the worker's other tasks while we block, so
        // the runtime (accept loop, other connections, the bus) stays live.
        match tokio::task::block_in_place(|| reply_rx.recv()) {
            Ok(reply) => reply,
            Err(_) => McpReply::plain(mcp_error_response(line, "orkia shell disconnected")),
        }
    }
}

/// Build a JSON-RPC error response carrying the request's `id` (so the agent's
/// client matches it), used when the REPL is unreachable (fail-closed, #8).
fn mcp_error_response(request_line: &str, message: &str) -> String {
    let id = serde_json::from_str::<serde_json::Value>(request_line)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32000, "message": message },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::journal::types::EventType;
    use std::io::{BufRead, BufReader};

    fn envelope_frame(hub_seq: Option<u64>) -> StreamFrame {
        let mut env = JournalEnvelope::now(EventType::Hook);
        env.hub_seq = hub_seq;
        StreamFrame::Envelope { envelope: env }
    }

    /// max `hub_seq` only after each frame is written AND flushed to the socket
    /// — proof the REPL received it. This is why the high-water lives in
    /// `stream_writer` and not in the forwarder (where `sink.send` succeeds even
    /// against a dead socket and would advance the cursor past unsent frames).
    #[test]
    fn stream_writer_advances_high_water_on_socket_write() {
        let (server, client) = UnixStream::pair().expect("socketpair");
        let (tx, rx) = std::sync::mpsc::channel::<StreamFrame>();
        let streamed_through = Arc::new(AtomicU64::new(10));
        let cursor = Arc::clone(&streamed_through);
        let writer = std::thread::spawn(move || stream_writer(server, rx, cursor));

        tx.send(envelope_frame(Some(11))).unwrap();
        tx.send(envelope_frame(Some(12))).unwrap();
        tx.send(envelope_frame(Some(13))).unwrap();

        // Drain the three NDJSON lines off the client end so the writes land.
        let mut reader = BufReader::new(client);
        for _ in 0..3 {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
        }
        // Close the channel so the writer exits, then join for a clean read.
        drop(tx);
        writer.join().expect("writer thread");

        assert_eq!(streamed_through.load(Ordering::SeqCst), 13);
    }

    /// A non-`Envelope` frame (`JobEvent`) is delivered but MUST NOT advance the
    /// high-water: only stamped journal envelopes participate in the SEAL replay
    /// backlog, so a job-event write between two envelopes can't skip the cursor.
    #[test]
    fn stream_writer_ignores_non_envelope_frames_for_high_water() {
        let (server, client) = UnixStream::pair().expect("socketpair");
        let (tx, rx) = std::sync::mpsc::channel::<StreamFrame>();
        let streamed_through = Arc::new(AtomicU64::new(5));
        let cursor = Arc::clone(&streamed_through);
        let writer = std::thread::spawn(move || stream_writer(server, rx, cursor));

        tx.send(StreamFrame::JobEvent {
            event: DaemonJobEvent {
                job_id: 1,
                event: "spawned".to_string(),
                kind: None,
                pid: None,
                exit_code: None,
                label: None,
            },
        })
        .unwrap();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        drop(tx);
        writer.join().expect("writer thread");

        // Cursor unchanged: a JobEvent is not journal backlog.
        assert_eq!(streamed_through.load(Ordering::SeqCst), 5);
    }

    /// An `Envelope` with no `hub_seq` (e.g. a REPL-local fallback emit that was
    /// never stamped by the daemon hub) is delivered but leaves the cursor put —
    /// `fetch_max` only runs for stamped frames.
    #[test]
    fn stream_writer_ignores_unstamped_envelopes_for_high_water() {
        let (server, client) = UnixStream::pair().expect("socketpair");
        let (tx, rx) = std::sync::mpsc::channel::<StreamFrame>();
        let streamed_through = Arc::new(AtomicU64::new(7));
        let cursor = Arc::clone(&streamed_through);
        let writer = std::thread::spawn(move || stream_writer(server, rx, cursor));

        tx.send(envelope_frame(None)).unwrap();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        drop(tx);
        writer.join().expect("writer thread");

        assert_eq!(streamed_through.load(Ordering::SeqCst), 7);
    }
}
