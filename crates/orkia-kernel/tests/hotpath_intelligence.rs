// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Intelligence-level hot-path integration: prove the wiring the REPL's
//! `boot_intelligence` performs end-to-end — a premium gate boots the
//! consumer task, journal hook envelopes pushed onto the broadcast bus land
//! as turns in the on-disk store, attributed via the shared per-job scope map.
//! Mirrors how the shell subscribes the consumer to the same bus SEAL/stream
//! read.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::broadcast;
use uuid::Uuid;

use orkia_auth::AuthProvider;
use orkia_auth::provider::{AuthError, AuthEventSink, SessionInfo};
use orkia_kernel::{BootConfig, CaptureScope, Intelligence, JobScope, new_job_scopes};
use orkia_reasoning_store::ReasoningStore;
use orkia_shell_types::journal::{EventType, JournalEnvelope};

const WS: u128 = 0xA11CE;
const ACC: u128 = 0xB0B;

/// Minimal in-process auth provider: a fixed session with the given plan and
/// (optionally) the workspace/account identity the gate parses.
struct StubAuth(Option<SessionInfo>);

impl AuthProvider for StubAuth {
    fn login(&self, _: &mut dyn AuthEventSink) -> Result<SessionInfo, AuthError> {
        Err(AuthError::Cancelled)
    }
    fn logout(&self) -> Result<(), AuthError> {
        Ok(())
    }
    fn current(&self) -> Option<SessionInfo> {
        self.0.clone()
    }
    fn bearer(&self) -> Option<String> {
        None
    }
}

fn premium_session() -> SessionInfo {
    SessionInfo {
        display_name: "k".into(),
        email: "k@x.io".into(),
        plan: "solo-pro".into(),
        issued_at: Utc::now(),
        expires_at: None,
        account_id: Some(Uuid::from_u128(ACC).to_string()),
        workspace_id: Some(Uuid::from_u128(WS).to_string()),
    }
}

fn hook(event: &str, job_id: u32) -> JournalEnvelope {
    let mut env = JournalEnvelope::now(EventType::Hook);
    env.job_id = Some(job_id);
    env.agent = Some("faye".into());
    env.event = Some(event.into());
    env
}

/// Poll the on-disk store until `project_id` has at least one turn (the writer
/// is a separate connection draining the bus async), or time out. Uses async
/// sleep so the single-threaded test runtime can run the consumer task.
async fn await_turns(
    path: &std::path::Path,
    project_id: Uuid,
) -> Vec<orkia_reasoning_store::StoredTurn> {
    for _ in 0..50 {
        if let Ok(store) = ReasoningStore::open(path)
            && let Ok(turns) = store.turns_for_project(project_id)
            && !turns.is_empty()
        {
            return turns;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    Vec::new()
}

#[tokio::test]
async fn premium_gate_captures_bus_hooks_into_store() {
    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("reasoning").join("reasoning.db");

    // A live per-job scope (as the REPL writes at spawn) attributes job 7 to a
    // known project, so we can query the store deterministically.
    let project = Uuid::from_u128(0xC0FFEE);
    let job_scopes = new_job_scopes();
    job_scopes.write().unwrap().insert(
        7,
        JobScope {
            project_id: Some(project),
            rfc_ref: None,
        },
    );

    let (tx, rx) = broadcast::channel(64);
    let auth = Arc::new(StubAuth(Some(premium_session())));
    let mut intel = Intelligence::new(auth, None);

    // Identity resolves from the session's workspace/account UUIDs.
    let id = intel.identity().expect("premium identity");
    assert_eq!(id.workspace_id, Uuid::from_u128(WS));
    assert_eq!(id.account_id, Uuid::from_u128(ACC));

    let booted = intel
        .boot(BootConfig {
            store_path: store_path.clone(),
            scope: CaptureScope {
                workspace_id: id.workspace_id,
                account_id: id.account_id,
                project_id: None,
                rfc_ref: None,
            },
            bus: rx,
            job_scopes: job_scopes.clone(),
            backend_url: String::new(),
            audit: None,
        })
        .expect("boot");
    assert!(booted, "premium gate must boot the consumer");
    assert!(intel.is_active());

    // Push a session + a tool call onto the SAME bus the shell subscribes to.
    tx.send(hook("SessionStart", 7)).unwrap();
    let mut pre = hook("PreToolUse", 7);
    pre.tool = Some("Bash".into());
    pre.target = Some("cargo build".into());
    tx.send(pre).unwrap();

    let turns = await_turns(&store_path, project).await;
    assert_eq!(turns.len(), 1, "the tool call must land as one turn");
    assert_eq!(
        turns[0].kind,
        orkia_reasoning_core::enums::TurnKind::ToolCall("Bash".into()),
    );
    assert_eq!(turns[0].project_id, Some(project));

    intel.shutdown();
    assert!(!intel.is_active());
}

#[tokio::test]
async fn free_gate_stays_inert_no_store() {
    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("reasoning").join("reasoning.db");
    let mut session = premium_session();
    session.plan = "free".into();

    let (_tx, rx) = broadcast::channel(8);
    let auth = Arc::new(StubAuth(Some(session)));
    let mut intel = Intelligence::new(auth, None);

    assert!(
        intel.identity().is_none(),
        "free plan has no reasoning identity"
    );
    let booted = intel
        .boot(BootConfig {
            store_path: store_path.clone(),
            scope: CaptureScope {
                workspace_id: Uuid::from_u128(WS),
                account_id: Uuid::from_u128(ACC),
                project_id: None,
                rfc_ref: None,
            },
            bus: rx,
            job_scopes: new_job_scopes(),
            backend_url: String::new(),
            audit: None,
        })
        .expect("boot");
    assert!(!booted, "free gate must stay inert");
    assert!(!intel.is_active());
    // Fail-closed: no store directory created on the inert path.
    assert!(!store_path.exists());
}
