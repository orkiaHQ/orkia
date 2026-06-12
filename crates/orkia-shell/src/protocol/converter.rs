// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Routing + dedup for the unified [`OrkiaEvent`] stream.
//!
//! Sources call `EventRouter::on_*` helpers; the router formats an
//! [`OrkiaEvent`], applies dedup by `(job_id, tag)` within a 2 s
//! window using [`EventSource::priority`], and forwards survivors
//! down a single `tokio::sync::mpsc::UnboundedSender`. Consumers
//! pick up the receiver via [`EventRouter::take_rx`].
//!
//! The router is **cheap to clone** — it holds an `Arc` over the
//! shared state — so the REPL can hand `EventRouter` handles to the
//! state-machine worker thread, the journal listener, and any
//! per-agent BlockParser callback without coordination.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orkia_shell_types::JobId;
use parking_lot::Mutex;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::{EventPayload, EventSource, OrkiaEvent};

/// Re-export so the REPL can refer to the marker type through the
/// protocol module without depending on `orkia-terminal-core`
/// directly. The canonical definition lives in
/// `orkia-terminal-core::blocks` because `BlockParser` is where the
/// markers are recognised.
pub use orkia_terminal_core::Osc133Marker;

pub struct FanoutConfig {
    pub job_scopes: orkia_kernel::JobScopes,
    pub outputs: Vec<UnboundedSender<OrkiaEvent>>,
}

/// Single-owner fanout for the unified event stream. The router keeps one
/// receiver; this task clones each event to downstream consumers and stamps
/// hook/tool events with the REPL-owned job RFC scope when available.
pub fn spawn_fanout(
    mut input: UnboundedReceiver<OrkiaEvent>,
    cfg: FanoutConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(mut event) = input.recv().await {
            if event.rfc_id.is_none() {
                let scope = orkia_kernel::scope_for(&cfg.job_scopes, event.job_id.0);
                event.rfc_id = scope.rfc_ref.map(|r| r.rfc_id);
            }
            for tx in &cfg.outputs {
                let _ = tx.send(event.clone());
            }
        }
    })
}

/// How long an earlier higher-priority event blocks later
/// lower-priority events with the same `(job_id, tag)`. 2 s comes
/// from real-world latencies: claude hooks land 200-500 ms after
/// the matching OSC 133 marker, and the state machine's tick is
/// 500 ms; 2 s covers both with margin.
const DEDUP_WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct EventRouter {
    tx: UnboundedSender<OrkiaEvent>,
    rx: Arc<Mutex<Option<UnboundedReceiver<OrkiaEvent>>>>,
    dedup: Arc<Mutex<DedupState>>,
}

struct DedupState {
    /// Most-recently emitted `(source priority, instant)` per
    /// `(job_id, tag)`. We compare priority on insert; if the
    /// incoming event has *strictly lower* priority than the
    /// recorded one (and is still within the window), we drop it.
    recent: HashMap<(JobId, &'static str), (u8, Instant)>,
}

impl Default for EventRouter {
    fn default() -> Self {
        Self::new()
    }
}

// Bridges the concrete router into the process-agnostic journal hub
// (`orkia-journal-hub`), which holds it as `Arc<dyn HookRouter>` so it can
// run in either the REPL or the pty-daemon without depending on this crate.
impl crate::journal::HookRouter for EventRouter {
    fn route_hook(&self, env: &crate::journal::JournalEnvelope) -> bool {
        self.on_hook(env)
    }
}

impl EventRouter {
    pub fn new() -> Self {
        let (router, _rx) = Self::new_with_rx();
        // Re-park the receiver so first take_rx() succeeds, preserving
        // the legacy single-consumer contract.
        *router.rx.lock() = Some(_rx);
        router
    }

    /// Construct a router together with its single consumer-side
    /// receiver. Preferred over `new()` + `take_rx()` because the
    /// receiver is non-Optional at the type level: there is no
    /// runtime invariant to assert at the call site.
    pub fn new_with_rx() -> (Self, UnboundedReceiver<OrkiaEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let router = Self {
            tx,
            rx: Arc::new(Mutex::new(None)),
            dedup: Arc::new(Mutex::new(DedupState {
                recent: HashMap::new(),
            })),
        };
        (router, rx)
    }

    /// Take the consumer-side receiver exactly once. Subsequent
    /// calls return `None`. The REPL holds this until it stands up
    /// downstream consumers (Surface app, etc.).
    pub fn take_rx(&self) -> Option<UnboundedReceiver<OrkiaEvent>> {
        self.rx.lock().take()
    }

    /// Source 1: OSC 133 marker observed in the byte stream
    /// (callback from `BlockParser`).
    pub fn on_osc133(&self, job_id: JobId, agent_name: &str, marker: Osc133Marker) {
        let payload = match marker {
            Osc133Marker::PromptStart => EventPayload::PromptStart,
            Osc133Marker::PromptReady => EventPayload::PromptReady,
            Osc133Marker::OutputStart => EventPayload::OutputStart,
            Osc133Marker::OutputFinished { exit_code } => {
                EventPayload::OutputFinished { exit_code }
            }
        };
        self.emit(EventSource::Osc133, job_id, agent_name, payload, 1.0);
    }

    /// Source 2: hook journal envelope arrived via `orkia bridge`.
    /// Returns `true` when the envelope produced an event (i.e. it
    /// was a `Hook` envelope we know how to convert).
    pub fn on_hook(&self, envelope: &crate::journal::JournalEnvelope) -> bool {
        self.on_hook_with_rfc(envelope, None)
    }

    /// Hook ingestion with REPL-resolved RFC attribution. The hook JSON itself
    /// is provider-owned and does not carry Orkia's active RFC scope.
    pub fn on_hook_with_rfc(
        &self,
        envelope: &crate::journal::JournalEnvelope,
        rfc_id: Option<orkia_rfc_core::RfcId>,
    ) -> bool {
        if let Some(event) = super::hooks::convert_hook_with_rfc(envelope, rfc_id) {
            self.send_if_not_deduped(event);
            true
        } else {
            false
        }
    }

    /// Source 3: state-machine detector event.
    pub fn on_state_machine(
        &self,
        event: &crate::terminal_state::DetectorEvent,
        agent_name: &str,
    ) -> bool {
        if let Some(e) = super::convert_detector_event(event, agent_name) {
            self.send_if_not_deduped(e);
            true
        } else {
            false
        }
    }

    /// Source 4 (V2): native APC protocol payload.
    pub fn on_orkia_protocol(&self, job_id: JobId, agent_name: &str, payload: EventPayload) {
        self.emit(EventSource::OrkiaProtocol, job_id, agent_name, payload, 1.0);
    }

    /// Source 5: REPL-internal emission. Used for events that don't
    /// originate from a stream parser (RFC mutations, `agent.spawn`
    /// metadata, approve/deny outcomes, tells, …) but still need to
    /// flow through the unified channel so consumers like
    /// `SealManager` see them. Bypasses dedup — all Custom payloads
    /// share the `"custom"` tag, so the standard window-based
    /// suppression would incorrectly collapse unrelated events.
    pub fn on_custom(&self, job_id: JobId, agent_name: &str, name: &str, data: serde_json::Value) {
        self.on_custom_with_rfc(job_id, agent_name, name, data, None);
    }

    /// Like [`Self::on_custom`] but stamps the event with an RFC id so
    /// the SEAL consumer can route it into that RFC's audit slice. The
    /// REPL calls this whenever it has an active RFC scope
    /// (`rfc cd <slug>`); other sources pass `None`.
    //
    // 5 args is over the project's 4-arg limit, but this is a thin
    // dispatcher into the message channel — turning the args into a
    // builder would add allocation per emit without buying clarity.
    #[allow(clippy::too_many_arguments)]
    pub fn on_custom_with_rfc(
        &self,
        job_id: JobId,
        agent_name: &str,
        name: &str,
        data: serde_json::Value,
        rfc_id: Option<orkia_rfc_core::RfcId>,
    ) {
        let evt = OrkiaEvent {
            source: EventSource::Internal,
            event: EventPayload::Custom {
                name: name.to_string(),
                data,
            },
            confidence: 1.0,
            timestamp: chrono::Utc::now(),
            job_id,
            agent_name: agent_name.to_string(),
            rfc_id,
        };
        let _ = self.tx.send(evt);
    }

    fn emit(
        &self,
        source: EventSource,
        job_id: JobId,
        agent_name: &str,
        event: EventPayload,
        confidence: f32,
    ) {
        let evt = OrkiaEvent {
            source,
            event,
            confidence,
            timestamp: chrono::Utc::now(),
            job_id,
            agent_name: agent_name.to_string(),
            // Non-Custom events come from external sources (OSC133,
            // hooks, state machine) that don't carry the REPL's RFC
            // scope. Always None here; REPL-stamped events use
            // `on_custom_with_rfc` instead.
            rfc_id: None,
        };
        self.send_if_not_deduped(evt);
    }

    fn send_if_not_deduped(&self, evt: OrkiaEvent) {
        let key = (evt.job_id, evt.event.tag());
        let prio = evt.source.priority();
        let now = Instant::now();
        let mut dedup = self.dedup.lock();
        // GC entries older than the window so the map stays bounded.
        dedup
            .recent
            .retain(|_, (_, when)| now.duration_since(*when) < DEDUP_WINDOW);
        if let Some((seen_prio, when)) = dedup.recent.get(&key)
            && now.duration_since(*when) < DEDUP_WINDOW
            && *seen_prio > prio
        {
            // A higher-priority source already reported the same
            // semantic event recently. Suppress.
            tracing::trace!(
                ?evt.source, tag = evt.event.tag(),
                "EventRouter: dropping lower-priority duplicate",
            );
            return;
        }
        dedup.recent.insert(key, (prio, now));
        drop(dedup);
        if self.tx.send(evt).is_err() {
            tracing::debug!("EventRouter: receiver dropped, event discarded");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_kernel::{JobScope, new_job_scopes};
    use orkia_reasoning_core::dto::RfcRef;
    use orkia_rfc_core::RfcId;

    fn job(n: u32) -> JobId {
        JobId(n)
    }

    #[test]
    fn two_independent_events_both_pass() {
        let router = EventRouter::new();
        let mut rx = router.take_rx().expect("rx");
        router.on_osc133(job(1), "faye", Osc133Marker::PromptReady);
        router.on_osc133(job(1), "faye", Osc133Marker::OutputStart);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn lower_priority_dup_within_window_is_suppressed() {
        let router = EventRouter::new();
        let mut rx = router.take_rx().expect("rx");
        // High-priority OSC 133 PromptReady first.
        router.on_osc133(job(1), "faye", Osc133Marker::PromptReady);
        // State-machine event with same job + same tag arriving
        // 50 ms later should be dropped.
        let det =
            crate::terminal_state::DetectorEvent::Attention(crate::terminal_state::JobAttention {
                job_id: job(1),
                agent_name: "faye".into(),
                confidence: 0.7,
                prompt_type: crate::terminal_state::PromptType::ShellPrompt,
                last_line: String::new(),
                has_pending_body: false,
                pending_body_preview: None,
            });
        router.on_state_machine(&det, "faye");
        let first = rx.try_recv().expect("first");
        assert!(matches!(first.source, EventSource::Osc133));
        // Second receive should fail — the state-machine
        // PromptReady was deduped against the OSC 133 one.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn higher_priority_after_lower_still_passes() {
        let router = EventRouter::new();
        let mut rx = router.take_rx().expect("rx");
        let det =
            crate::terminal_state::DetectorEvent::Attention(crate::terminal_state::JobAttention {
                job_id: job(1),
                agent_name: "faye".into(),
                confidence: 0.7,
                prompt_type: crate::terminal_state::PromptType::ShellPrompt,
                last_line: String::new(),
                has_pending_body: false,
                pending_body_preview: None,
            });
        router.on_state_machine(&det, "faye");
        // Higher-priority OSC 133 same tag right after — not
        // suppressed (higher overwrites lower).
        router.on_osc133(job(1), "faye", Osc133Marker::PromptReady);
        let _first = rx.try_recv().expect("state machine event");
        let _second = rx.try_recv().expect("osc 133 event");
    }

    #[test]
    fn different_jobs_do_not_dedup_each_other() {
        let router = EventRouter::new();
        let mut rx = router.take_rx().expect("rx");
        router.on_osc133(job(1), "faye", Osc133Marker::PromptReady);
        router.on_osc133(job(2), "sage", Osc133Marker::PromptReady);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn fanout_stamps_rfc_scope_and_delivers_to_all_outputs() {
        let scopes = new_job_scopes();
        scopes.write().expect("scopes").insert(
            7,
            JobScope {
                project_id: None,
                rfc_ref: Some(RfcRef::new(RfcId::new("operator-v1"))),
            },
        );
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (seal_tx, mut seal_rx) = mpsc::unbounded_channel();
        let (operator_tx, mut operator_rx) = mpsc::unbounded_channel();

        let handle = spawn_fanout(
            input_rx,
            FanoutConfig {
                job_scopes: scopes,
                outputs: vec![seal_tx, operator_tx],
            },
        );
        input_tx
            .send(OrkiaEvent {
                source: EventSource::Hook {
                    provider: "claude".to_string(),
                },
                event: EventPayload::ToolUse {
                    tool: "write_file".to_string(),
                    target: Some("orkia/crates/orkia-shell/src/operator.rs".to_string()),
                    input_summary: None,
                },
                confidence: 1.0,
                timestamp: chrono::Utc::now(),
                job_id: job(7),
                agent_name: "faye".to_string(),
                rfc_id: None,
            })
            .expect("send event");
        drop(input_tx);

        let seal_event = seal_rx.recv().await.expect("seal event");
        let operator_event = operator_rx.recv().await.expect("operator event");
        assert_eq!(seal_event.rfc_id, Some(RfcId::new("operator-v1")));
        assert_eq!(operator_event.rfc_id, Some(RfcId::new("operator-v1")));
        handle.await.expect("fanout exits");
    }
}
