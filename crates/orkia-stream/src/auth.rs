// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `AuthContext` — bearer access + identity resolution.
//!
//! The shell hands `orkia-stream` an `Arc<dyn AuthProvider>`. This
//! module wraps it with a small, cheap-to-clone façade that the
//! batcher/transport touch on every flush.
//!
//! Identity (workspace_id, account_id) comes from the persisted session:
//! the magic-link login saves them alongside the bearer (resolved from
//! the signed JWT's claims by the backend at verify time), and the
//! [`AuthProvider`] surfaces them on [`orkia_auth::SessionInfo`]. No env
//! override — a session is the only source of identity.

use std::sync::Arc;

use orkia_auth::AuthProvider;
use uuid::Uuid;

/// Probe returning `true` when the current session is a member of at
/// least one team. Resolved live (callers like `ScopeGate` re-read on
/// every decision) so that a mid-session `orkia team join` flips
/// outcomes without needing to reconstruct the gate.
pub type TeamMembershipProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// Snapshot of auth state at one instant — used by [`crate::scope::ScopeGate`]
#[derive(Debug, Clone, Copy)]
pub struct AuthSnapshot {
    pub authenticated: bool,
    pub has_any_team_membership: bool,
}

#[derive(Clone)]
pub struct AuthContext {
    provider: Arc<dyn AuthProvider>,
    team_probe: Option<TeamMembershipProbe>,
}

impl AuthContext {
    pub fn new(provider: Arc<dyn AuthProvider>) -> Self {
        Self {
            provider,
            team_probe: None,
        }
    }

    /// Attach a probe used by the scope gate to decide `scope=team`
    /// outcomes. When `None`, the gate behaves as if the caller has no
    /// team membership (the safe default).
    pub fn with_team_probe(mut self, probe: TeamMembershipProbe) -> Self {
        self.team_probe = Some(probe);
        self
    }

    pub fn bearer(&self) -> Option<String> {
        self.provider.bearer()
    }

    /// Live read of auth + team state for one decision.
    pub fn snapshot(&self) -> AuthSnapshot {
        AuthSnapshot {
            authenticated: self.provider.bearer().is_some(),
            has_any_team_membership: self.team_probe.as_ref().map(|p| p()).unwrap_or(false),
        }
    }

    /// `(workspace_id, account_id, team_id)` for outbound pushes, read
    /// from the persisted session. `team_id` is always `None` in V1
    pub fn identity(&self) -> Option<(Uuid, Uuid, Option<Uuid>)> {
        let session = self.provider.current()?;
        let ws = parse_uuid(session.workspace_id.as_deref()?)?;
        let acc = parse_uuid(session.account_id.as_deref()?)?;
        Some((ws, acc, None))
    }

    /// Re-read the bearer token after a 401. Returns true if a fresh
    /// bearer is now available. The provider trait does not expose a
    /// refresh API — concrete backends refresh on their own schedule
    /// — so this is best-effort.
    pub fn try_refresh(&self) -> bool {
        self.provider.bearer().is_some()
    }
}

fn parse_uuid(s: &str) -> Option<Uuid> {
    Uuid::parse_str(s).ok()
}
