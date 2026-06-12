// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! The single login + premium gate. Every intelligence feature asks the gate
//! whether it is allowed to run. Fail-closed: no session, or an unknown/free
//! plan, means everything stays inert.

use std::sync::Arc;

use orkia_auth::AuthProvider;
use uuid::Uuid;

/// The set of plan slugs that unlock Orkia Intelligence. These mirror the
/// backend `BillingPlan` enum (`free`/`starter`/`team`/`org`) — every non-free
/// plan is premium. Anything not in this set — including the empty string,
/// `"free"`, or an unrecognized value — is treated as non-premium (fail-closed,
/// CLAUDE.md #8).
const PREMIUM_PLANS: &[&str] = &["starter", "team", "org"];

/// Reads the auth session and decides whether intelligence is enabled.
#[derive(Clone)]
pub struct Gate {
    auth: Arc<dyn AuthProvider>,
}

/// Why the gate is closed — surfaced to `$reasoning status` and login toasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    /// Logged in with a premium plan — intelligence runs.
    Enabled,
    /// No valid session.
    Anonymous,
    /// Logged in, but the plan is not premium.
    FreePlan,
}

impl Gate {
    pub fn new(auth: Arc<dyn AuthProvider>) -> Self {
        Self { auth }
    }

    /// Current gate state, recomputed from the live auth snapshot.
    pub fn state(&self) -> GateState {
        match self.auth.current() {
            None => GateState::Anonymous,
            Some(s) if is_premium(&s.plan) => GateState::Enabled,
            Some(_) => GateState::FreePlan,
        }
    }

    /// Convenience: is any intelligence feature allowed right now?
    pub fn is_enabled(&self) -> bool {
        self.state() == GateState::Enabled
    }

    /// Resolve the reasoning-graph identity (workspace + account) from the
    /// live session. Returns `None` when the gate is closed, when the session
    /// carries no identity (env/Forge-only), or when the ids are not valid
    /// UUIDs (fail-closed, CLAUDE.md #8) — the caller keeps intelligence inert.
    pub fn identity(&self) -> Option<Identity> {
        if self.state() != GateState::Enabled {
            return None;
        }
        let s = self.auth.current()?;
        let workspace_id = Uuid::parse_str(s.workspace_id.as_deref()?).ok()?;
        let account_id = Uuid::parse_str(s.account_id.as_deref()?).ok()?;
        Some(Identity {
            workspace_id,
            account_id,
        })
    }
}

/// The session-scoped identity the reasoning graph stamps onto every turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub workspace_id: Uuid,
    pub account_id: Uuid,
}

/// Premium check, isolated for testing. Case-insensitive on the slug.
pub(crate) fn is_premium(plan: &str) -> bool {
    let p = plan.trim().to_ascii_lowercase();
    PREMIUM_PLANS.contains(&p.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use orkia_auth::provider::{AuthError, AuthEventSink, SessionInfo};
    use std::sync::Mutex;

    struct StubAuth(Mutex<Option<SessionInfo>>);

    impl AuthProvider for StubAuth {
        fn login(&self, _: &mut dyn AuthEventSink) -> Result<SessionInfo, AuthError> {
            Err(AuthError::Cancelled)
        }
        fn logout(&self) -> Result<(), AuthError> {
            Ok(())
        }
        fn current(&self) -> Option<SessionInfo> {
            self.0.lock().unwrap().clone()
        }
        fn bearer(&self) -> Option<String> {
            None
        }
    }

    fn session(plan: &str) -> SessionInfo {
        SessionInfo {
            display_name: "k".into(),
            email: "k@x.io".into(),
            plan: plan.into(),
            issued_at: Utc::now(),
            expires_at: None,
            account_id: None,
            workspace_id: None,
        }
    }

    fn session_with_identity(plan: &str, ws: &str, acc: &str) -> SessionInfo {
        SessionInfo {
            account_id: Some(acc.into()),
            workspace_id: Some(ws.into()),
            ..session(plan)
        }
    }

    fn gate_with(plan: Option<&str>) -> Gate {
        Gate::new(Arc::new(StubAuth(Mutex::new(plan.map(session)))))
    }

    #[test]
    fn anonymous_is_closed() {
        assert_eq!(gate_with(None).state(), GateState::Anonymous);
        assert!(!gate_with(None).is_enabled());
    }

    #[test]
    fn free_plan_is_closed() {
        assert_eq!(gate_with(Some("free")).state(), GateState::FreePlan);
        assert!(!gate_with(Some("free")).is_enabled());
    }

    #[test]
    fn unknown_plan_fails_closed() {
        assert_eq!(gate_with(Some("wizard")).state(), GateState::FreePlan);
        assert_eq!(gate_with(Some("")).state(), GateState::FreePlan);
    }

    #[test]
    fn premium_plans_open_the_gate() {
        for p in ["starter", "Team", "ORG", " starter "] {
            assert!(gate_with(Some(p)).is_enabled(), "plan {p:?} should enable");
        }
    }

    #[test]
    fn identity_resolves_for_premium_with_valid_uuids() {
        let ws = "00000000-0000-0000-0000-0000000000aa";
        let acc = "00000000-0000-0000-0000-0000000000bb";
        let gate = Gate::new(Arc::new(StubAuth(Mutex::new(Some(session_with_identity(
            "starter", ws, acc,
        ))))));
        let id = gate.identity().expect("identity present");
        assert_eq!(id.workspace_id, Uuid::parse_str(ws).unwrap());
        assert_eq!(id.account_id, Uuid::parse_str(acc).unwrap());
    }

    #[test]
    fn identity_is_none_when_closed_or_missing_or_malformed() {
        // Free plan → closed → no identity even with ids present.
        let g_free = Gate::new(Arc::new(StubAuth(Mutex::new(Some(session_with_identity(
            "free",
            "00000000-0000-0000-0000-0000000000aa",
            "00000000-0000-0000-0000-0000000000bb",
        ))))));
        assert!(g_free.identity().is_none());
        // Premium but no ids → none.
        assert!(gate_with(Some("starter")).identity().is_none());
        // Premium but malformed ids → fail-closed.
        let g_bad = Gate::new(Arc::new(StubAuth(Mutex::new(Some(session_with_identity(
            "starter",
            "not-a-uuid",
            "also-bad",
        ))))));
        assert!(g_bad.identity().is_none());
    }
}
