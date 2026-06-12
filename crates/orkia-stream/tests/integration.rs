// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! End-to-end: synthesize SealChain files + journal envelopes, run
//! `orkia_stream::start`, point it at a local axum mock server, and
//! verify the captured push lines.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use orkia_auth::{AuthError, AuthEventSink, AuthProvider, SessionInfo};
use orkia_shell_types::journal::JournalEnvelope;
use orkia_shell_types::journal::types::EventType;
use orkia_shell_types::seal::SealRecord;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use uuid::Uuid;

#[derive(Default)]
struct CaptureState {
    lines: Mutex<Vec<serde_json::Value>>,
}

async fn push_handler(
    State(state): State<Arc<CaptureState>>,
    body: String,
) -> axum::Json<serde_json::Value> {
    let mut count = 0;
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            state.lines.lock().unwrap().push(v);
            count += 1;
        }
    }
    axum::Json(serde_json::json!({
        "accepted": count,
        "conflicts": 0,
        "errors": 0,
        "results": [],
    }))
}

async fn spawn_mock_backend() -> (String, Arc<CaptureState>) {
    let state = Arc::new(CaptureState::default());
    let app = Router::new()
        .route("/api/sync/push", post(push_handler))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state)
}

struct StaticAuth {
    bearer: String,
    identity: Option<(Uuid, Uuid)>,
}
impl StaticAuth {
    fn new(bearer: &str) -> Self {
        Self {
            bearer: bearer.into(),
            identity: None,
        }
    }
    fn with_identity(bearer: &str, ws: Uuid, acc: Uuid) -> Self {
        Self {
            bearer: bearer.into(),
            identity: Some((ws, acc)),
        }
    }
}
impl AuthProvider for StaticAuth {
    fn login(&self, _sink: &mut dyn AuthEventSink) -> Result<SessionInfo, AuthError> {
        Err(AuthError::Backend("test".into()))
    }
    fn logout(&self) -> Result<(), AuthError> {
        Ok(())
    }
    fn current(&self) -> Option<SessionInfo> {
        let (ws, acc) = self.identity?;
        Some(SessionInfo {
            display_name: "test".into(),
            email: "test@e2e.orkia.dev".into(),
            plan: "team".into(),
            issued_at: chrono::Utc::now(),
            expires_at: None,
            account_id: Some(acc.to_string()),
            workspace_id: Some(ws.to_string()),
        })
    }
    fn bearer(&self) -> Option<String> {
        Some(self.bearer.clone())
    }
}

fn write_record(path: &Path, record: &SealRecord) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    let line = serde_json::to_string(record).unwrap();
    writeln!(f, "{line}").unwrap();
}

fn rec(seq: u64, scope: &str, event: &str) -> SealRecord {
    SealRecord {
        seq,
        timestamp: chrono::Utc::now().to_rfc3339(),
        event_type: event.into(),
        detail: serde_json::json!({"scope": scope, "k": "v"}),
        hash: format!("h{seq}"),
        prev_hash: if seq == 0 {
            "0".into()
        } else {
            format!("h{}", seq - 1)
        },
        rfc_id: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn publishes_public_drops_private_and_team() {
    let (backend_url, state) = spawn_mock_backend().await;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    write_record(
        &home.join("projects/p/seal.jsonl"),
        &rec(0, "public", "rfc.create"),
    );
    write_record(
        &home.join("projects/p/seal.jsonl"),
        &rec(1, "private", "rfc.create"),
    );
    write_record(
        &home.join("projects/p/seal.jsonl"),
        &rec(2, "team", "rfc.create"),
    );
    write_record(
        &home.join("projects/p/seal.jsonl"),
        &rec(3, "public", "rfc.update"),
    );
    write_record(
        &home.join("workspace/seal.jsonl"),
        &rec(0, "public", "workspace.scope_default_changed"),
    );

    // Identity now comes from the session (provider.current()), not env.
    let ws_id = Uuid::new_v4();
    let acc_id = Uuid::new_v4();
    // SAFETY: tests are single-threaded over this env var; no concurrent reader.
    unsafe {
        std::env::set_var("ORKIA_BACKEND_URL", &backend_url);
    }
    // The default URL must be https; our test backend is http. Bypass
    // by tweaking the config directly instead of going through env.
    let mut cfg =
        orkia_stream::StreamConfig::from_env(home).unwrap_or_else(|_| orkia_stream::StreamConfig {
            backend_url: backend_url.clone(),
            seal_root: home.to_path_buf(),
            state_dir: home.join("state/stream"),
            batch_max_events: 50,
            batch_max_bytes: 262_144,
            batch_flush_interval: Duration::from_millis(200),
            disabled: false,
        });
    cfg.backend_url = backend_url.clone();
    cfg.batch_flush_interval = Duration::from_millis(200);

    let (bus_tx, _) = broadcast::channel::<JournalEnvelope>(64);
    let bus_rx = bus_tx.subscribe();
    let auth = Arc::new(StaticAuth::with_identity("test-bearer", ws_id, acc_id));

    let handle = orkia_stream::start(cfg, bus_rx, auth, None).unwrap();
    assert!(handle.is_some(), "stream must start with bearer + session");
    let handle = handle.unwrap();

    // Emit a public + private journal envelope.
    let mut env_pub = JournalEnvelope::now(EventType::Hook);
    env_pub.event = Some("PreToolUse".into());
    env_pub
        .extra
        .insert("scope".into(), serde_json::Value::String("public".into()));
    bus_tx.send(env_pub).unwrap();
    let mut env_priv = JournalEnvelope::now(EventType::Hook);
    env_priv.event = Some("PreToolUse".into());
    bus_tx.send(env_priv).unwrap();

    // Wait for the periodic rescan to surface the pre-seeded chain
    // files and for the batch flush interval to drain them to the
    // mock backend. The notify watcher install runs on a background
    // OS thread and may take many seconds on macOS, but the periodic
    // rescan (1s) is sufficient to pick up the seeded files.
    tokio::time::sleep(Duration::from_secs(3)).await;
    orkia_stream::shutdown(handle, Duration::from_secs(2)).await;

    let captured = state.lines.lock().unwrap();
    // Expected: 2 public seal (seq 0 + seq 3), 1 public workspace seal,
    // 1 public journal event. Private + team + private journal dropped.
    let seal_events: Vec<_> = captured
        .iter()
        .filter(|v| v["entity_type"] == "local_seal_record")
        .collect();
    let journal_events: Vec<_> = captured
        .iter()
        .filter(|v| v["entity_type"] == "journal_event")
        .collect();
    assert_eq!(
        seal_events.len(),
        3,
        "expected 3 public seal events, got {seal_events:#?}"
    );
    assert_eq!(
        journal_events.len(),
        1,
        "expected 1 public journal event, got {journal_events:#?}"
    );
    // All journal events must be public.
    for je in &journal_events {
        assert_eq!(je["data"]["scope"], "public");
    }
    // All seal events must be public scope (in content payload).
    for se in &seal_events {
        assert_eq!(se["data"]["content"]["scope"], "public");
    }
}

/// W2 success-path complement: `start` returns `Some(handle)` when the
/// provider exposes a bearer and the config is enabled. Pairs with
/// [`does_not_start_without_bearer`] and [`disabled_config_does_not_start`]
/// O1 / A3).
#[tokio::test(flavor = "multi_thread")]
async fn starts_when_bearer_present_and_config_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = orkia_stream::StreamConfig {
        backend_url: "https://api.example".into(),
        seal_root: dir.path().to_path_buf(),
        state_dir: dir.path().join("state/stream"),
        batch_max_events: 50,
        batch_max_bytes: 262_144,
        batch_flush_interval: Duration::from_millis(200),
        disabled: false,
    };
    let (_tx, rx) = broadcast::channel::<JournalEnvelope>(8);
    let handle =
        orkia_stream::start(cfg, rx, Arc::new(StaticAuth::new("test-bearer")), None).unwrap();
    assert!(
        handle.is_some(),
        "stream must start when bearer is present and config is enabled"
    );
    if let Some(h) = handle {
        orkia_stream::shutdown(h, Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn does_not_start_without_bearer() {
    struct NoAuth;
    impl AuthProvider for NoAuth {
        fn login(&self, _s: &mut dyn AuthEventSink) -> Result<SessionInfo, AuthError> {
            Err(AuthError::Backend("test".into()))
        }
        fn logout(&self) -> Result<(), AuthError> {
            Ok(())
        }
        fn current(&self) -> Option<SessionInfo> {
            None
        }
        fn bearer(&self) -> Option<String> {
            None
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = orkia_stream::StreamConfig {
        backend_url: "https://api.example".into(),
        seal_root: dir.path().to_path_buf(),
        state_dir: dir.path().join("state/stream"),
        batch_max_events: 50,
        batch_max_bytes: 262_144,
        batch_flush_interval: Duration::from_millis(200),
        disabled: false,
    };
    let (_tx, rx) = broadcast::channel::<JournalEnvelope>(8);
    let handle = orkia_stream::start(cfg, rx, Arc::new(NoAuth), None).unwrap();
    assert!(handle.is_none(), "stream must not start without a bearer");
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_config_does_not_start() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = orkia_stream::StreamConfig {
        backend_url: "https://api.example".into(),
        seal_root: dir.path().to_path_buf(),
        state_dir: dir.path().join("state/stream"),
        batch_max_events: 50,
        batch_max_bytes: 262_144,
        batch_flush_interval: Duration::from_millis(200),
        disabled: true,
    };
    let (_tx, rx) = broadcast::channel::<JournalEnvelope>(8);
    let handle = orkia_stream::start(cfg, rx, Arc::new(StaticAuth::new("any")), None).unwrap();
    assert!(handle.is_none(), "disabled config must not start");
}
