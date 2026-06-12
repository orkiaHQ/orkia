// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

//! Option B — "bin drives the pump").
//!
//! The daemon owns `orkia.sock` and hosts the journal hub. The REPL is a
//! subscriber: it cannot depend on this bin's IPC protocol, so the bin owns
//! the daemon connection and pumps the two flows across the single
//! full-duplex `Subscribe` stream, using the three handles the REPL exposes
//! ([`DaemonJournalHandles`]):
//!
//! - **reader thread** — decodes daemon→REPL [`StreamFrame`]s. `Envelope`
//!   frames are pushed to the REPL relay (`feed_tx`); `McpProxy` frames are
//!   dispatched against the REPL's real `McpShellDispatcher` (`mcp`) and the
//!   reply is queued for the writer.
//! - **emit-forwarder thread** — drains the REPL's in-process emits
//!   (`emit_rx`) and queues each as a `JournalEmit` for the writer.
//! - **writer thread** — the single owner of the write half, draining one
//!   `Request` channel fed by both the emit-forwarder and the reader's MCP
//!   replies (one writer per stream — no interleaved partial frames).

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

use orkia_shell::journal::McpReply;
use orkia_shell::repl::DaemonJournalHandles;
use orkia_shell_types::JobId;

use super::protocol::{DaemonJobEvent, Request, StreamFrame, send_request};

/// Wire the pump threads for an established, handshaken subscribe stream.
/// `reader` is the daemon→REPL frame stream (timeouts already cleared); the
/// write half is obtained by cloning the underlying socket so the reader and
/// writer never contend on one handle.
pub(super) fn spawn(reader: BufReader<UnixStream>, handles: DaemonJournalHandles) {
    let DaemonJournalHandles {
        feed_tx,
        mut emit_rx,
        mcp,
        job_event_tx,
    } = handles;

    let write_stream = match reader.get_ref().try_clone() {
        Ok(s) => s,
        Err(err) => {
            // Without a write half we can still receive (one-way degrade),
            // but in-process emits and MCP replies would never reach the
            // daemon. Surface and bail; the REPL keeps running, journaling
            // is best-effort (#1 — never block the loop on this).
            eprintln!("orkia: journal pump: clone write half failed: {err}");
            return;
        }
    };

    // Single `Request` channel feeding the one writer thread.
    let (req_tx, req_rx) = mpsc::channel::<Request>();

    // Writer thread: the sole owner of the write half.
    let mut write_stream_owned = write_stream;
    std::thread::Builder::new()
        .name("orkia-journal-pump-writer".to_string())
        .spawn(move || {
            while let Ok(req) = req_rx.recv() {
                if send_request(&mut write_stream_owned, &req).is_err() {
                    break; // daemon gone — stop writing.
                }
            }
        })
        .ok();

    // Emit-forwarder thread: REPL in-process emits → daemon `JournalEmit`.
    let emit_req_tx = req_tx.clone();
    std::thread::Builder::new()
        .name("orkia-journal-pump-emit".to_string())
        .spawn(move || {
            while let Some(envelope) = emit_rx.blocking_recv() {
                if emit_req_tx.send(Request::JournalEmit { envelope }).is_err() {
                    break; // writer gone.
                }
            }
        })
        .ok();

    // Reader thread: daemon→REPL frames.
    let reader_handles = ReaderHandles {
        feed_tx,
        mcp,
        req_tx,
        job_event_tx,
    };
    std::thread::Builder::new()
        .name("orkia-journal-pump-reader".to_string())
        .spawn(move || reader_loop(reader, reader_handles))
        .ok();
}

/// The four REPL-side sinks the reader thread fans daemon frames onto. Bundled
/// so `reader_loop` stays under the 4-argument limit (CLAUDE.md).
struct ReaderHandles {
    feed_tx: tokio::sync::mpsc::UnboundedSender<orkia_shell::journal::JournalEnvelope>,
    mcp: std::sync::Arc<dyn orkia_shell::journal::McpDispatcher>,
    req_tx: mpsc::Sender<Request>,
    job_event_tx: tokio::sync::mpsc::UnboundedSender<orkia_shell_types::job::JobEvent>,
}

fn reader_loop(mut reader: BufReader<UnixStream>, handles: ReaderHandles) {
    let ReaderHandles {
        feed_tx,
        mcp,
        req_tx,
        job_event_tx,
    } = handles;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return, // daemon closed the stream.
            Ok(_) => {}
            Err(_) => return, // read error — treat as disconnect.
        }
        if line.trim().is_empty() {
            continue;
        }
        // Every byte from the daemon is untrusted (#7): a malformed frame is
        // skipped, never fatal.
        let frame: StreamFrame = match serde_json::from_str(&line) {
            Ok(frame) => frame,
            Err(_) => continue,
        };
        match frame {
            StreamFrame::Envelope { envelope } => {
                if feed_tx.send(envelope).is_err() {
                    return; // REPL relay gone — stop pumping.
                }
            }
            StreamFrame::McpProxy {
                corr_id,
                line: rpc,
                peer_job_id,
            } => {
                let reply: McpReply = mcp.dispatch(&rpc, peer_job_id.map(JobId));
                if req_tx
                    .send(Request::McpProxyReply {
                        corr_id,
                        response: reply.response,
                        accessed_node_ids: reply.accessed_node_ids,
                    })
                    .is_err()
                {
                    return; // writer gone.
                }
            }
            // 3c: map the daemon-pushed projection back onto the REPL's
            // in-process `JobEvent` and inject it. Unmappable tags (e.g.
            // `spawned`, see `reconstruct`) and future kinds are skipped (#7).
            StreamFrame::JobEvent { event } => {
                if let Some(reconstructed) = reconstruct(event)
                    && job_event_tx.send(reconstructed).is_err()
                {
                    return; // REPL gone — stop pumping.
                }
            }
        }
    }
}

/// Map a daemon-pushed [`DaemonJobEvent`] back onto the REPL's in-process
/// `JobEvent`. Returns `None` for tags the REPL cannot faithfully reconstruct:
///
/// - `spawned` carries only a `JobKind` *tag* on the wire; rebuilding
///   `JobKind::Agent` would require fabricating a nil agent UUID (a fake SEAL
///   identity, #8). The main REPL surfaces detached spawns via the daemon
///   `list` merge instead, so a pushed `spawned` is intentionally dropped.
/// - unknown / future tags are tolerated and skipped (#7).
///
/// `exit_code` defaults to `0` when a `completed` frame omits it (the
/// projection always sets it, but the wire field is optional).
fn reconstruct(event: DaemonJobEvent) -> Option<orkia_shell_types::job::JobEvent> {
    use orkia_shell_types::job::{JobEvent, JobId};
    let id = JobId(event.job_id);
    let label = event.label.unwrap_or_default();
    match event.event.as_str() {
        "completed" => Some(JobEvent::Completed {
            id,
            exit_code: event.exit_code.unwrap_or(0),
            label,
        }),
        "stopped" => Some(JobEvent::Stopped { id, label }),
        "continued" => Some(JobEvent::Continued { id, label }),
        "attached" => Some(JobEvent::Attached { id }),
        "detached" => Some(JobEvent::Detached { id }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::job::JobEvent;

    fn wire(event: &str) -> DaemonJobEvent {
        DaemonJobEvent {
            job_id: 5,
            event: event.to_string(),
            kind: None,
            pid: None,
            exit_code: None,
            label: Some("agent:sage".to_string()),
        }
    }

    #[test]
    fn reconstructs_completed_with_exit_and_label() {
        let mut w = wire("completed");
        w.exit_code = Some(3);
        match reconstruct(w) {
            Some(JobEvent::Completed {
                id,
                exit_code,
                label,
            }) => {
                assert_eq!(id.0, 5);
                assert_eq!(exit_code, 3);
                assert_eq!(label, "agent:sage");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn completed_defaults_exit_code_to_zero_when_absent() {
        match reconstruct(wire("completed")) {
            Some(JobEvent::Completed { exit_code, .. }) => assert_eq!(exit_code, 0),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn reconstructs_attached_and_detached() {
        assert!(matches!(
            reconstruct(wire("attached")),
            Some(JobEvent::Attached { .. })
        ));
        assert!(matches!(
            reconstruct(wire("detached")),
            Some(JobEvent::Detached { .. })
        ));
    }

    #[test]
    fn spawned_and_unknown_tags_are_dropped() {
        // `spawned` is intentionally unmappable (would fabricate an identity).
        assert!(reconstruct(wire("spawned")).is_none());
        // Forward-compat: an unknown tag is tolerated, not fatal (#7).
        assert!(reconstruct(wire("teleported")).is_none());
    }
}
