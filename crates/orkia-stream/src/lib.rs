// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `orkia-stream` — local-to-backend event publisher.
//!
//! Tails the on-disk SealChain `.jsonl` files and the in-process
//! Journal broadcast bus, filters every event by its `scope`
//! (fail-closed), and POSTs NDJSON batches to `orkia-server`'s
//! `/api/sync/push` endpoint. It is a publisher only — never pulls,
//! never reconciles, never reads from the backend.
//!

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod auth;
pub mod batch;
pub mod builtin;
pub mod config;
pub mod cursor;
pub mod errors;
pub mod scope;
pub mod sources;
pub mod translate;
pub mod transport;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use orkia_auth::AuthProvider;
use orkia_shell_types::journal::JournalEnvelope;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

pub use auth::{AuthContext, AuthSnapshot, TeamMembershipProbe};
pub use batch::Batcher;
pub use builtin::{StreamAction, dispatch};
pub use config::StreamConfig;
pub use cursor::SealCursor;
pub use errors::StreamError;
pub use scope::ScopeGate;
pub use sources::seal::SealSource;
pub use translate::{JournalEventPush, LocalSealRecordPush, PushLine};
pub use transport::{HttpClient, PushOutcome};

/// Status snapshot returned by `status()`. Cheap to read.
#[derive(Debug, Clone)]
pub enum StreamStatus {
    Running {
        events_published: u64,
        last_flush: Option<Instant>,
        lag: Duration,
    },
    Paused,
    NoAuth,
    Unreachable {
        last_attempt: Option<Instant>,
        retry_count: u32,
    },
    Disabled,
}

/// Handle held by the shell. Dropping it does not stop the task —
/// callers must call [`shutdown`] explicitly so the final flush has
/// a chance to land. Cloning is cheap (all internals are `Arc`).
#[derive(Clone)]
pub struct StreamHandle {
    inner: Arc<HandleInner>,
}

struct HandleInner {
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    events_published: Arc<AtomicU64>,
    retry_count: Arc<std::sync::atomic::AtomicU32>,
    last_flush: Arc<parking_lot_compat::Mutex<Option<Instant>>>,
    last_attempt: Arc<parking_lot_compat::Mutex<Option<Instant>>>,
    no_auth: Arc<AtomicBool>,
    unreachable: Arc<AtomicBool>,
    paused_flag_path: std::path::PathBuf,
    join: parking_lot_compat::Mutex<Option<JoinHandle<()>>>,
}

// Tiny std-only Mutex shim so we don't pull in parking_lot just for
// one cell. Wraps std::sync::Mutex with poison-tolerant locking.
mod parking_lot_compat {
    use std::sync::Mutex as StdMutex;
    pub struct Mutex<T>(StdMutex<T>);
    impl<T> Mutex<T> {
        pub fn new(v: T) -> Self {
            Self(StdMutex::new(v))
        }
        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().unwrap_or_else(|p| p.into_inner())
        }
    }
}

/// Build a config + spawn the stream task.
///
/// Returns `Ok(None)` when the stream should not run (disabled by env,
/// no auth token). Otherwise returns a handle the shell can query and
/// later shut down.
pub fn start(
    config: StreamConfig,
    journal: broadcast::Receiver<JournalEnvelope>,
    auth: Arc<dyn AuthProvider>,
    team_probe: Option<auth::TeamMembershipProbe>,
) -> Result<Option<StreamHandle>, StreamError> {
    if config.disabled {
        tracing::info!("orkia-stream: disabled via config / env, not starting");
        return Ok(None);
    }
    let bearer = auth.bearer();
    if bearer.is_none() {
        tracing::info!("orkia-stream: no auth token, run 'orkia auth login' to enable publishing");
        return Ok(None);
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(config.paused_flag_present()));
    let events_published = Arc::new(AtomicU64::new(0));
    let retry_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let last_flush = Arc::new(parking_lot_compat::Mutex::new(None));
    let last_attempt = Arc::new(parking_lot_compat::Mutex::new(None));
    let no_auth = Arc::new(AtomicBool::new(false));
    let unreachable = Arc::new(AtomicBool::new(false));

    let mut auth_ctx = AuthContext::new(auth);
    if let Some(probe) = team_probe {
        auth_ctx = auth_ctx.with_team_probe(probe);
    }
    let transport = HttpClient::new(config.backend_url.clone(), auth_ctx.clone())?;
    let scope_gate = ScopeGate::new(Arc::new(auth_ctx.clone()));
    let seal_root = config.seal_root.clone();
    let state_dir = config.state_dir.clone();
    let cursor = SealCursor::load_or_default(&state_dir);
    let batcher = Batcher::new(
        config.batch_max_events,
        config.batch_max_bytes,
        config.batch_flush_interval,
    );

    let task_state = TaskState {
        cancel: cancel.clone(),
        paused: paused.clone(),
        events_published: events_published.clone(),
        retry_count: retry_count.clone(),
        last_flush: last_flush.clone(),
        last_attempt: last_attempt.clone(),
        no_auth: no_auth.clone(),
        unreachable: unreachable.clone(),
    };

    let join = tokio::spawn(run(
        RunCtx {
            state: task_state,
            config: config.clone(),
            seal_root,
            scope_gate,
            batcher,
            transport,
            auth_ctx,
        },
        journal,
        cursor,
    ));

    Ok(Some(StreamHandle {
        inner: Arc::new(HandleInner {
            cancel,
            paused,
            events_published,
            retry_count,
            last_flush,
            last_attempt,
            no_auth,
            unreachable,
            paused_flag_path: config.paused_flag_path(),
            join: parking_lot_compat::Mutex::new(Some(join)),
        }),
    }))
}

/// Status snapshot. Reads atomics; safe to call from anywhere.
pub fn status(handle: &StreamHandle) -> StreamStatus {
    let h = &handle.inner;
    if h.no_auth.load(Ordering::Relaxed) {
        return StreamStatus::NoAuth;
    }
    if h.paused.load(Ordering::Relaxed) {
        return StreamStatus::Paused;
    }
    if h.unreachable.load(Ordering::Relaxed) {
        return StreamStatus::Unreachable {
            last_attempt: *h.last_attempt.lock(),
            retry_count: h.retry_count.load(Ordering::Relaxed),
        };
    }
    let last = *h.last_flush.lock();
    let lag = last.map(|t| t.elapsed()).unwrap_or(Duration::ZERO);
    StreamStatus::Running {
        events_published: h.events_published.load(Ordering::Relaxed),
        last_flush: last,
        lag,
    }
}

/// Pause: writes the flag file and flips the atomic. Idempotent.
pub fn pause(handle: &StreamHandle) -> std::io::Result<()> {
    let p = &handle.inner.paused_flag_path;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(p, b"paused\n")?;
    handle.inner.paused.store(true, Ordering::Relaxed);
    Ok(())
}

/// Resume: removes the flag file and flips the atomic. Idempotent.
pub fn resume(handle: &StreamHandle) -> std::io::Result<()> {
    let _ = std::fs::remove_file(&handle.inner.paused_flag_path);
    handle.inner.paused.store(false, Ordering::Relaxed);
    Ok(())
}

/// Graceful shutdown with a bounded flush attempt. After `timeout`
/// the join handle is dropped — any pending batch may not land.
pub async fn shutdown(handle: StreamHandle, timeout: Duration) {
    handle.inner.cancel.store(true, Ordering::Relaxed);
    let join = handle.inner.join.lock().take();
    if let Some(j) = join {
        let _ = tokio::time::timeout(timeout, j).await;
    }
}

#[derive(Clone)]
struct TaskState {
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    events_published: Arc<AtomicU64>,
    retry_count: Arc<std::sync::atomic::AtomicU32>,
    last_flush: Arc<parking_lot_compat::Mutex<Option<Instant>>>,
    last_attempt: Arc<parking_lot_compat::Mutex<Option<Instant>>>,
    no_auth: Arc<AtomicBool>,
    unreachable: Arc<AtomicBool>,
}

/// Bundles the invariant inputs to [`run`] — everything except the
/// mutable journal receiver (which Tokio requires exclusive ownership of
/// and cannot be stored in a shared struct) and the seal cursor (which is
/// mutated inside the loop). Both are passed alongside `RunCtx` at the
/// call site.
struct RunCtx {
    state: TaskState,
    config: StreamConfig,
    seal_root: std::path::PathBuf,
    scope_gate: ScopeGate,
    batcher: Batcher,
    transport: HttpClient,
    auth_ctx: AuthContext,
}

async fn run(
    ctx: RunCtx,
    journal_rx: broadcast::Receiver<JournalEnvelope>,
    mut cursor: SealCursor,
) {
    let RunCtx {
        state,
        config,
        seal_root,
        scope_gate,
        mut batcher,
        transport,
        auth_ctx,
    } = ctx;

    // Decouple the ephemeral journal broadcast from the (slow, retrying)
    // network flush below. A tiny pump task drains the broadcast into a bounded
    // internal queue and never touches the network, so a backend outage can no
    // longer wedge this loop inside `flush_batch` and starve `journal_rx` —
    // which used to overflow the SHARED broadcast bus and lose live events for
    // every consumer. Internal-queue overflow is dropped with a counter:
    // bounded, local loss instead of a shared-bus lag (BUG-041). Seal events
    // stay on this loop — they're file-backed and naturally backpressured (the
    // cursor only advances on a successful flush), so they are never dropped.
    let (journal_tx, mut journal_rx_internal) =
        tokio::sync::mpsc::channel::<JournalEnvelope>(JOURNAL_QUEUE_CAP);
    let pump = tokio::spawn(journal_pump(journal_rx, journal_tx, state.clone()));

    let mut seal_source = SealSource::new(seal_root, &cursor);
    seal_source.initial_scan();

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if state.cancel.load(Ordering::Relaxed) {
            break;
        }
        if !state.paused.load(Ordering::Relaxed) {
            for ev in seal_source.poll() {
                process_seal(&ev, &scope_gate, &mut batcher, &auth_ctx);
            }
        }
        drain_journal(
            &mut journal_rx_internal,
            &state,
            &scope_gate,
            &mut batcher,
            &auth_ctx,
        )
        .await;
        if batcher.should_flush() {
            flush_batch(&state, &config, &mut batcher, &transport, &mut cursor).await;
            tick.reset();
        }
        let _ = tick.tick().await;
    }

    if batcher.has_pending() {
        let batch = batcher.take();
        let _ = transport.push_batch(&batch).await;
    }
    let _ = pump.await;
}

/// Maximum live journal events buffered between the broadcast pump and the
/// batch loop before overflow is dropped (with a counter). Bounds local loss
/// during a backend outage instead of lagging the shared broadcast bus.
const JOURNAL_QUEUE_CAP: usize = 4096;

/// Drain the journal broadcast bus into the bounded internal queue. Runs as its
/// own task so it always keeps the shared broadcast drained, regardless of how
/// long a network flush takes in the batch loop (BUG-041).
async fn journal_pump(
    mut broadcast_rx: broadcast::Receiver<JournalEnvelope>,
    tx: tokio::sync::mpsc::Sender<JournalEnvelope>,
    state: TaskState,
) {
    let mut dropped: u64 = 0;
    loop {
        if state.cancel.load(Ordering::Relaxed) {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(250), broadcast_rx.recv()).await {
            Ok(Ok(env)) => {
                // Never block on the internal queue: that would re-introduce
                // the broadcast-lag starvation. Drop with a counter instead.
                if tx.try_send(env).is_err() {
                    dropped += 1;
                    if dropped.is_power_of_two() {
                        tracing::warn!(
                            dropped,
                            "orkia-stream: internal journal queue full; dropping live events (backend slow/down)"
                        );
                    }
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                tracing::warn!(dropped = n, "orkia-stream: journal broadcast lagged");
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Err(_) => { /* timeout — re-check cancel */ }
        }
    }
}

/// Drain one buffered journal event with a short timeout so the loop also
/// honours the flush interval.
async fn drain_journal(
    journal_rx: &mut tokio::sync::mpsc::Receiver<JournalEnvelope>,
    state: &TaskState,
    scope_gate: &ScopeGate,
    batcher: &mut Batcher,
    auth_ctx: &AuthContext,
) {
    match tokio::time::timeout(Duration::from_millis(100), journal_rx.recv()).await {
        Ok(Some(env)) => {
            if !state.paused.load(Ordering::Relaxed) {
                process_journal(&env, scope_gate, batcher, auth_ctx);
            }
        }
        Ok(None) => { /* pump stopped / internal channel closed */ }
        Err(_) => { /* drain timeout — fall through to flush check */ }
    }
}

/// Attempt to flush the current batch to the backend. On success advances
/// the cursor; on transient failure requeues the batch for the next tick.
async fn flush_batch(
    state: &TaskState,
    config: &StreamConfig,
    batcher: &mut Batcher,
    transport: &HttpClient,
    cursor: &mut SealCursor,
) {
    *state.last_attempt.lock() = Some(Instant::now());
    let batch = batcher.take();
    match transport.push_batch(&batch).await {
        Ok(PushOutcome::Accepted { accepted }) => {
            state.unreachable.store(false, Ordering::Relaxed);
            state.retry_count.store(0, Ordering::Relaxed);
            state
                .events_published
                .fetch_add(accepted as u64, Ordering::Relaxed);
            *state.last_flush.lock() = Some(Instant::now());
            cursor.advance(&batch);
            cursor.persist(&config.state_dir);
        }
        Ok(PushOutcome::Dropped) => {
            cursor.advance(&batch);
            cursor.persist(&config.state_dir);
        }
        Ok(PushOutcome::AuthExpired) => {
            state.no_auth.store(true, Ordering::Relaxed);
            tracing::error!("orkia-stream: auth expired, run 'orkia auth login' to resume");
            state.paused.store(true, Ordering::Relaxed);
            batcher.requeue(batch);
        }
        Err(_) => {
            state.unreachable.store(true, Ordering::Relaxed);
            state.retry_count.fetch_add(1, Ordering::Relaxed);
            batcher.requeue(batch);
        }
    }
}

fn process_seal(
    ev: &sources::seal::SealEvent,
    gate: &ScopeGate,
    batcher: &mut Batcher,
    auth: &AuthContext,
) {
    let decision = gate.evaluate_seal(&ev.record, &ev.chain_id);
    if !decision.publish {
        if let Some(reason) = decision.warn_reason {
            tracing::warn!(reason = %reason, "orkia-stream: dropping seal event");
        }
        // Advance batcher's cursor view via a sentinel so the run-loop
        // cursor advance still moves past this record.
        batcher.note_dropped_seal(
            &ev.chain_id,
            ev.record.seq,
            ev.record.hash.clone(),
            ev.byte_end,
        );
        return;
    }
    let (workspace_id, account_id, team_id) = match auth.identity() {
        Some(id) => id,
        None => return,
    };
    let push = translate::to_local_seal_push(
        &ev.chain_id,
        &ev.record,
        workspace_id,
        account_id,
        team_id,
        decision.scope_label(),
    );
    batcher.push_seal(
        &ev.chain_id,
        ev.record.seq,
        ev.record.hash.clone(),
        ev.byte_end,
        push,
    );
}

fn process_journal(
    env: &JournalEnvelope,
    gate: &ScopeGate,
    batcher: &mut Batcher,
    auth: &AuthContext,
) {
    // Use the envelope target as the dedup key when present; fall
    // back to the source (e.g. "scope", "hook") so dedup still groups
    // sensibly when target is absent.
    let artifact_id = env
        .target
        .as_deref()
        .or(env.source.as_deref())
        .unwrap_or("journal");
    let decision = gate.evaluate_journal(env, artifact_id);
    if !decision.publish {
        if let Some(reason) = decision.warn_reason {
            tracing::warn!(reason = %reason, "orkia-stream: dropping journal event");
        }
        return;
    }
    let (workspace_id, account_id, team_id) = match auth.identity() {
        Some(id) => id,
        None => return,
    };
    // events become typed public pushes rather than generic journal_events.
    match env.event.as_deref() {
        Some("job.spawned") | Some("job.complete") => {
            if let Some(push) = translate::to_public_job_push(env, workspace_id) {
                batcher.push_journal(push);
            }
            return;
        }
        Some("routing.decided") => {
            if let Some(push) = translate::to_public_routing_push(env, workspace_id) {
                batcher.push_journal(push);
            }
            return;
        }
        _ => {}
    }
    let push = translate::to_journal_push(
        env,
        workspace_id,
        account_id,
        team_id,
        decision.scope_label(),
    );
    batcher.push_journal(push);
}

impl StreamHandle {
    /// Read-only access to the underlying paused flag (for tests + builtin).
    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::Relaxed)
    }

    pub fn events_published(&self) -> u64 {
        self.inner.events_published.load(Ordering::Relaxed)
    }
}
