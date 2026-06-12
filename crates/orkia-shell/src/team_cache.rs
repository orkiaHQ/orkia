// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! `~/.orkia/teams.cache.json` mirrors the slice of the workspace
//! snapshot the team-mode builtins need: teams, members, pending
//! invites, shared projects. The cache is read on every builtin call
//! and refreshed via [`TeamCache::refresh`] when the entry is stale
//! (5-minute TTL), the workspace changed, or the user typed
//! `team refresh`.
//!
//! Live `workspace_events` subscription is out of V1 — the cloud
//! client's `subscribe_workspace_events` still returns
//! `NotYetImplemented`. The cache instead refreshes on demand and
//! after every mutation the builtins perform locally.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use orkia_shell_types::{
    InviteSummary, ProjectSummary, SharedProjectSummary, TeamClient, TeamClientError,
    TeamMemberSummary, TeamSnapshot, TeamSummary, WorkspaceMemberSummary,
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

const CACHE_FILENAME: &str = "teams.cache.json";
const STALE_AFTER: Duration = Duration::from_secs(300);

/// Disk representation. `fetched_at` is RFC3339 for grep-friendliness;
/// `SystemTime` would serialize as a u64 pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTeamData {
    pub workspace_id: Option<Uuid>,
    pub fetched_at: DateTime<Utc>,
    pub seq: i64,
    pub teams: Vec<TeamSummary>,
    pub team_members: Vec<TeamMemberSummary>,
    pub workspace_members: Vec<WorkspaceMemberSummary>,
    pub pending_invites: Vec<InviteSummary>,
    pub shared_projects: Vec<SharedProjectSummary>,
    /// V1.1 (Item 2.4): workspace projects keyed by name, populated
    /// from the bootstrap's `entities.project` by the proprietary
    /// `TeamClient` impl. `serde(default)` keeps old cache files
    /// (pre-2.4) loadable.
    #[serde(default)]
    pub projects: Vec<ProjectSummary>,
    pub team_scope: Vec<Uuid>,
}

impl CachedTeamData {
    fn from_snapshot(snap: TeamSnapshot) -> Self {
        Self {
            workspace_id: snap.workspace_id,
            fetched_at: Utc::now(),
            seq: snap.seq,
            teams: snap.teams,
            team_members: snap.team_members,
            workspace_members: snap.workspace_members,
            pending_invites: snap.pending_invites,
            shared_projects: snap.shared_projects,
            projects: snap.projects,
            team_scope: snap.team_scope,
        }
    }

    fn is_stale(&self) -> bool {
        let now: SystemTime = SystemTime::now();
        match now.duration_since(SystemTime::from(self.fetched_at)) {
            Ok(d) => d > STALE_AFTER,
            Err(_) => false, // clock skew → treat as fresh
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TeamCacheError {
    #[error("team backend: {0}")]
    Backend(#[from] TeamClientError),
    #[error("cache IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("cache parse: {0}")]
    Parse(String),
}

/// In-memory cache wrapping a [`TeamClient`]. Cheap to clone (`Arc`s
/// inside); each shell builds one at session start and hands it to
/// every builtin handler.
pub struct TeamCache {
    data_dir: PathBuf,
    inner: RwLock<Option<CachedTeamData>>,
    client: Arc<dyn TeamClient>,
}

impl TeamCache {
    pub fn new(data_dir: PathBuf, client: Arc<dyn TeamClient>) -> Self {
        let inner = Self::load_from_disk(&data_dir).ok();
        Self {
            data_dir,
            inner: RwLock::new(inner),
            client,
        }
    }

    /// Snapshot reference for read-only access. Returns `None` when
    /// the cache hasn't been populated yet — callers should usually
    /// go through [`Self::get_or_refresh`] instead.
    pub async fn current(&self) -> Option<CachedTeamData> {
        self.inner.read().await.clone()
    }

    /// Borrow the inner `RwLock` for synchronous `try_read` access.
    /// Used by the REPL's completion snapshot to read the cache
    /// Item 2.3 of the V1 punchlist). Callers must accept that a
    /// concurrent mutation may cause `try_read` to fail; the
    /// caller's degrade-to-empty path is the correct response.
    pub fn inner_lock(&self) -> &RwLock<Option<CachedTeamData>> {
        &self.inner
    }

    /// Read the cache, refreshing if missing, stale, or pointing at
    /// a different workspace than the caller expects.
    pub async fn get_or_refresh(
        &self,
        expected_workspace: Option<Uuid>,
    ) -> Result<CachedTeamData, TeamCacheError> {
        let needs_refresh = {
            let guard = self.inner.read().await;
            match &*guard {
                None => true,
                Some(d) => d.is_stale() || d.workspace_id != expected_workspace,
            }
        };
        if needs_refresh {
            self.refresh().await?;
        }
        Ok(self
            .inner
            .read()
            .await
            .clone()
            .unwrap_or_else(|| CachedTeamData {
                workspace_id: expected_workspace,
                fetched_at: Utc::now(),
                seq: 0,
                teams: Vec::new(),
                team_members: Vec::new(),
                workspace_members: Vec::new(),
                pending_invites: Vec::new(),
                shared_projects: Vec::new(),
                projects: Vec::new(),
                team_scope: Vec::new(),
            }))
    }

    /// Force a re-bootstrap. Used by `team refresh` and by mutation
    /// handlers after a successful create/delete/share/etc.
    pub async fn refresh(&self) -> Result<(), TeamCacheError> {
        let snap = self.client.bootstrap().await?;
        let data = CachedTeamData::from_snapshot(snap);
        Self::save_to_disk(&self.data_dir, &data)?;
        *self.inner.write().await = Some(data);
        Ok(())
    }

    /// Returns true when the cached snapshot lists at least one team
    /// for the active workspace. Used by the scope-warning path
    /// trigger the "no team membership" warning. Non-blocking; treats
    /// an unloaded cache as "no team" (conservative).
    pub fn has_any_team_sync(&self) -> bool {
        match self.inner.try_read() {
            Ok(guard) => guard.as_ref().map(|d| !d.teams.is_empty()).unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Look up a team by identifier (slug) or UUID.
    pub async fn find_team(&self, target: &str) -> Option<TeamSummary> {
        let guard = self.inner.read().await;
        let data = guard.as_ref()?;
        if let Ok(uuid) = Uuid::parse_str(target) {
            return data.teams.iter().find(|t| t.id == uuid).cloned();
        }
        data.teams.iter().find(|t| t.identifier == target).cloned()
    }

    /// punchlist Item 2.4). Returns `None` until a real TeamClient
    /// populates `projects` from the bootstrap — the OSS Noop client
    /// always leaves this empty.
    pub async fn find_project(&self, target: &str) -> Option<ProjectSummary> {
        let guard = self.inner.read().await;
        let data = guard.as_ref()?;
        if let Ok(uuid) = Uuid::parse_str(target) {
            return data.projects.iter().find(|p| p.id == uuid).cloned();
        }
        data.projects.iter().find(|p| p.name == target).cloned()
    }

    fn cache_path(data_dir: &Path) -> PathBuf {
        data_dir.join(CACHE_FILENAME)
    }

    fn load_from_disk(data_dir: &Path) -> Result<CachedTeamData, TeamCacheError> {
        let path = Self::cache_path(data_dir);
        let text = std::fs::read_to_string(&path)?;
        serde_json::from_str(&text).map_err(|e| TeamCacheError::Parse(e.to_string()))
    }

    fn save_to_disk(data_dir: &Path, data: &CachedTeamData) -> Result<(), TeamCacheError> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::cache_path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json =
            serde_json::to_string_pretty(data).map_err(|e| TeamCacheError::Parse(e.to_string()))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use orkia_shell_types::{
        AcceptInviteOutcome, AddMemberArgs, CreateInviteArgs, CreateTeamArgs, MeView,
        ShareIssueArgs, ShareProjectArgs,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// Mock client that records bootstrap calls and returns a canned
    /// snapshot. Sufficient for cache-refresh logic; the
    /// TeamClient trait surface is wide so the mock stubs each
    /// method with `unreachable!()` — tests that need other methods
    /// extend this struct.
    struct MockClient {
        bootstrap_calls: AtomicUsize,
        snapshot: TeamSnapshot,
    }

    impl MockClient {
        fn new(snapshot: TeamSnapshot) -> Self {
            Self {
                bootstrap_calls: AtomicUsize::new(0),
                snapshot,
            }
        }
    }

    #[async_trait]
    impl TeamClient for MockClient {
        async fn me(&self) -> Result<MeView, TeamClientError> {
            unreachable!("me not exercised in cache tests")
        }
        async fn bootstrap(&self) -> Result<TeamSnapshot, TeamClientError> {
            self.bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.snapshot.clone())
        }
        async fn create_team(&self, _: CreateTeamArgs) -> Result<TeamSummary, TeamClientError> {
            unreachable!()
        }
        async fn delete_team(&self, _: Uuid) -> Result<bool, TeamClientError> {
            unreachable!()
        }
        async fn create_invite(
            &self,
            _: CreateInviteArgs,
        ) -> Result<InviteSummary, TeamClientError> {
            unreachable!()
        }
        async fn revoke_invite(&self, _: &str) -> Result<bool, TeamClientError> {
            unreachable!()
        }
        async fn accept_invite(&self, _: &str) -> Result<AcceptInviteOutcome, TeamClientError> {
            unreachable!()
        }
        async fn join_team(
            &self,
            _: &str,
        ) -> Result<orkia_shell_types::TeamJoinResponse, TeamClientError> {
            unreachable!()
        }
        async fn add_team_member(
            &self,
            _: AddMemberArgs,
        ) -> Result<TeamMemberSummary, TeamClientError> {
            unreachable!()
        }
        async fn remove_team_member(
            &self,
            _: Uuid,
            _: Option<Uuid>,
            _: Option<String>,
        ) -> Result<bool, TeamClientError> {
            unreachable!()
        }
        async fn change_team_member_role(
            &self,
            _: Uuid,
            _: Option<Uuid>,
            _: Option<String>,
            _: String,
        ) -> Result<TeamMemberSummary, TeamClientError> {
            unreachable!()
        }
        async fn share_project(
            &self,
            _: ShareProjectArgs,
        ) -> Result<SharedProjectSummary, TeamClientError> {
            unreachable!()
        }
        async fn unshare_project(&self, _: Uuid, _: Uuid) -> Result<bool, TeamClientError> {
            unreachable!()
        }
        async fn share_issue(&self, _: ShareIssueArgs) -> Result<(), TeamClientError> {
            unreachable!()
        }
        async fn leave_workspace(&self) -> Result<bool, TeamClientError> {
            unreachable!()
        }
    }

    fn sample_snapshot() -> TeamSnapshot {
        let ws = Uuid::new_v4();
        TeamSnapshot {
            workspace_id: Some(ws),
            seq: 1,
            teams: vec![TeamSummary {
                id: Uuid::new_v4(),
                identifier: "engineering".into(),
                name: "Engineering".into(),
                description: None,
                color: None,
                owner_account_id: Uuid::new_v4(),
            }],
            team_members: vec![],
            workspace_members: vec![],
            pending_invites: vec![],
            shared_projects: vec![],
            projects: vec![],
            team_scope: vec![],
        }
    }

    #[tokio::test]
    async fn refresh_calls_backend_and_persists() {
        let dir = TempDir::new().unwrap();
        let mock = Arc::new(MockClient::new(sample_snapshot()));
        let cache = TeamCache::new(dir.path().to_path_buf(), mock.clone());
        cache.refresh().await.unwrap();
        assert_eq!(mock.bootstrap_calls.load(Ordering::SeqCst), 1);
        // Cache file exists on disk.
        let p = dir.path().join("teams.cache.json");
        assert!(p.exists());
    }

    #[tokio::test]
    async fn get_or_refresh_short_circuits_when_fresh() {
        let dir = TempDir::new().unwrap();
        let snap = sample_snapshot();
        let ws = snap.workspace_id;
        let mock = Arc::new(MockClient::new(snap));
        let cache = TeamCache::new(dir.path().to_path_buf(), mock.clone());
        cache.refresh().await.unwrap();
        // Same workspace, within TTL → no extra refresh.
        cache.get_or_refresh(ws).await.unwrap();
        assert_eq!(mock.bootstrap_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn workspace_mismatch_forces_refresh() {
        let dir = TempDir::new().unwrap();
        let mock = Arc::new(MockClient::new(sample_snapshot()));
        let cache = TeamCache::new(dir.path().to_path_buf(), mock.clone());
        cache.refresh().await.unwrap();
        // Different workspace_id forces a refetch.
        cache.get_or_refresh(Some(Uuid::new_v4())).await.unwrap();
        assert_eq!(mock.bootstrap_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn find_team_resolves_by_identifier() {
        let dir = TempDir::new().unwrap();
        let mock = Arc::new(MockClient::new(sample_snapshot()));
        let cache = TeamCache::new(dir.path().to_path_buf(), mock);
        cache.refresh().await.unwrap();
        let team = cache.find_team("engineering").await.unwrap();
        assert_eq!(team.identifier, "engineering");
        assert!(cache.find_team("nope").await.is_none());
    }
}
