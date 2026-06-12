// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Process-agnostic journal hub.
//!
//! [`JournalHub`] owns the listener construction, the broadcast bus, and
//! the disk-backed subscribers (FinalResponseService stop-hook + disk tee)
//! without requiring any reference to the REPL (`orkia_shell::repl::Repl`).
//!
//! The REPL-resident subscribers (knowledge-activity, stream-publisher,
//! intelligence) still live in `repl/journal.rs` because they carry REPL
//! fields (auth, job_scopes, handles). They obtain a bus receiver via
//! [`JournalHub::subscribe`] after hub construction.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender as StdSender;

use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::JournalHubError;
use crate::listener::{JournalListener, LiveJournalHandlers};
use orkia_shell_types::JournalStopHook;
use orkia_shell_types::journal::JournalEnvelope;

/// Everything the hub needs to construct the listener and the
/// disk-backed subscribers — no `Repl` reference required.
pub struct JournalHubConfig {
    /// Data directory passed to [`JournalListener::start_with_channel`].
    pub data_dir: PathBuf,
    /// Explicit socket path to bind instead of the `<data_dir>/run/orkia.sock`
    /// default. `None` keeps the default (every existing caller). `Some` is
    /// "LPH"), where the runtime binds a per-job socket so it can host and
    /// consume its own agent's hooks without moving `data_dir`.
    pub socket_path_override: Option<PathBuf>,
    /// Pre-built handler set (router, printer, attach_active, mcp,
    /// stop_hook, envelope_hook). The `stop_hook` field is intentionally
    /// left `None` here; the hub wires it through the bus subscriber
    /// instead (same pattern as `boot_journal` today).
    pub handlers: LiveJournalHandlers,
    /// The mpsc receiver end that the REPL drains. The hub hands the
    /// matching `tx` to the listener so all socket + in-process envelopes
    /// flow through the same channel the REPL already owns.
    pub outbound_tx: UnboundedSender<JournalEnvelope>,
    /// Pre-constructed FinalResponseService (as a `JournalStopHook`).
    /// `None` means FRS was already installed by the caller or is not
    /// needed.
    pub stop_hook: Option<Arc<dyn JournalStopHook>>,
    /// Clone of the journal-store writer channel. When `Some`, the hub
    /// spawns a disk-tee task that persists every envelope to
    /// `journal.jsonl` independent of the REPL drain cadence.
    pub disk_writer: Option<StdSender<String>>,
    /// listener stamps every envelope with a monotonic `hub_seq` from `n + 1`
    /// (the daemon-resident hub passes the on-disk max so the sequence survives
    /// a daemon restart). `None` ⇒ no stamping — the REPL-local daemon-less
    /// fallback and the per-job LPH, which are single-process and have no
    /// resubscribe gap to close.
    pub seq_seed: Option<u64>,
}

/// Owns the journal listener + the disk-backed subscriber tasks.
///
/// Call [`JournalHub::start`] to bind the socket and spawn the
/// internal tasks, then call [`JournalHub::subscribe`] /
/// [`JournalHub::sender`] to wire the REPL-resident subscribers,
/// and finally [`JournalHub::into_listener`] to hand ownership of
/// the raw listener back to the REPL (for `journal_listener` field
/// storage and `reboot_intelligence`).
pub struct JournalHub {
    listener: JournalListener,
}

impl JournalHub {
    /// Bind the Unix socket, spawn the fanout task inside
    /// [`JournalListener`], then spawn the disk-tee and stop-hook
    /// subscriber tasks. Returns `Err` only if the socket bind fails
    /// (matches the existing `JournalListener::start_with_channel` error
    /// surface).
    pub fn start(cfg: JournalHubConfig) -> Result<Self, JournalHubError> {
        let listener = match cfg.socket_path_override {
            Some(path) => {
                JournalListener::start_at_seeded(path, cfg.handlers, cfg.outbound_tx, cfg.seq_seed)?
            }
            None => JournalListener::start_with_channel_seeded(
                &cfg.data_dir,
                cfg.handlers,
                cfg.outbound_tx,
                cfg.seq_seed,
            )?,
        };
        spawn_stop_hook_subscriber(&listener, cfg.stop_hook);
        spawn_disk_tee(&listener, cfg.disk_writer);
        Ok(Self { listener })
    }

    /// Relay variant for subscribed mode (MIGRATE-AGENT-SPAWN-TO-DAEMON
    /// from the daemon stream. Deliberately spawns NO disk-tee and NO
    /// stop-hook subscriber: those movable disk-backed consumers (journal
    /// mirror, FinalResponseService) run in the daemon-hosted hub so capture
    /// survives a REPL restart. The REPL-resident subscribers (knowledge,
    /// stream, intelligence) still attach via [`Self::subscribe`].
    ///
    /// Returns the hub plus the `feed_tx` the bin pump pushes
    /// daemon-streamed envelopes into.
    pub fn start_relay(
        handlers: LiveJournalHandlers,
        outbound_tx: UnboundedSender<JournalEnvelope>,
    ) -> (Self, UnboundedSender<JournalEnvelope>) {
        let (listener, feed_tx) = JournalListener::start_relay(handlers, outbound_tx);
        (Self { listener }, feed_tx)
    }

    /// Clone the in-process sender so callers can emit envelopes without
    /// going through the socket.
    pub fn sender(&self) -> UnboundedSender<JournalEnvelope> {
        self.listener.sender()
    }

    /// Subscribe to the broadcast bus. Every parsed [`JournalEnvelope`]
    /// — whether from the Unix socket or via the in-process sender — is
    /// fanned out to every subscriber.
    pub fn subscribe(&self) -> broadcast::Receiver<JournalEnvelope> {
        self.listener.subscribe()
    }

    /// Consume the hub and return the underlying listener. Used by the
    /// REPL to store it in `journal_listener` and later pass it to
    /// `reboot_intelligence`.
    pub fn into_listener(self) -> JournalListener {
        self.listener
    }
}

// ── internal helpers ──────────────────────────────────────────────────────────

/// Bus-subscribe the stop-hook so it fires independent of the REPL
/// drain cadence. Mirrors `Repl::spawn_stop_hook_subscriber` exactly.
fn spawn_stop_hook_subscriber(listener: &JournalListener, hook: Option<Arc<dyn JournalStopHook>>) {
    let Some(hook) = hook else {
        return;
    };
    let mut bus_rx = listener.subscribe();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match bus_rx.recv().await {
                Ok(env) => hook.on_stop(&env),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// Forward every parsed envelope to the journal-disk writer thread
/// immediately, independent of the REPL's drain cadence.
/// Mirrors `Repl::spawn_disk_tee` exactly.
fn spawn_disk_tee(listener: &JournalListener, disk_tx: Option<StdSender<String>>) {
    let Some(disk_tx) = disk_tx else {
        return;
    };
    let mut bus_rx = listener.subscribe();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match bus_rx.recv().await {
                Ok(env) => match serde_json::to_string(&env) {
                    Ok(line) => {
                        if disk_tx.send(line).is_err() {
                            break; // writer thread gone — store dropped.
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "journal disk tee: serialize failed; dropping",
                        );
                    }
                },
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(
                        skipped = n,
                        "journal disk tee: subscriber lagged; some envelopes will be missing from journal.jsonl",
                    );
                    continue;
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}
