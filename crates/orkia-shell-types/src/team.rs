// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Shell builtins ($team / $invite / $members / $share / $leave) call
//! through this trait so the OSS shell stays decoupled from any HTTP
//! transport. The proprietary distribution wires a
//! concrete `TeamClient` backed by the proprietary cloud client +
//! GraphQL; OSS builds inject a [`NoopTeamClient`] that surfaces a
//! "team operations require Orkia Team" message uniformly.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum TeamClientError {
    /// Shell isn't logged in. UI maps to "Run `$login` first."
    #[error("unauthenticated")]
    Unauthenticated,
    /// Token decoded but carries no role (legacy v1 JWT). UI maps to
    /// "Your session is stale. `$logout` then `$login` again."
    #[error("no role on caller; please re-login")]
    NoRole,
    /// Server returned 403 / FORBIDDEN. UI surfaces a permission
    /// message. Carries the server's message for debug logging.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// Caller is not a member of the named team.
    #[error("not a team member")]
    NotTeamMember,
    /// JWT lacks `workspace_id`. UI maps to
    /// "Switch to a workspace first."
    #[error("no workspace on caller")]
    NoWorkspace,
    /// Rate limit. Used by `$invite create`.
    #[error("rate limited")]
    RateLimited,
    /// No team backend wired (OSS shell). UI suggests Orkia Team.
    #[error("team operations require Orkia Team: {reason}")]
    Unavailable { reason: String },
    /// Catch-all for transport / decode failures. Carries the
    /// server's display string for debug logging.
    #[error("team backend error: {0}")]
    Other(String),
}

/// Snapshot of a team (subset of `team::Model` the shell renders).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamSummary {
    pub id: Uuid,
    pub identifier: String,
    pub name: String,
    pub description: Option<String>,
    pub color: Option<String>,
    pub owner_account_id: Uuid,
}

/// Single member row inside a team.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMemberSummary {
    pub id: Uuid,
    pub team_id: Uuid,
    pub account_id: Option<Uuid>,
    pub agent_name: Option<String>,
    /// Stringified role (`"owner"`, `"admin"`, `"member"`, `"guest"`).
    pub role: String,
}

/// Workspace membership row (caller's view of who's in the workspace
/// and at what tier).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMemberSummary {
    pub account_id: Uuid,
    pub workspace_id: Uuid,
    /// `"owner"`, `"admin"`, `"member"`.
    pub role: String,
}

/// Pending invite row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteSummary {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub email: String,
    /// `"owner"`, `"admin"`, `"member"`.
    pub role: String,
    pub nonce: String,
    pub status: String,
    pub expires_at: String,
}

/// Project-clone row (a workspace receiving a shared project).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedProjectSummary {
    pub id: Uuid,
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    /// `"read"` or `"write"`.
    pub access: String,
}

/// Bootstrap snapshot — what `$team ls` / `$members ls` read.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TeamSnapshot {
    pub workspace_id: Option<Uuid>,
    pub seq: i64,
    pub teams: Vec<TeamSummary>,
    pub team_members: Vec<TeamMemberSummary>,
    pub workspace_members: Vec<WorkspaceMemberSummary>,
    pub pending_invites: Vec<InviteSummary>,
    pub shared_projects: Vec<SharedProjectSummary>,
    /// Projects in the current workspace, keyed by their name (the
    /// "slug" the shell resolves for `$share project <slug>`). V1.1
    /// addition (punchlist Item 2.4); pre-2.4 snapshots default to
    /// empty and the share handler falls back to UUID-only.
    #[serde(default)]
    pub projects: Vec<ProjectSummary>,
    /// Team ids the caller belongs to (mirror of `claims.teams`).
    pub team_scope: Vec<Uuid>,
}

/// Minimal project shape carried in the team snapshot for slug→UUID
/// (project name) in `$share project`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: Uuid,
    pub name: String,
}

/// What the GraphQL `me` query returns. Used by the pipeline
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeView {
    pub account_id: Uuid,
    pub email: String,
    pub workspace_id: Option<Uuid>,
    pub role: Option<String>,
    pub org_role: Option<String>,
    pub teams: Vec<MeTeamMembership>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeTeamMembership {
    pub team_id: Uuid,
    pub role: String,
}

#[derive(Debug, Clone)]
pub struct CreateTeamArgs {
    pub identifier: String,
    pub name: String,
    pub description: Option<String>,
    pub color: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CreateInviteArgs {
    pub email: String,
    pub role: String,
    pub ttl_days: i64,
}

#[derive(Debug, Clone)]
pub struct AddMemberArgs {
    pub team_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub account_id: Option<Uuid>,
    pub agent_name: Option<String>,
    pub role: String,
}

#[derive(Debug, Clone)]
pub struct ShareProjectArgs {
    pub project_id: Uuid,
    pub target_workspace_id: Uuid,
    pub access: String,
}

#[derive(Debug, Clone)]
pub struct ShareIssueArgs {
    pub issue_id: Uuid,
    pub target_workspace_id: Uuid,
    pub access: String,
}

#[derive(Debug, Clone)]
pub struct AcceptInviteOutcome {
    pub account_id: Uuid,
    pub workspace_id: Uuid,
    /// Bearer token the shell stashes via its [`AuthProvider`].
    pub token: String,
}

/// Outcome of [`TeamClient::join_team`]. Mirrors `AcceptInviteOutcome`
/// but carries the team identifiers the caller needs to update its
/// in-process cache + emit the `workspace.team_joined` SEAL record.
#[derive(Debug, Clone)]
pub struct TeamJoinResponse {
    pub account_id: Uuid,
    pub team_id: Uuid,
    pub team_name: String,
    /// Role granted on join. Free-form string matching the backend's
    /// role taxonomy (`"member"`, `"admin"`, …) so the shell can
    /// surface it without depending on an enum that may evolve.
    pub role: String,
    /// Refreshed JWT whose `claims.teams` now includes `team_id`.
    /// The shell stashes this via its [`AuthProvider`].
    pub token: String,
}

/// Backend-agnostic team operations. Each builtin maps to one method.
/// Implementations are expected to fail with
/// [`TeamClientError::Unavailable`] when the backend is intentionally
/// absent (OSS builds), [`TeamClientError::Unauthenticated`] /
/// [`TeamClientError::NoRole`] for stale-token cases, and
/// [`TeamClientError::Other`] for transport hiccups.
#[async_trait]
pub trait TeamClient: Send + Sync {
    /// Caller introspection (GraphQL `me`). Used by the pipeline
    /// coordinator gate to confirm server-resolved team membership.
    async fn me(&self) -> Result<MeView, TeamClientError>;

    /// Full workspace snapshot (16-entity bootstrap). Backs
    /// `$team ls`, `$members ls`, `$share ls`.
    async fn bootstrap(&self) -> Result<TeamSnapshot, TeamClientError>;

    async fn create_team(&self, args: CreateTeamArgs) -> Result<TeamSummary, TeamClientError>;
    async fn delete_team(&self, team_id: Uuid) -> Result<bool, TeamClientError>;

    async fn create_invite(&self, args: CreateInviteArgs)
    -> Result<InviteSummary, TeamClientError>;
    async fn revoke_invite(&self, nonce: &str) -> Result<bool, TeamClientError>;
    /// `accept_invite` is callable by an unauthenticated shell — the
    /// returned token is what the shell stashes via its
    /// [`AuthProvider`].
    async fn accept_invite(&self, nonce: &str) -> Result<AcceptInviteOutcome, TeamClientError>;

    /// Accept a **team** invite (distinct from workspace `accept_invite`).
    /// The caller is already authenticated to a workspace and uses the
    /// nonce to be added as a member of a team within that workspace.
    /// Returns the team membership details plus a refreshed JWT whose
    /// `claims.teams` includes the newly-joined team.
    async fn join_team(&self, nonce: &str) -> Result<TeamJoinResponse, TeamClientError>;

    async fn add_team_member(
        &self,
        args: AddMemberArgs,
    ) -> Result<TeamMemberSummary, TeamClientError>;
    async fn remove_team_member(
        &self,
        team_id: Uuid,
        account_id: Option<Uuid>,
        agent_name: Option<String>,
    ) -> Result<bool, TeamClientError>;
    async fn change_team_member_role(
        &self,
        team_id: Uuid,
        account_id: Option<Uuid>,
        agent_name: Option<String>,
        new_role: String,
    ) -> Result<TeamMemberSummary, TeamClientError>;

    async fn share_project(
        &self,
        args: ShareProjectArgs,
    ) -> Result<SharedProjectSummary, TeamClientError>;
    async fn unshare_project(
        &self,
        project_id: Uuid,
        target_workspace_id: Uuid,
    ) -> Result<bool, TeamClientError>;
    async fn share_issue(&self, args: ShareIssueArgs) -> Result<(), TeamClientError>;

    async fn leave_workspace(&self) -> Result<bool, TeamClientError>;
}

/// Drop-in implementation for OSS builds. Every method short-circuits
/// to [`TeamClientError::Unavailable`] so the builtins surface a
/// consistent "team operations require Orkia Team" message instead
/// of attempting a network call that would fail with a generic
/// connection error.
pub struct NoopTeamClient;

impl NoopTeamClient {
    pub fn new() -> Self {
        Self
    }

    fn unavailable<T>(op: &str) -> Result<T, TeamClientError> {
        Err(TeamClientError::Unavailable {
            reason: format!("no team backend wired ({op})"),
        })
    }
}

impl Default for NoopTeamClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TeamClient for NoopTeamClient {
    async fn me(&self) -> Result<MeView, TeamClientError> {
        Self::unavailable("me")
    }
    async fn bootstrap(&self) -> Result<TeamSnapshot, TeamClientError> {
        Self::unavailable("bootstrap")
    }
    async fn create_team(&self, _: CreateTeamArgs) -> Result<TeamSummary, TeamClientError> {
        Self::unavailable("team create")
    }
    async fn delete_team(&self, _: Uuid) -> Result<bool, TeamClientError> {
        Self::unavailable("team rm")
    }
    async fn create_invite(&self, _: CreateInviteArgs) -> Result<InviteSummary, TeamClientError> {
        Self::unavailable("invite create")
    }
    async fn revoke_invite(&self, _: &str) -> Result<bool, TeamClientError> {
        Self::unavailable("invite revoke")
    }
    async fn accept_invite(&self, _: &str) -> Result<AcceptInviteOutcome, TeamClientError> {
        Self::unavailable("invite accept")
    }
    async fn join_team(&self, _: &str) -> Result<TeamJoinResponse, TeamClientError> {
        Self::unavailable("team join")
    }
    async fn add_team_member(
        &self,
        _: AddMemberArgs,
    ) -> Result<TeamMemberSummary, TeamClientError> {
        Self::unavailable("members add")
    }
    async fn remove_team_member(
        &self,
        _: Uuid,
        _: Option<Uuid>,
        _: Option<String>,
    ) -> Result<bool, TeamClientError> {
        Self::unavailable("members rm")
    }
    async fn change_team_member_role(
        &self,
        _: Uuid,
        _: Option<Uuid>,
        _: Option<String>,
        _: String,
    ) -> Result<TeamMemberSummary, TeamClientError> {
        Self::unavailable("members role")
    }
    async fn share_project(
        &self,
        _: ShareProjectArgs,
    ) -> Result<SharedProjectSummary, TeamClientError> {
        Self::unavailable("share project")
    }
    async fn unshare_project(&self, _: Uuid, _: Uuid) -> Result<bool, TeamClientError> {
        Self::unavailable("share unshare project")
    }
    async fn share_issue(&self, _: ShareIssueArgs) -> Result<(), TeamClientError> {
        Self::unavailable("share issue")
    }
    async fn leave_workspace(&self) -> Result<bool, TeamClientError> {
        Self::unavailable("leave")
    }
}

/// Translate a `TeamClientError` into the user-facing string the shell
/// specifies. Centralised so every builtin surfaces the same
/// wording.
pub fn error_message(err: &TeamClientError) -> String {
    match err {
        TeamClientError::Unauthenticated => "You're not logged in. Run `$login` first.".into(),
        TeamClientError::NoRole => {
            "Your session is stale. Run `$logout` then `$login` again.".into()
        }
        // V1.1 (punchlist Item 2.6): enrich with a next step.
        TeamClientError::Forbidden(_) => {
            "You don't have permission for this operation. Ask a workspace admin for elevated access."
                .into()
        }
        TeamClientError::NotTeamMember => {
            "You're not a member of that team. Ask a team admin to invite you, or `$team ls` to see teams you do belong to."
                .into()
        }
        TeamClientError::NoWorkspace => {
            "No workspace context. Run `$invite accept <nonce>` to join one, or check `$workspace`."
                .into()
        }
        TeamClientError::RateLimited => "Too many invites recently. Try again in an hour.".into(),
        TeamClientError::Unavailable { reason } => {
            format!("Team operations require Orkia Team. See https://orkia.dev/team ({reason})")
        }
        // V1.1 (Item 2.6 / quality F.6): translate server-side invite
        // errors that arrive via the catch-all `Other` variant into
        // user-actionable text. End-to-end error-code mapping is
        // DEFERRED — when it lands the bottom branch shrinks.
        TeamClientError::Other(msg) => {
            let lower = msg.to_ascii_lowercase();
            if lower.contains("invite already resolved") || lower.contains("alreadyresolved") {
                return "You've already accepted this invite \u{2014} you're a member of the workspace.".into();
            }
            if lower.contains("invite expired") || lower.contains("expired") {
                return "This invite has expired. Ask the admin for a fresh invite.".into();
            }
            if lower.contains("invite not found") {
                return "Invite nonce not recognized. Double-check the magic link.".into();
            }
            format!("Team backend error: {msg}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_message_includes_next_step() {
        let s = error_message(&TeamClientError::Forbidden("nope".into()));
        assert!(s.contains("Ask a workspace admin"));
    }

    #[test]
    fn not_team_member_mentions_team_ls() {
        let s = error_message(&TeamClientError::NotTeamMember);
        assert!(s.contains("$team ls"));
    }

    #[test]
    fn no_workspace_mentions_invite_accept() {
        let s = error_message(&TeamClientError::NoWorkspace);
        assert!(s.contains("$invite accept"));
    }

    #[test]
    fn already_resolved_invite_maps_to_actionable_message() {
        let s = error_message(&TeamClientError::Other("invite already resolved".into()));
        assert!(s.contains("you're a member"));
    }

    #[test]
    fn expired_invite_message() {
        let s = error_message(&TeamClientError::Other("invite expired".into()));
        assert!(s.contains("expired"));
        assert!(s.contains("fresh invite"));
    }
}

// ─── reusable mock ───────────────────────────────────────────────────────────
//
// Gated behind the `test-utils` feature (auto-enabled under `cfg(test)`) so
// downstream test crates can depend on `orkia-shell-types` with
// `features = ["test-utils"]` and instantiate `MockTeamClient` without
// re-implementing the wide TeamClient surface.

#[cfg(any(test, feature = "test-utils"))]
pub mod mock {
    use super::*;
    use std::sync::Mutex;

    /// Reusable [`TeamClient`] for tests. Most methods return
    /// `TeamClientError::Other("mock not configured")`; the small set of
    /// methods that tests typically wire up (`bootstrap`, `join_team`, `me`)
    /// are configurable via setter helpers.
    #[derive(Default)]
    pub struct MockTeamClient {
        pub snapshot: Mutex<Option<TeamSnapshot>>,
        pub me_view: Mutex<Option<MeView>>,
        pub join_response: Mutex<Option<TeamJoinResponse>>,
    }

    impl MockTeamClient {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn set_snapshot(&self, snap: TeamSnapshot) {
            *self.snapshot.lock().unwrap() = Some(snap);
        }

        pub fn set_me_view(&self, me: MeView) {
            *self.me_view.lock().unwrap() = Some(me);
        }

        pub fn set_join_response(&self, resp: TeamJoinResponse) {
            *self.join_response.lock().unwrap() = Some(resp);
        }
    }

    fn not_configured(method: &str) -> TeamClientError {
        TeamClientError::Other(format!("MockTeamClient::{method} not configured"))
    }

    #[async_trait]
    impl TeamClient for MockTeamClient {
        async fn me(&self) -> Result<MeView, TeamClientError> {
            self.me_view
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| not_configured("me"))
        }
        async fn bootstrap(&self) -> Result<TeamSnapshot, TeamClientError> {
            self.snapshot
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| not_configured("bootstrap"))
        }
        async fn create_team(&self, _: CreateTeamArgs) -> Result<TeamSummary, TeamClientError> {
            Err(not_configured("create_team"))
        }
        async fn delete_team(&self, _: Uuid) -> Result<bool, TeamClientError> {
            Err(not_configured("delete_team"))
        }
        async fn create_invite(
            &self,
            _: CreateInviteArgs,
        ) -> Result<InviteSummary, TeamClientError> {
            Err(not_configured("create_invite"))
        }
        async fn revoke_invite(&self, _: &str) -> Result<bool, TeamClientError> {
            Err(not_configured("revoke_invite"))
        }
        async fn accept_invite(&self, _: &str) -> Result<AcceptInviteOutcome, TeamClientError> {
            Err(not_configured("accept_invite"))
        }
        async fn join_team(&self, _: &str) -> Result<TeamJoinResponse, TeamClientError> {
            self.join_response
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| not_configured("join_team"))
        }
        async fn add_team_member(
            &self,
            _: AddMemberArgs,
        ) -> Result<TeamMemberSummary, TeamClientError> {
            Err(not_configured("add_team_member"))
        }
        async fn remove_team_member(
            &self,
            _: Uuid,
            _: Option<Uuid>,
            _: Option<String>,
        ) -> Result<bool, TeamClientError> {
            Err(not_configured("remove_team_member"))
        }
        async fn change_team_member_role(
            &self,
            _: Uuid,
            _: Option<Uuid>,
            _: Option<String>,
            _: String,
        ) -> Result<TeamMemberSummary, TeamClientError> {
            Err(not_configured("change_team_member_role"))
        }
        async fn share_project(
            &self,
            _: ShareProjectArgs,
        ) -> Result<SharedProjectSummary, TeamClientError> {
            Err(not_configured("share_project"))
        }
        async fn unshare_project(&self, _: Uuid, _: Uuid) -> Result<bool, TeamClientError> {
            Err(not_configured("unshare_project"))
        }
        async fn share_issue(&self, _: ShareIssueArgs) -> Result<(), TeamClientError> {
            Err(not_configured("share_issue"))
        }
        async fn leave_workspace(&self) -> Result<bool, TeamClientError> {
            Err(not_configured("leave_workspace"))
        }
    }

    #[cfg(test)]
    mod mock_tests {
        use super::*;

        #[test]
        fn setters_round_trip() {
            let m = MockTeamClient::new();
            m.set_join_response(TeamJoinResponse {
                account_id: Uuid::nil(),
                team_id: Uuid::nil(),
                team_name: "acme".into(),
                role: "member".into(),
                token: "tok".into(),
            });
            assert_eq!(
                m.join_response.lock().unwrap().as_ref().unwrap().team_name,
                "acme"
            );
        }
    }
}
