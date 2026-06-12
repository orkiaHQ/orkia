// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Per-flow environment selection.
//!
//! A flow declares the [`FlowEnv`] it needs; the `orkia-check` runner
//! groups flows by env and boots one session per distinct env. The only
//! axis that varies today is the subscription [`Plan`], which the harness
//! maps to a per-plan fixture email and logs in for real against the
//! compose backend. `FlowEnv` is a struct (not a bare `Plan`) so future
//! axes (kernel attachment, team wiring) can be added without touching
//! every flow's declaration.

/// Subscription plan a flow's shell session runs under. Selects the
/// per-plan fixture account the harness logs in as; the backend resolves
/// the plan from that account's `organization.billing_plan` and bakes it
/// into the signed JWT.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Plan {
    #[default]
    Free,
    SoloPro,
    Team,
    Enterprise,
}

impl Plan {
    /// Canonical plan string (`orkia_capabilities::Plan::parse` accepts
    /// it). Used for labels and assertions, not for injecting auth.
    pub fn as_env_value(self) -> &'static str {
        match self {
            Plan::Free => "free",
            Plan::SoloPro => "solo-pro",
            Plan::Team => "team",
            Plan::Enterprise => "enterprise",
        }
    }

    /// The seeded fixture account this plan logs in as. Each maps to an
    /// org carrying the matching `billing_plan` (see
    /// `orkia-server/src/seed.rs` `PLAN_FIXTURES`).
    pub fn fixture_email(self) -> &'static str {
        match self {
            Plan::Free => "free@e2e.orkia.dev",
            Plan::SoloPro => "solo@e2e.orkia.dev",
            Plan::Team => "team@e2e.orkia.dev",
            Plan::Enterprise => "ent@e2e.orkia.dev",
        }
    }

    /// Rank for deterministic group ordering — Free first so the existing
    /// flows always run in the first booted session.
    fn rank(self) -> u8 {
        match self {
            Plan::Free => 0,
            Plan::SoloPro => 1,
            Plan::Team => 2,
            Plan::Enterprise => 3,
        }
    }
}

impl PartialOrd for Plan {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Plan {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

/// Environment a flow requires. Flows are grouped by this; the runner
/// boots one session per distinct value (Free sorts first). `extra_env`
/// participates in `Eq`/`Hash`/`Ord`, so a flow with extra vars forms its
/// own group → its own session boot.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct FlowEnv {
    pub plan: Plan,
    /// Extra environment variables injected at session boot (after
    /// `ORKIA_PLAN`). Used by F502 to set `ORKIA_SCHEDULED=1` (simulate a
    /// crond-fired invocation).
    pub extra_env: Vec<(String, String)>,
}

impl FlowEnv {
    /// The default Free environment — what all S0–S3 flows use.
    pub fn free() -> Self {
        Self {
            plan: Plan::Free,
            extra_env: Vec::new(),
        }
    }

    /// A flow that needs a specific plan.
    pub fn with_plan(plan: Plan) -> Self {
        Self {
            plan,
            extra_env: Vec::new(),
        }
    }

    /// A flow that needs extra env vars on top of a plan (own session group).
    pub fn with_env(plan: Plan, extra_env: Vec<(String, String)>) -> Self {
        Self { plan, extra_env }
    }

    /// Stable label for JSON output (`env_group`). The plan name, suffixed
    /// with `+<key>` for each extra env var so distinct groups are visible.
    pub fn label(&self) -> String {
        if self.extra_env.is_empty() {
            self.plan.as_env_value().to_string()
        } else {
            let extras: Vec<&str> = self.extra_env.iter().map(|(k, _)| k.as_str()).collect();
            format!("{}+{}", self.plan.as_env_value(), extras.join("+"))
        }
    }
}
