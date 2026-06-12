// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! This is the **constraint**, not the **score**. It defines *what* a trust layer
//! may do (the auditable public path); *how much* trust there is (the evidence
//! store + scoring) is the proprietary moat and lives outside this crate.
//!
//! The pieces:
//! - [`TrustAdjuster`] — the seam an Atlas scorer implements. Its only output is
//!   [`AskOutcome`]; it can **never** name `Allow`/`Deny`, so it cannot create a
//!   permission or turn a deny into an allow.
//! - [`NoopTrustAdjuster`] — the OSS default (mirrors `NoopForgeBuilder`): never
//!   promotes. The enterprise build swaps in the scorer; **OSS stays inert.**
//! - [`UnlockStore`] — the one-time human approvals per `(agent, project,
//!   capability)`. The **only** persisted trust authority — accumulating evidence
//!   can never unlock a sensitive capability, only a recorded human entry can.
//! - [`apply_trust`] — the framework that applies promotions. It alone guarantees
//!   the invariants: `Deny`/`Allow` untouched, sensitive needs an unlock, an
//!   untrusted scope promotes nothing. The adjuster cannot weaken these.
//!
//! **Inert in V1.** Nothing in the cage / `orkia-sh` decision path calls this with
//! a real adjuster; `orkia-cage` wires it with [`NoopTrustAdjuster`], which leaves
//! every policy unchanged. It exists so Atlas attaches later with zero migration.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::policy::{AskOutcome, Policy, Sensitivity, Verdict};

/// raw `cwd`, so a subdir `cd` cannot move trust scope. When no stable id is
/// resolvable the scope is untrusted and nothing promotes (see [`TrustScope`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectId(pub String);

/// Resolve a stable [`ProjectId`] by walking up from `cwd` for a `.git` entry
/// (dir or file — repos and worktrees both). Returns the git-root path as the id,
/// Read-only filesystem walk.
pub fn resolve_project_id(cwd: &Path) -> Option<ProjectId> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Some(ProjectId(d.to_string_lossy().into_owned()));
        }
        dir = d.parent();
    }
    None
}

/// capability.name)`. Only ever constructed for a trusted scope (concrete agent +
/// project), so the adjuster never sees a phantom-agent or unresolved-project key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrustKey {
    pub agent: String,
    pub project: ProjectId,
    pub capability: String,
}

/// scope whose agent is empty **or** whose project is unresolved (`None`) is
///
/// (The empty-agent guard matters because the cage sources the agent from
/// `ORKIA_AGENT_NAME`, which may be unset; an empty agent must never collapse all
/// keys under one phantom agent or match an unlock recorded for `""`.)
#[derive(Debug, Clone)]
pub struct TrustScope {
    pub agent: String,
    pub project: Option<ProjectId>,
}

impl TrustScope {
    /// The trusted `(agent, project)` pair, or `None` when the scope is untrusted
    /// (empty/whitespace agent or unresolved project). Promotion is considered
    /// only for a `Some`.
    fn trusted(&self) -> Option<(&str, &ProjectId)> {
        let agent = self.agent.trim();
        match (agent.is_empty(), self.project.as_ref()) {
            (false, Some(project)) => Some((agent, project)),
            _ => None,
        }
    }
}

/// answers whether evidence warrants auto-promotion, as an [`AskOutcome`].
///
/// It returns `AskOutcome` — **not** [`Verdict`] — so it can never name
/// `Allow`/`Deny` directly. The structural proof (a trust layer cannot produce a
/// verdict): an impl returning `Verdict` does not compile —
///
/// ```compile_fail
/// use orkia_shell_types::{TrustAdjuster, TrustKey, Sensitivity, Verdict};
/// struct Bad;
/// impl TrustAdjuster for Bad {
///     // ERROR: expected `AskOutcome`, found `Verdict`
///     fn adjust(&self, _k: &TrustKey, _s: Sensitivity) -> Verdict { Verdict::Allow }
/// }
/// ```
///
/// …while a well-typed adjuster (the only legal shape) compiles:
///
/// ```
/// use orkia_shell_types::{TrustAdjuster, TrustKey, Sensitivity, AskOutcome};
/// struct Scorer;
/// impl TrustAdjuster for Scorer {
///     fn adjust(&self, _k: &TrustKey, _s: Sensitivity) -> AskOutcome { AskOutcome::Auto }
/// }
/// ```
pub trait TrustAdjuster: Send + Sync {
    fn adjust(&self, key: &TrustKey, sensitivity: Sensitivity) -> AskOutcome;

    /// `Ask`-tier capabilities whose evidence has crossed the auto-promotion
    /// threshold but which carry no human unlock yet — the cold-review list the
    /// human disposes via `trust unlock`. The *computation* is the proprietary
    /// scorer's (it weighs evidence); the **default is empty** so the OSS path
    /// surfaces nothing. This only proposes — it never grants (an unlock still
    /// requires the recorded human authority in [`apply_trust`]).
    fn eligibility(
        &self,
        _policy: &Policy,
        _scope: &TrustScope,
        _unlocks: &UnlockStore,
    ) -> Vec<TrustKey> {
        Vec::new()
    }
}

/// The OSS default (mirrors `NoopForgeBuilder`): never promotes anything. The
/// enterprise build replaces it with the scoring adjuster; the OSS path stays
/// inert, so `apply_trust` with this returns the policy unchanged.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopTrustAdjuster;

impl TrustAdjuster for NoopTrustAdjuster {
    fn adjust(&self, _key: &TrustKey, _sensitivity: Sensitivity) -> AskOutcome {
        AskOutcome::Ask // no promotion, ever
    }
}

/// One recorded human approval — a `(agent, project, capability)` triple.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct UnlockEntry {
    agent: String,
    project: String,
    capability: String,
}

/// authority: a sensitive capability can be auto-promoted only when a matching
/// entry exists here — accumulating evidence alone can never unlock it. File-backed
/// JSON, auditable. (Granting an unlock also emits a SEAL event at the approval
/// site in `orkia-shell`; this store persists + queries the authority itself.)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnlockStore {
    #[serde(default)]
    unlocks: Vec<UnlockEntry>,
}

impl UnlockStore {
    /// An empty store — no unlocks, so no sensitive capability promotes.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load from `path`. A missing, unreadable, or corrupt file → **empty** store
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// True iff a human unlock is recorded for this exact `(agent, project,
    /// capability)` triple — keyed on the full triple, so an unlock in one project
    pub fn has(&self, key: &TrustKey) -> bool {
        self.unlocks.iter().any(|u| {
            u.agent == key.agent && u.project == key.project.0 && u.capability == key.capability
        })
    }

    /// Record an unlock for `key` (idempotent). Persisting is [`UnlockStore::save`].
    pub fn record(&mut self, key: &TrustKey) {
        if !self.has(key) {
            self.unlocks.push(UnlockEntry {
                agent: key.agent.clone(),
                project: key.project.0.clone(),
                capability: key.capability.clone(),
            });
        }
    }

    /// Revoke the unlock for `key` (idempotent). Returns `true` if an entry was
    /// removed. The `orkia trust lock` counterpart to [`UnlockStore::record`] —
    /// a human can withdraw a durable grant, and the next spawn stops promoting
    /// that sensitive capability. Persisting is [`UnlockStore::save`].
    pub fn remove(&mut self, key: &TrustKey) -> bool {
        let before = self.unlocks.len();
        self.unlocks.retain(|u| {
            !(u.agent == key.agent && u.project == key.project.0 && u.capability == key.capability)
        });
        self.unlocks.len() != before
    }

    /// Persist to `path` (creating parent dirs).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, raw)
    }
}

/// One surfaced eligibility signal — a `(agent, project, capability)` the scorer
/// reported as evidence-eligible but not yet unlocked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingEntry {
    agent: String,
    project: String,
    capability: String,
}

/// sensitive capabilities whose evidence has crossed the auto-promotion threshold
/// but which carry no human unlock yet. **Derived, never authority** — recomputed
/// every spawn (the cage rewrites the current scope's entries), a cold-review list
/// the human reads via `trust pending`. It grants nothing; only a recorded
/// [`UnlockStore`] entry does. File-backed JSON, auditable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingStore {
    #[serde(default)]
    pending: Vec<PendingEntry>,
}

impl PendingStore {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load from `path`; a missing/unreadable/corrupt file → empty (it is only a
    /// review cache, so a fault simply shows nothing pending).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Replace the entries for `(agent, project)` with `keys` — recompute-at-spawn
    /// semantics: each spawn overwrites only its own scope, leaving other agents'
    /// and projects' signals intact. Returns `true` if anything changed (so the
    /// caller can skip a pointless write — keeping the OSS/Noop path inert).
    pub fn update_scope(&mut self, agent: &str, project: &ProjectId, keys: &[TrustKey]) -> bool {
        let mut next: Vec<PendingEntry> = self
            .pending
            .iter()
            .filter(|e| !(e.agent == agent && e.project == project.0))
            .cloned()
            .collect();
        for k in keys {
            next.push(PendingEntry {
                agent: k.agent.clone(),
                project: k.project.0.clone(),
                capability: k.capability.clone(),
            });
        }
        // Order-insensitive comparison via sort keys would be cleaner, but the
        // per-scope rebuild keeps ordering stable enough; compare directly.
        let changed = next != self.pending;
        if changed {
            self.pending = next;
        }
        changed
    }

    /// Persist to `path` (creating parent dirs).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, raw)
    }

    /// True iff `key` is currently a surfaced eligibility signal.
    pub fn has(&self, key: &TrustKey) -> bool {
        self.pending.iter().any(|e| {
            e.agent == key.agent && e.project == key.project.0 && e.capability == key.capability
        })
    }
}

/// `Policy` clone with `Ask`→auto-`Allow` promotions applied **within already-
/// granted bounds**. The framework — not the adjuster — guarantees every invariant:
///
/// - the adjuster is consulted **only** for capabilities whose base verdict is
///   `Ask`; `Allow`/`Deny` pass through untouched, so `Deny`→`Allow` is
///   unrepresentable here as well as in the [`crate::PolicyDecision`] type;
/// - a **benign** capability promotes on `AskOutcome::Auto`; a **sensitive** one
///   promotes on `Auto` **only if** a human unlock is recorded — so accumulating
///   evidence can never, by itself, unlock a sensitive capability (no sleeper);
/// - an **untrusted scope** (empty agent or unresolved project) promotes nothing.
///
/// Inert in V1: [`NoopTrustAdjuster`] returns `Ask` for everything, so the result
/// equals `base`.
pub fn apply_trust(
    base: &Policy,
    scope: &TrustScope,
    adjuster: &dyn TrustAdjuster,
    unlocks: &UnlockStore,
) -> Policy {
    let mut out = base.clone();
    let Some((agent, project)) = scope.trusted() else {
        return out; // untrusted scope → no promotion (fail-closed)
    };
    for cap in &mut out.capabilities {
        // Allow/Deny are terminal — the adjuster is never even consulted for them.
        if cap.verdict != Verdict::Ask {
            continue;
        }
        let key = TrustKey {
            agent: agent.to_string(),
            project: project.clone(),
            capability: cap.name.clone(),
        };
        let promote = match adjuster.adjust(&key, cap.sensitivity) {
            AskOutcome::Ask => false,
            AskOutcome::Auto => match cap.sensitivity {
                Sensitivity::Benign => true,
                // Sensitive: evidence (Auto) is necessary but NOT sufficient —
                // a recorded human unlock is required (anti-sleeper).
                Sensitivity::Sensitive => unlocks.has(&key),
            },
        };
        if promote {
            cap.verdict = Verdict::Allow; // only ever Ask → Allow, never touches Deny
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Capability, ClassCaps, WorkspaceScope};

    /// A test adjuster with a fixed answer — stands in for the proprietary scorer.
    struct Fixed(AskOutcome);
    impl TrustAdjuster for Fixed {
        fn adjust(&self, _k: &TrustKey, _s: Sensitivity) -> AskOutcome {
            self.0
        }
    }

    fn cap(name: &str, verdict: Verdict, sensitivity: Sensitivity) -> Capability {
        Capability {
            name: name.into(),
            matches: vec![format!("{name}*")],
            verdict,
            sensitivity,
        }
    }

    /// Base policy: one of each tier/sensitivity combination that matters.
    fn base() -> Policy {
        Policy {
            default_verdict: Verdict::Ask,
            caps: ClassCaps::default(),
            workspace: WorkspaceScope { root: ".".into() },
            capabilities: vec![
                cap("git.commit", Verdict::Ask, Sensitivity::Benign),
                cap("git.push", Verdict::Ask, Sensitivity::Sensitive),
                cap("git.unannotated", Verdict::Ask, Sensitivity::Sensitive), // default
                cap("git.status", Verdict::Allow, Sensitivity::Sensitive),
                cap("rm.rf", Verdict::Deny, Sensitivity::Benign),
            ],
        }
    }

    fn scope(project: &str) -> TrustScope {
        TrustScope {
            agent: "faye".into(),
            project: Some(ProjectId(project.into())),
        }
    }

    fn verdict_of(p: &Policy, name: &str) -> Verdict {
        p.capabilities
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("missing cap {name}"))
            .verdict
    }

    fn key(agent: &str, project: &str, capability: &str) -> TrustKey {
        TrustKey {
            agent: agent.into(),
            project: ProjectId(project.into()),
            capability: capability.into(),
        }
    }

    // Deny/Allow are never touched, even when the adjuster says Auto.
    #[test]
    fn deny_and_allow_are_never_touched() {
        let out = apply_trust(
            &base(),
            &scope("A"),
            &Fixed(AskOutcome::Auto),
            &UnlockStore::empty(),
        );
        assert_eq!(verdict_of(&out, "git.status"), Verdict::Allow); // unchanged
        assert_eq!(verdict_of(&out, "rm.rf"), Verdict::Deny); // unchanged — never promoted
    }

    // Benign Ask auto-promotes on Auto.
    #[test]
    fn benign_ask_auto_promotes() {
        let out = apply_trust(
            &base(),
            &scope("A"),
            &Fixed(AskOutcome::Auto),
            &UnlockStore::empty(),
        );
        assert_eq!(verdict_of(&out, "git.commit"), Verdict::Allow);
    }

    // a sensitive Ask needs a recorded human unlock; Auto alone is not enough.
    #[test]
    fn sensitive_ask_needs_unlock() {
        // No unlock → stays Ask despite Auto.
        let out = apply_trust(
            &base(),
            &scope("A"),
            &Fixed(AskOutcome::Auto),
            &UnlockStore::empty(),
        );
        assert_eq!(verdict_of(&out, "git.push"), Verdict::Ask);

        // Record the unlock → promotes.
        let mut store = UnlockStore::empty();
        store.record(&key("faye", "A", "git.push"));
        let out = apply_trust(&base(), &scope("A"), &Fixed(AskOutcome::Auto), &store);
        assert_eq!(verdict_of(&out, "git.push"), Verdict::Allow);
    }

    // the sleeper: no number of Auto evaluations promotes a sensitive cap
    // without the human unlock.
    #[test]
    fn sleeper_sensitive_never_promotes_without_unlock() {
        for n in 0..1000 {
            let out = apply_trust(
                &base(),
                &scope("A"),
                &Fixed(AskOutcome::Auto),
                &UnlockStore::empty(),
            );
            assert_eq!(
                verdict_of(&out, "git.push"),
                Verdict::Ask,
                "sensitive cap promoted on session {n} without a human unlock — sleeper!"
            );
        }
    }

    // an unlock for (faye, A, git.push) does not satisfy project B.
    #[test]
    fn scope_isolation_unlock_does_not_cross_projects() {
        let mut store = UnlockStore::empty();
        store.record(&key("faye", "A", "git.push"));
        // Same agent + capability, different project → not unlocked.
        let out = apply_trust(&base(), &scope("B"), &Fixed(AskOutcome::Auto), &store);
        assert_eq!(verdict_of(&out, "git.push"), Verdict::Ask);
        // And the matching project IS unlocked.
        let out = apply_trust(&base(), &scope("A"), &Fixed(AskOutcome::Auto), &store);
        assert_eq!(verdict_of(&out, "git.push"), Verdict::Allow);
    }

    // an un-annotated capability defaults to Sensitive: human-gated.
    #[test]
    fn unannotated_capability_is_fail_closed_sensitive() {
        let out = apply_trust(
            &base(),
            &scope("A"),
            &Fixed(AskOutcome::Auto),
            &UnlockStore::empty(),
        );
        assert_eq!(verdict_of(&out, "git.unannotated"), Verdict::Ask);
    }

    // an untrusted scope (empty agent OR unresolved project) promotes nothing,
    // even a benign cap the adjuster wants to auto-promote.
    #[test]
    fn untrusted_scope_promotes_nothing() {
        let empty_agent = TrustScope {
            agent: "   ".into(),
            project: Some(ProjectId("A".into())),
        };
        let out = apply_trust(
            &base(),
            &empty_agent,
            &Fixed(AskOutcome::Auto),
            &UnlockStore::empty(),
        );
        assert_eq!(
            verdict_of(&out, "git.commit"),
            Verdict::Ask,
            "empty agent must not promote"
        );

        let no_project = TrustScope {
            agent: "faye".into(),
            project: None,
        };
        let out = apply_trust(
            &base(),
            &no_project,
            &Fixed(AskOutcome::Auto),
            &UnlockStore::empty(),
        );
        assert_eq!(
            verdict_of(&out, "git.commit"),
            Verdict::Ask,
            "no project must not promote"
        );
    }

    // The OSS default is inert: apply_trust changes nothing.
    #[test]
    fn noop_adjuster_is_inert() {
        let b = base();
        let out = apply_trust(&b, &scope("A"), &NoopTrustAdjuster, &UnlockStore::empty());
        assert_eq!(out, b);
    }

    #[test]
    fn resolve_project_id_finds_git_root_and_none_outside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        // a subdir resolves to the SAME root id (a `cd` cannot move scope).
        assert_eq!(
            resolve_project_id(&sub),
            Some(ProjectId(root.to_string_lossy().into_owned()))
        );
        // outside any repo → None (untrusted).
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(resolve_project_id(bare.path()), None);
    }

    #[test]
    fn unlock_store_round_trips_and_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust/unlocks.json");
        // missing file → empty (fail-closed).
        assert!(!UnlockStore::load(&path).has(&key("faye", "A", "git.push")));

        let mut store = UnlockStore::empty();
        store.record(&key("faye", "A", "git.push"));
        store.save(&path).unwrap();
        let loaded = UnlockStore::load(&path);
        assert!(loaded.has(&key("faye", "A", "git.push")));
        assert!(!loaded.has(&key("faye", "A", "git.commit")));
    }

    // `lock` revokes a grant: remove is idempotent and scoped to the full triple.
    #[test]
    fn unlock_store_remove_revokes_the_grant() {
        let mut store = UnlockStore::empty();
        store.record(&key("faye", "A", "git.push"));
        assert!(store.has(&key("faye", "A", "git.push")));
        // Removing a different project's key is a no-op.
        assert!(!store.remove(&key("faye", "B", "git.push")));
        assert!(store.has(&key("faye", "A", "git.push")));
        // Removing the exact key revokes it; a second remove is a no-op.
        assert!(store.remove(&key("faye", "A", "git.push")));
        assert!(!store.has(&key("faye", "A", "git.push")));
        assert!(!store.remove(&key("faye", "A", "git.push")));
    }

    // PendingStore: per-scope rewrite (recompute-at-spawn), change detection,
    // and isolation across scopes.
    #[test]
    fn pending_store_rewrites_per_scope_and_isolates() {
        let a = ProjectId("A".into());
        let b = ProjectId("B".into());
        let mut p = PendingStore::empty();

        // First surfacing for (faye, A) → changed.
        assert!(p.update_scope("faye", &a, &[key("faye", "A", "git.push")]));
        assert!(p.has(&key("faye", "A", "git.push")));
        // Identical resurfacing → no change (keeps the Noop path inert).
        assert!(!p.update_scope("faye", &a, &[key("faye", "A", "git.push")]));

        // A different scope is recorded independently.
        p.update_scope("kane", &b, &[key("kane", "B", "git.push")]);

        // Clearing (faye, A) removes only its entry; (kane, B) survives.
        assert!(p.update_scope("faye", &a, &[]));
        assert!(!p.has(&key("faye", "A", "git.push")));
        assert!(p.has(&key("kane", "B", "git.push")));
    }
}
