// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `FinalResponseService` — entry point for the journal listener.
//!
//! On every Stop envelope from a known provider, the listener calls
//! [`FinalResponseService::on_stop`]. That method spawns an extraction
//! task on the current Tokio runtime and returns immediately, so the
//! listener loop never blocks on transcript I/O.
//!
//! The task:
//!
//! 1. Looks up the extractor for the provider.
//! 2. Reads the provider's transcript and pulls the final assistant text.
//! 3. Persists the text under the run-dir (see `storage`).
//! 4. Builds an `AgentFinalResponse` `JournalEnvelope` and sends it via
//!    the channel handed to the service at construction.
//! 5. Updates the per-job LRU and fires every subscriber.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use orkia_shell_types::{
    EventType, FinalResponseCallback, FinalResponseEvent, FinalResponseSource, JournalEnvelope,
    JournalStopHook, ProviderId,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::extractor::{ExtractionContext, ExtractionError, TranscriptExtractor};
use crate::extractors::{ClaudeExtractor, CodexExtractor, GeminiExtractor};
use crate::storage::{self, EMPTY_TURN_PREVIEW};

/// Maximum number of jobs whose latest event we remember in-memory for
/// `latest_for_job`. Beyond this, oldest entries are evicted. This is
/// fine — the journal on disk is the source of truth.
const LATEST_CACHE_CAP: usize = 256;

pub struct FinalResponseService {
    data_dir: PathBuf,
    envelope_tx: UnboundedSender<JournalEnvelope>,
    extractors: HashMap<ProviderId, Arc<dyn TranscriptExtractor>>,
    latest: Mutex<LatestCache>,
    subscribers: Mutex<Vec<FinalResponseCallback>>,
    /// Weak self-reference, populated by [`Self::into_arc`]. Lets the
    /// `JournalStopHook::on_stop` implementation (which only sees
    /// `&self`) reach into the original `Arc` and spawn the extraction
    /// task. The Weak path keeps the service free of strong cycles.
    self_arc: OnceLock<Weak<Self>>,
    /// Extractions currently in flight, keyed `(job_id, session_id)`.
    /// Duplicate Stop hooks for the same turn (e.g. global + per-stage
    /// settings both firing) arrive ~ms apart; running both extractions
    /// races the persist path and double-emits AFR envelopes. The second
    /// Stop while one is in flight is skipped — the turn it witnesses is
    /// the one already being extracted.
    in_flight: Mutex<HashSet<(u32, Option<String>)>>,
}

impl FinalResponseService {
    /// Build with the three first-party extractors (claude / codex /
    /// gemini) pre-registered. Tests can use `with_extractor` to swap
    /// in fakes.
    pub fn new(data_dir: PathBuf, envelope_tx: UnboundedSender<JournalEnvelope>) -> Self {
        let mut extractors: HashMap<ProviderId, Arc<dyn TranscriptExtractor>> = HashMap::new();
        extractors.insert(ProviderId::Claude, Arc::new(ClaudeExtractor));
        extractors.insert(ProviderId::Codex, Arc::new(CodexExtractor));
        extractors.insert(ProviderId::Gemini, Arc::new(GeminiExtractor));
        Self {
            data_dir,
            envelope_tx,
            extractors,
            latest: Mutex::new(LatestCache::new(LATEST_CACHE_CAP)),
            subscribers: Mutex::new(Vec::new()),
            self_arc: OnceLock::new(),
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    pub fn with_extractor(mut self, source: &str, extractor: Arc<dyn TranscriptExtractor>) -> Self {
        self.extractors.insert(ProviderId::parse(source), extractor);
        self
    }

    /// Finalise construction. Returns an `Arc<Self>` after wiring an
    /// internal `Weak<Self>` so the `JournalStopHook` impl can spawn
    /// tasks that need the strong reference.
    pub fn into_arc(self) -> Arc<Self> {
        let arc = Arc::new(self);
        // OnceLock::set returns Err if already set; we just constructed
        // this so it cannot be — ignore the result.
        let _ = arc.self_arc.set(Arc::downgrade(&arc));
        arc
    }

    /// Spawn one extraction task for a given Stop event. Public so the
    /// `JournalStopHook` shim and tests can both reach it.
    pub fn spawn_extraction(self: &Arc<Self>, source: String, ctx: ExtractionContext) {
        let Some(extractor) = self.extractors.get(&ProviderId::parse(&source)).cloned() else {
            tracing::debug!(source, "final-response: no extractor registered");
            return;
        };
        let key = (ctx.job_id, ctx.session_id.clone());
        if let Ok(mut guard) = self.in_flight.lock()
            && !guard.insert(key.clone())
        {
            tracing::debug!(
                job_id = ctx.job_id,
                "final-response: duplicate Stop while extraction in flight; skipping"
            );
            return;
        }
        let service = Arc::clone(self);
        let span = tracing::info_span!(
            "final_response_task",
            job_id = ctx.job_id,
            agent = %ctx.agent,
            source = %source,
        );
        let fut = async move {
            let job_id = ctx.job_id;
            let agent = ctx.agent.clone();
            let session_id = ctx.session_id.clone();
            // Heavy work (file I/O, hashing) runs on the blocking pool so
            // the runtime worker threads stay responsive.
            let extractor_for_task = Arc::clone(&extractor);
            let data_dir = service.data_dir.clone();
            let result = tokio::task::spawn_blocking(move || {
                run_extraction_blocking(&data_dir, extractor_for_task.as_ref(), &ctx)
            })
            .await;

            let event = match result {
                Ok(Ok(ok)) => build_success_event(job_id, agent, session_id, ok),
                Ok(Err(reason)) => build_failure_event(job_id, agent, session_id, reason),
                Err(join_err) => {
                    build_failure_event(job_id, agent, session_id, format!("task join: {join_err}"))
                }
            };

            self_after_extraction(&service, &event);
            if let Ok(mut guard) = service.in_flight.lock() {
                guard.remove(&key);
            }
        };
        tokio::spawn(tracing::Instrument::instrument(fut, span));
    }

    /// Publish a native-runtime turn outcome directly — no transcript,
    /// no extractor. The native session task owns the final text, so
    /// the persist + envelope + cache + subscriber path runs on it
    /// verbatim (same `final-response.md.<N>` layout and
    /// `AgentFinalResponse` envelope as a vendor extraction; sinks,
    /// projection, and pipeline waiters can't tell the difference).
    /// Blocking file I/O — callers run it via `spawn_blocking`.
    pub fn publish_native(self: &Arc<Self>, req: NativePublishRequest) {
        let event = match req.outcome {
            Ok(text) => {
                let was_empty = text.is_empty();
                match storage::persist_response(&self.data_dir, &req.agent, req.job_id, &text) {
                    Ok(outcome) => {
                        let preview = if was_empty {
                            EMPTY_TURN_PREVIEW.to_string()
                        } else {
                            outcome.preview
                        };
                        build_success_event(
                            req.job_id,
                            req.agent,
                            req.session_id,
                            ExtractionOk {
                                sha256_short: outcome.sha256_short,
                                bytes: outcome.bytes,
                                preview,
                                response_path: outcome.current_path,
                            },
                        )
                    }
                    Err(e) => build_failure_event(
                        req.job_id,
                        req.agent,
                        req.session_id,
                        format!("persist: {e}"),
                    ),
                }
            }
            Err(reason) => build_failure_event(req.job_id, req.agent, req.session_id, reason),
        };
        self_after_extraction(self, &event);
    }
}

/// One native turn's outcome, handed to [`FinalResponseService::publish_native`].
pub struct NativePublishRequest {
    pub job_id: u32,
    pub agent: String,
    pub session_id: Option<String>,
    /// `Ok(final_text)` for a completed turn (empty = legitimate empty
    /// turn), `Err(reason)` for a failed one — mirrors the extraction
    /// success/failure envelope split.
    pub outcome: Result<String, String>,
}

fn self_after_extraction(service: &Arc<FinalResponseService>, event: &FinalResponseEvent) {
    let envelope = envelope_from_event(event);
    if service.envelope_tx.send(envelope).is_err() {
        tracing::warn!("final-response: journal channel closed; dropping envelope");
    }
    // A poisoned mutex must not silently disable the cache / subscriber
    // broadcast for the rest of the process — recover and log (BUG-N06).
    match service.latest.lock() {
        Ok(mut latest) => latest.put(event.job_id, event.clone()),
        Err(poisoned) => {
            tracing::error!("final-response: latest cache mutex poisoned; recovering");
            poisoned.into_inner().put(event.job_id, event.clone());
        }
    }
    let subscribers: Vec<FinalResponseCallback> = match service.subscribers.lock() {
        Ok(g) => g.clone(),
        Err(poisoned) => {
            tracing::error!("final-response: subscribers mutex poisoned; recovering");
            poisoned.into_inner().clone()
        }
    };
    for cb in subscribers {
        cb(event.clone());
    }
}

impl JournalStopHook for FinalResponseService {
    /// Pulled by the listener on every parsed envelope. Filters for
    /// `Hook` envelopes whose `event` is `Stop` from a known provider,
    /// builds the extraction context from envelope fields, and hands
    /// off to [`Self::on_stop`].
    fn on_stop(&self, env: &JournalEnvelope) {
        if env.event_type != EventType::Hook {
            return;
        }
        if env.event.as_deref() != Some("Stop") {
            return;
        }
        let Some(source) = env.source.clone() else {
            return;
        };
        if !self.extractors.contains_key(&ProviderId::parse(&source)) {
            return;
        }
        let Some(job_id) = env.job_id else {
            return;
        };
        let agent = env.agent.clone().unwrap_or_else(|| source.clone());
        let transcript_path_hint = env
            .extra
            .get("transcript_path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from);
        let spawn_cwd = env
            .extra
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from);
        let ctx = ExtractionContext {
            job_id,
            agent,
            session_id: env.session_id.clone(),
            transcript_path_hint,
            spawn_cwd,
            // Production confines hints to each provider's real transcripts
            // dir; only tests override this (SEC-029).
            confine_root: None,
        };
        // `spawn_extraction` requires `Arc<Self>`; recover the strong
        // ref via the weak handle installed by `into_arc`.
        let Some(weak) = self.self_arc.get() else {
            tracing::error!(
                "final-response: hook fired before into_arc was called; dropping Stop event"
            );
            return;
        };
        let Some(strong) = weak.upgrade() else {
            // Service is being dropped; nothing to do.
            return;
        };
        strong.spawn_extraction(source, ctx);
    }
}

impl FinalResponseSource for FinalResponseService {
    fn subscribe(&self, callback: FinalResponseCallback) {
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.push(callback);
        }
    }

    fn latest_for_job(&self, job_id: u32) -> Option<FinalResponseEvent> {
        self.latest.lock().ok().and_then(|g| g.get(job_id))
    }
}

struct ExtractionOk {
    sha256_short: String,
    bytes: u64,
    preview: String,
    response_path: PathBuf,
}

fn run_extraction_blocking(
    data_dir: &std::path::Path,
    extractor: &dyn TranscriptExtractor,
    ctx: &ExtractionContext,
) -> Result<ExtractionOk, String> {
    // The `Stop` hook fires the instant the turn ends, but the provider may not
    // have fsync'd the final assistant block yet — a fast turn can land `Stop`
    // tens of ms before the text hits disk, yielding a spurious
    // `NoAssistantMessage` (or `TranscriptNotFound` if the file is brand new).
    // That spurious failure both loses the final response AND drops downstream
    // those "not ready yet" errors. This runs on the blocking pool, so the sleep
    // is free; a genuine empty turn (`Ok("")`) and hard errors do not spin.
    const EXTRACT_ATTEMPTS: usize = 6;
    const EXTRACT_BACKOFF_MS: u64 = 120;
    let mut extracted: Option<String> = None;
    let mut last_err = String::new();
    for attempt in 0..EXTRACT_ATTEMPTS {
        match extractor.extract_final_assistant_text(ctx) {
            Ok(s) => {
                extracted = Some(s);
                break;
            }
            Err(
                e @ (ExtractionError::TranscriptNotFound | ExtractionError::NoAssistantMessage),
            ) => {
                last_err = describe_err(&e);
                if attempt + 1 < EXTRACT_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(EXTRACT_BACKOFF_MS));
                }
            }
            Err(e) => return Err(describe_err(&e)),
        }
    }
    let raw = match extracted {
        Some(s) => s,
        None => return Err(last_err),
    };
    let was_empty = raw.is_empty();
    let outcome = match storage::persist_response(data_dir, &ctx.agent, ctx.job_id, &raw) {
        Ok(o) => o,
        Err(e) => return Err(format!("persist: {e}")),
    };
    let preview = if was_empty {
        EMPTY_TURN_PREVIEW.to_string()
    } else {
        outcome.preview
    };
    Ok(ExtractionOk {
        sha256_short: outcome.sha256_short,
        bytes: outcome.bytes,
        preview,
        response_path: outcome.current_path,
    })
}

fn describe_err(e: &ExtractionError) -> String {
    match e {
        ExtractionError::TranscriptNotFound => "transcript not found".into(),
        ExtractionError::TranscriptUnreadable(io) => format!("transcript unreadable: {io}"),
        ExtractionError::NoAssistantMessage => "no assistant message".into(),
        ExtractionError::MalformedTranscript(m) => format!("malformed transcript: {m}"),
    }
}

fn build_success_event(
    job_id: u32,
    agent: String,
    session_id: Option<String>,
    ok: ExtractionOk,
) -> FinalResponseEvent {
    FinalResponseEvent {
        job_id,
        agent,
        session_id,
        response_path: Some(ok.response_path),
        // Always record the sha — even for an empty turn it is a
        // legitimate witness of "agent said nothing on this turn".
        response_sha256: Some(ok.sha256_short),
        response_bytes: ok.bytes,
        response_preview: ok.preview,
    }
}

fn build_failure_event(
    job_id: u32,
    agent: String,
    session_id: Option<String>,
    reason: String,
) -> FinalResponseEvent {
    FinalResponseEvent {
        job_id,
        agent,
        session_id,
        response_path: None,
        response_sha256: None,
        response_bytes: 0,
        response_preview: storage::failure_preview(&reason),
    }
}

fn envelope_from_event(e: &FinalResponseEvent) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.event = Some("AgentFinalResponse".into());
    env.job_id = Some(e.job_id);
    env.agent = Some(e.agent.clone());
    env.session_id = e.session_id.clone();
    env.response_path = e
        .response_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    env.response_sha256 = e.response_sha256.clone();
    env.response_bytes = Some(e.response_bytes);
    env.response_preview = Some(e.response_preview.clone());
    env
}

/// Tiny FIFO LRU keyed by job_id. We deliberately avoid pulling in
/// `lru` as a dependency — the cap is small and access pattern is
/// trivial.
struct LatestCache {
    cap: usize,
    order: VecDeque<u32>,
    map: HashMap<u32, FinalResponseEvent>,
}

impl LatestCache {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            order: VecDeque::with_capacity(cap),
            map: HashMap::with_capacity(cap),
        }
    }

    fn put(&mut self, job_id: u32, event: FinalResponseEvent) {
        if self.map.insert(job_id, event).is_some() {
            self.order.retain(|j| *j != job_id);
        }
        self.order.push_back(job_id);
        while self.order.len() > self.cap {
            if let Some(evicted) = self.order.pop_front() {
                self.map.remove(&evicted);
            }
        }
    }

    fn get(&self, job_id: u32) -> Option<FinalResponseEvent> {
        self.map.get(&job_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeExtractor {
        text: String,
        calls: AtomicUsize,
    }

    impl TranscriptExtractor for FakeExtractor {
        fn extract_final_assistant_text(
            &self,
            _ctx: &ExtractionContext,
        ) -> Result<String, ExtractionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.text.clone())
        }
    }

    struct FailingExtractor;
    impl TranscriptExtractor for FailingExtractor {
        fn extract_final_assistant_text(
            &self,
            _ctx: &ExtractionContext,
        ) -> Result<String, ExtractionError> {
            Err(ExtractionError::TranscriptNotFound)
        }
    }

    fn ctx(job_id: u32) -> ExtractionContext {
        ExtractionContext {
            job_id,
            agent: "faye".into(),
            session_id: Some("s".into()),
            transcript_path_hint: None,
            spawn_cwd: None,
            confine_root: None,
        }
    }

    #[tokio::test]
    async fn success_writes_file_and_emits_envelope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
        let svc = Arc::new(
            FinalResponseService::new(dir.path().to_path_buf(), tx).with_extractor(
                "fake",
                Arc::new(FakeExtractor {
                    text: "hello".into(),
                    calls: AtomicUsize::new(0),
                }),
            ),
        );
        svc.spawn_extraction("fake".into(), ctx(11));
        let env = rx.recv().await.expect("envelope");
        assert_eq!(env.event.as_deref(), Some("AgentFinalResponse"));
        assert_eq!(env.job_id, Some(11));
        assert_eq!(env.response_bytes, Some(5));
        assert!(env.response_path.is_some());
        assert!(env.response_sha256.is_some());
    }

    #[tokio::test]
    async fn failure_still_emits_envelope_with_preview() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
        let svc = Arc::new(
            FinalResponseService::new(dir.path().to_path_buf(), tx)
                .with_extractor("fake", Arc::new(FailingExtractor)),
        );
        svc.spawn_extraction("fake".into(), ctx(12));
        let env = rx.recv().await.expect("envelope");
        assert_eq!(env.response_bytes, Some(0));
        assert!(env.response_path.is_none());
        let preview = env.response_preview.expect("preview");
        assert!(preview.starts_with("<extraction failed:"));
    }

    #[tokio::test]
    async fn empty_turn_uses_documented_preview() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
        let svc = Arc::new(
            FinalResponseService::new(dir.path().to_path_buf(), tx).with_extractor(
                "fake",
                Arc::new(FakeExtractor {
                    text: "".into(),
                    calls: AtomicUsize::new(0),
                }),
            ),
        );
        svc.spawn_extraction("fake".into(), ctx(13));
        let env = rx.recv().await.expect("envelope");
        assert_eq!(env.response_bytes, Some(0));
        assert_eq!(env.response_preview.as_deref(), Some(EMPTY_TURN_PREVIEW));
    }

    /// Extractor that blocks long enough for a duplicate Stop to arrive
    /// while the first extraction is still in flight.
    struct SlowExtractor {
        calls: Arc<AtomicUsize>,
    }

    impl TranscriptExtractor for SlowExtractor {
        fn extract_final_assistant_text(
            &self,
            _ctx: &ExtractionContext,
        ) -> Result<String, ExtractionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok("slow".into())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_stop_while_in_flight_runs_one_extraction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
        let calls = Arc::new(AtomicUsize::new(0));
        let svc = Arc::new(
            FinalResponseService::new(dir.path().to_path_buf(), tx).with_extractor(
                "fake",
                Arc::new(SlowExtractor {
                    calls: Arc::clone(&calls),
                }),
            ),
        );
        // Two Stops for the same (job, session) ~back-to-back — the dup
        // hook pattern observed in the wild (global + per-stage settings).
        svc.spawn_extraction("fake".into(), ctx(21));
        svc.spawn_extraction("fake".into(), ctx(21));
        let env = rx.recv().await.expect("envelope");
        assert_eq!(env.job_id, Some(21));
        // Give a skipped-but-spawned duplicate time to surface if the
        // guard failed, then assert exactly one extraction ran.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            rx.try_recv().is_err(),
            "second envelope must not be emitted"
        );

        // The guard clears after completion — a later turn for the same
        // key extracts again.
        svc.spawn_extraction("fake".into(), ctx(21));
        let env2 = rx.recv().await.expect("second turn envelope");
        assert_eq!(env2.job_id, Some(21));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// `publish_native` must be indistinguishable from a vendor
    /// extraction downstream: same envelope shape, same persisted
    /// layout, same sha/preview semantics for the same text.
    #[tokio::test]
    async fn publish_native_envelope_matches_vendor_extraction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
        let svc = Arc::new(
            FinalResponseService::new(dir.path().to_path_buf(), tx).with_extractor(
                "fake",
                Arc::new(FakeExtractor {
                    text: "hello".into(),
                    calls: AtomicUsize::new(0),
                }),
            ),
        );
        svc.spawn_extraction("fake".into(), ctx(11));
        let vendor = rx.recv().await.expect("vendor envelope");
        svc.publish_native(NativePublishRequest {
            job_id: 11,
            agent: "faye".into(),
            session_id: Some("s".into()),
            outcome: Ok("hello".into()),
        });
        let native = rx.recv().await.expect("native envelope");
        assert_eq!(native.event.as_deref(), Some("AgentFinalResponse"));
        assert_eq!(native.job_id, vendor.job_id);
        assert_eq!(native.agent, vendor.agent);
        assert_eq!(native.session_id, vendor.session_id);
        assert_eq!(native.response_sha256, vendor.response_sha256);
        assert_eq!(native.response_bytes, vendor.response_bytes);
        assert_eq!(native.response_preview, vendor.response_preview);
        assert!(svc.latest_for_job(11).is_some());
    }

    #[tokio::test]
    async fn publish_native_failure_emits_failure_preview() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
        let svc = Arc::new(FinalResponseService::new(dir.path().to_path_buf(), tx));
        svc.publish_native(NativePublishRequest {
            job_id: 31,
            agent: "kimi".into(),
            session_id: None,
            outcome: Err("kernel: unavailable".into()),
        });
        let env = rx.recv().await.expect("envelope");
        assert_eq!(env.response_bytes, Some(0));
        assert!(env.response_path.is_none());
        let preview = env.response_preview.expect("preview");
        assert!(preview.starts_with("<extraction failed:"), "{preview}");
    }

    #[tokio::test]
    async fn subscriber_is_invoked_and_latest_is_cached() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JournalEnvelope>();
        let svc = Arc::new(
            FinalResponseService::new(dir.path().to_path_buf(), tx).with_extractor(
                "fake",
                Arc::new(FakeExtractor {
                    text: "world".into(),
                    calls: AtomicUsize::new(0),
                }),
            ),
        );
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);
        svc.subscribe(Arc::new(move |_e| {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        svc.spawn_extraction("fake".into(), ctx(14));
        let _env = rx.recv().await.expect("envelope");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(svc.latest_for_job(14).is_some());
        assert!(svc.latest_for_job(99).is_none());
    }
}
