// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

//! Detached-runtime → daemon `JobEvent` forwarder (MIGRATE-AGENT-SPAWN-TO-DAEMON
//!
//! A detached `orkia` runtime owns its agent's in-process `JobController`, which
//! emits the real lifecycle/terminal `JobEvent`s. The main REPL no longer sees
//! those directly (the agent PTY lives under the daemon). So the runtime installs
//! a [`JobEventObserver`] that projects each event onto the wire
//! [`DaemonJobEvent`] — stamped with the DAEMON's job id (`ORKIA_DETACHED_JOB_ID`),
//! not the runtime-local id — and pushes it up to the daemon, which relays it to
//! the main REPL subscriber as a `StreamFrame::JobEvent`.
//!
//! The socket send is blocking I/O, so it runs on a dedicated thread fed by an
//! unbounded channel: `on_job_event` only does a non-blocking channel send, never
//! blocking the runtime's REPL drain (#1).

use std::sync::Arc;

use orkia_shell::ShellConfig;
use orkia_shell::journal::JournalEnvelope;
use orkia_shell_types::JobEventObserver;
use orkia_shell_types::JournalEnvelopeHook;
use orkia_shell_types::job::JobEvent;

use super::protocol::DaemonJobEvent;

struct DetachedJobForwarder {
    /// Non-blocking handoff to the forward thread (it owns the socket I/O).
    tx: std::sync::mpsc::Sender<DaemonJobEvent>,
    /// The daemon's job id for this runtime (every event is stamped with it so
    /// the main REPL keys it the same way `List` does).
    daemon_job_id: u32,
    /// Events handed off but not yet sent over the socket. Incremented in
    /// `on_job_event`, decremented by the forward thread after the send.
    /// `flush_pending` waits on this so the runtime's teardown exit does not
    /// race the forward thread mid-send (which would drop the terminal
    /// `Completed` → no `[1] done` on the main REPL).
    pending: Arc<std::sync::atomic::AtomicUsize>,
}

/// Build the forwarder iff this process is a detached runtime (i.e.
/// `ORKIA_DETACHED_JOB_ID` is set to a `u32`). `None` for the main REPL, which
/// installs no observer. Spawns the forward thread on success.
pub(crate) fn observer(config: &ShellConfig) -> Option<Arc<dyn JobEventObserver>> {
    let daemon_job_id: u32 = std::env::var("ORKIA_DETACHED_JOB_ID").ok()?.parse().ok()?;
    let (tx, rx) = std::sync::mpsc::channel::<DaemonJobEvent>();
    let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pending_thread = Arc::clone(&pending);
    let cfg = config.clone();
    std::thread::Builder::new()
        .name("orkia-job-forward".to_string())
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                super::client_api::forward_job_event(&cfg, event);
                pending_thread.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        })
        .ok()?;
    Some(Arc::new(DetachedJobForwarder {
        tx,
        daemon_job_id,
        pending,
    }))
}

/// Forwards a detached runtime's `AgentFinalResponse` journal envelope up to the
/// agent's turn. The runtime consumes its OWN hooks locally (teardown / SEAL /
/// FRS capture under the LPH); only the final-response envelope is forwarded —
/// NOT every hook (forwarding all would re-create the subscriber-slot/double-seal
/// problem the LPH model avoids). Non-blocking: the listener-bus callback only
/// does a channel send; the socket I/O runs on the forward thread.
struct DetachedAfrForwarder {
    tx: std::sync::mpsc::Sender<JournalEnvelope>,
}

/// Build the AFR forwarder iff this process is a detached runtime. `None` for the
/// main REPL. Spawns the forward thread on success. Mirrors [`observer`].
pub(crate) fn afr_forwarder(config: &ShellConfig) -> Option<Arc<dyn JournalEnvelopeHook>> {
    let _: u32 = std::env::var("ORKIA_DETACHED_JOB_ID").ok()?.parse().ok()?;
    let (tx, rx) = std::sync::mpsc::channel::<JournalEnvelope>();
    let cfg = config.clone();
    std::thread::Builder::new()
        .name("orkia-afr-forward".to_string())
        .spawn(move || {
            while let Ok(envelope) = rx.recv() {
                super::client_api::forward_journal_envelope(&cfg, envelope);
            }
        })
        .ok()?;
    Some(Arc::new(DetachedAfrForwarder { tx }))
}

impl JournalEnvelopeHook for DetachedAfrForwarder {
    fn on_envelope(&self, env: &JournalEnvelope) {
        // Forward ONLY the final-response envelope; the runtime keeps every other
        // envelope (hooks, lifecycle) local to its LPH. Dropped send is non-fatal.
        if env.event.as_deref() == Some("AgentFinalResponse") {
            let _ = self.tx.send(env.clone());
        }
    }
}

impl JobEventObserver for DetachedJobForwarder {
    fn on_job_event(&self, event: &JobEvent) {
        use std::sync::atomic::Ordering;
        // The state-machine SIGCHLD handler emits a sentinel
        // `Detached { id: JobId(0) }` purely to wake the runtime's OWN drain
        // loop (a local reap nudge). It is not a real lifecycle event, and
        // `project` would remap its id to this runtime's daemon job id — so
        // forwarding it floods the main REPL with spurious `[1] detached`
        // renders. Drop the sentinel; forward only real job events.
        if event.job_id().0 == 0 {
            return;
        }
        self.pending.fetch_add(1, Ordering::SeqCst);
        // Dropped send (forward thread gone) is non-fatal — the main REPL
        // re-derives state from `List`. Undo the count so `flush_pending`
        // does not wait on an event that will never be sent.
        if self.tx.send(project(event, self.daemon_job_id)).is_err() {
            self.pending.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn flush_pending(&self, timeout: std::time::Duration) {
        use std::sync::atomic::Ordering;
        let start = std::time::Instant::now();
        while self.pending.load(Ordering::SeqCst) > 0 {
            if start.elapsed() >= timeout {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Project a runtime `JobEvent` onto the wire frame, remapping the job id to the
/// daemon's id. Tags match `DaemonJobEvent`'s documented vocabulary.
fn project(event: &JobEvent, daemon_job_id: u32) -> DaemonJobEvent {
    let mut out = DaemonJobEvent {
        job_id: daemon_job_id,
        event: String::new(),
        kind: None,
        pid: None,
        exit_code: None,
        label: None,
    };
    match event {
        JobEvent::Spawned { kind, pid, .. } => {
            out.event = "spawned".into();
            out.kind = Some(kind.to_string());
            out.pid = *pid;
            out.label = Some(kind.to_string());
        }
        JobEvent::Attached { .. } => out.event = "attached".into(),
        JobEvent::Detached { .. } => out.event = "detached".into(),
        JobEvent::Stopped { label, .. } => {
            out.event = "stopped".into();
            out.label = Some(label.clone());
        }
        JobEvent::Continued { label, .. } => {
            out.event = "continued".into();
            out.label = Some(label.clone());
        }
        JobEvent::Completed {
            exit_code, label, ..
        } => {
            out.event = "completed".into();
            out.exit_code = Some(*exit_code);
            out.label = Some(label.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::job::{JobId, JobKind};

    #[test]
    fn projects_completed_with_daemon_job_id() {
        let ev = JobEvent::Completed {
            id: JobId(1),
            exit_code: 0,
            label: "agent:sage".into(),
        };
        let out = project(&ev, 7);
        assert_eq!(out.job_id, 7); // remapped to the daemon id, not the local 1
        assert_eq!(out.event, "completed");
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.label.as_deref(), Some("agent:sage"));
        assert_eq!(out.pid, None);
    }

    #[test]
    fn projects_spawned_with_kind_and_pid() {
        let ev = JobEvent::Spawned {
            id: JobId(1),
            kind: JobKind::Shell { cmd: "make".into() },
            pid: Some(4242),
        };
        let out = project(&ev, 3);
        assert_eq!(out.job_id, 3);
        assert_eq!(out.event, "spawned");
        assert_eq!(out.kind.as_deref(), Some("make"));
        assert_eq!(out.pid, Some(4242));
    }

    #[test]
    fn afr_forwarder_forwards_only_final_response_envelopes() {
        use orkia_shell::journal::{EventType, JournalEnvelope};
        let (tx, rx) = std::sync::mpsc::channel::<JournalEnvelope>();
        let fwd = DetachedAfrForwarder { tx };

        // A non-AFR hook is kept local (not forwarded).
        let mut other = JournalEnvelope::now(EventType::Hook);
        other.event = Some("PermissionRequest".into());
        fwd.on_envelope(&other);
        assert!(rx.try_recv().is_err(), "non-AFR envelope must not forward");

        // The final-response envelope is forwarded.
        let mut afr = JournalEnvelope::now(EventType::Hook);
        afr.event = Some("AgentFinalResponse".into());
        fwd.on_envelope(&afr);
        let got = rx.try_recv().expect("AFR envelope must forward");
        assert_eq!(got.event.as_deref(), Some("AgentFinalResponse"));
    }
}
