// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Plan → capability resolution for the Orkia shell.
//!
//! The shell asks one question at a few well-defined moments — "what
//! is the user allowed to do right now?" — and this crate answers
//! it. Inputs come from any source that can answer "what is the
//! current account + plan?" via the [`PlanSource`] trait; outputs are
//! an opaque [`CapabilitySet`] plus a subscription mechanism so the
//! REPL can swap behaviour without restarting.
//!
//! Network refresh, server-side plan revocation, and grace-period
//! intentionally stays a pure-local mapping: it never opens a socket.
//!
//! Capabilities currently defined: `CognitiveRouting`,
//! `ContextCompression`, `CognitiveRouter`, `TeamPipeline`,
//! `SealAuditExtended`, and `ForgeBuild` (premium Forge build &
//! generation, gated from solo-pro upward).

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeSet;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

pub mod plan;

pub use plan::{Plan, capabilities_for_plan};

/// shell uses `CapabilitySet::has` checks to gate features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Capability {
    /// Local LLM-based intent classification via the kernel daemon.
    CognitiveRouting,
    /// Local embeddings + summarization for context compression.
    ContextCompression,
    /// Local/cloud routing arbitration (the "cognitive router").
    CognitiveRouter,
    /// Multi-agent pipeline coordinator (`@a | @b`).
    TeamPipeline,
    /// Extended SEAL retention + enterprise audit features.
    SealAuditExtended,
    /// Premium Forge build & generation feature. Gates the proprietary Forge build/generation backend. Available from solo-pro.
    ForgeBuild,
}

/// Immutable snapshot of the capabilities currently unlocked for the
/// signed-in user. Cheap to clone — it's a small `BTreeSet` behind an
/// `Arc`. Compare snapshots with `==` to detect changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    inner: Arc<BTreeSet<Capability>>,
}

impl CapabilitySet {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_capabilities<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        Self {
            inner: Arc::new(iter.into_iter().collect()),
        }
    }

    pub fn has(&self, cap: Capability) -> bool {
        self.inner.contains(&cap)
    }

    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.inner.iter().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

/// Minimal account snapshot the shell renders in `$whoami`/`$plan`.
/// Filled in from whichever [`PlanSource`] the binary wires up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSnapshot {
    pub display_name: String,
    pub email: String,
    pub plan: String,
}

/// Read-only source of the currently signed-in account's plan.
/// Implemented by any auth backend; the resolver consumes it without
/// caring what kind of backend produced the data.
pub trait PlanSource: Send + Sync + 'static {
    /// Snapshot of the active account, if any. `None` means anonymous.
    fn account(&self) -> Option<AccountSnapshot>;
}

/// Adapter from any [`orkia_auth::AuthProvider`] to [`PlanSource`].
/// Lets binaries reuse one wired `Arc<dyn AuthProvider>` for both
/// `$login` plumbing and capability resolution.
pub struct ProviderPlanSource {
    provider: Arc<dyn orkia_auth::AuthProvider>,
}

impl ProviderPlanSource {
    pub fn new(provider: Arc<dyn orkia_auth::AuthProvider>) -> Self {
        Self { provider }
    }
}

impl PlanSource for ProviderPlanSource {
    fn account(&self) -> Option<AccountSnapshot> {
        self.provider.current().map(|s| AccountSnapshot {
            display_name: s.display_name,
            email: s.email,
            plan: s.plan,
        })
    }
}

/// Callback fired by a resolver when the active capability set
/// changes (e.g. after `$login` or a token refresh). Must be cheap +
/// non-blocking; subscribers do their own work on a separate thread.
pub type CapabilityCallback = Box<dyn Fn(CapabilitySet) + Send + Sync + 'static>;

/// Read + observe the active capability set.
pub trait CapabilityResolver: Send + Sync + 'static {
    /// Snapshot of the current capabilities.
    fn current(&self) -> CapabilitySet;

    /// Currently signed-in account snapshot, if any.
    fn account(&self) -> Option<AccountSnapshot>;

    /// Force a re-read of the underlying source. Fires subscribers iff
    /// the resolved capability set has changed.
    fn refresh(&self);

    /// Register a change subscriber. The callback runs synchronously
    /// inside `refresh()` and inside `$login`/`$logout` triggers —
    /// keep it short.
    fn subscribe(&self, cb: CapabilityCallback);
}

/// Maps the plan reported by a [`PlanSource`] to a [`CapabilitySet`],
/// with on-demand and explicit refresh. Holds no network or async
/// machinery — the caller (typically the REPL) drives `refresh()`
/// from `$login` and from a periodic tick.
pub struct PlanResolver {
    source: Arc<dyn PlanSource>,
    state: RwLock<ResolverState>,
    subscribers: RwLock<Vec<CapabilityCallback>>,
}

struct ResolverState {
    capabilities: CapabilitySet,
    account: Option<AccountSnapshot>,
}

impl PlanResolver {
    /// Build the resolver and perform the first read. Errors from the
    /// source collapse to "no capabilities, no account" — the shell
    /// degrades to OSS defaults rather than failing to start.
    pub fn new(source: Arc<dyn PlanSource>) -> Self {
        let (account, capabilities) = read_once(&*source);
        Self {
            source,
            state: RwLock::new(ResolverState {
                capabilities,
                account,
            }),
            subscribers: RwLock::new(Vec::new()),
        }
    }

    /// Replace the active state with the result of a fresh read and
    /// fire subscribers if anything changed. Returns the new set.
    pub fn refresh_now(&self) -> CapabilitySet {
        let (account, capabilities) = read_once(&*self.source);
        let mut state = self.state.write();
        let changed = state.capabilities != capabilities;
        state.capabilities = capabilities.clone();
        state.account = account;
        drop(state);
        if changed {
            let subs = self.subscribers.read();
            for cb in subs.iter() {
                cb(capabilities.clone());
            }
        }
        capabilities
    }
}

impl CapabilityResolver for PlanResolver {
    fn current(&self) -> CapabilitySet {
        self.state.read().capabilities.clone()
    }

    fn account(&self) -> Option<AccountSnapshot> {
        self.state.read().account.clone()
    }

    fn refresh(&self) {
        let _ = self.refresh_now();
    }

    fn subscribe(&self, cb: CapabilityCallback) {
        self.subscribers.write().push(cb);
    }
}

fn read_once(source: &dyn PlanSource) -> (Option<AccountSnapshot>, CapabilitySet) {
    match source.account() {
        Some(snap) => {
            let plan = Plan::parse(&snap.plan);
            let caps = capabilities_for_plan(plan);
            (Some(snap), caps)
        }
        None => (None, CapabilitySet::empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemSource {
        snap: Mutex<Option<AccountSnapshot>>,
    }

    impl MemSource {
        fn set(&self, plan: &str) {
            *self.snap.lock().unwrap() = Some(AccountSnapshot {
                display_name: "k".into(),
                email: "k@example.com".into(),
                plan: plan.into(),
            });
        }
        fn clear(&self) {
            *self.snap.lock().unwrap() = None;
        }
    }

    impl PlanSource for MemSource {
        fn account(&self) -> Option<AccountSnapshot> {
            self.snap.lock().unwrap().clone()
        }
    }

    #[test]
    fn anonymous_has_no_capabilities() {
        let src: Arc<dyn PlanSource> = Arc::new(MemSource::default());
        let r = PlanResolver::new(src);
        assert!(r.current().is_empty());
        assert!(r.account().is_none());
    }

    #[test]
    fn solo_pro_unlocks_cognitive() {
        let src = Arc::new(MemSource::default());
        src.set("solo-pro");
        let r = PlanResolver::new(src.clone() as Arc<dyn PlanSource>);
        let caps = r.current();
        assert!(caps.has(Capability::CognitiveRouting));
        assert!(caps.has(Capability::ContextCompression));
        assert!(caps.has(Capability::CognitiveRouter));
        assert!(caps.has(Capability::ForgeBuild));
        assert!(!caps.has(Capability::TeamPipeline));
    }

    #[test]
    fn team_inherits_solo() {
        let src = Arc::new(MemSource::default());
        src.set("team");
        let r = PlanResolver::new(src as Arc<dyn PlanSource>);
        let caps = r.current();
        assert!(caps.has(Capability::CognitiveRouting));
        assert!(caps.has(Capability::TeamPipeline));
        assert!(caps.has(Capability::ForgeBuild));
        assert!(!caps.has(Capability::SealAuditExtended));
    }

    #[test]
    fn enterprise_has_everything() {
        let src = Arc::new(MemSource::default());
        src.set("enterprise");
        let r = PlanResolver::new(src as Arc<dyn PlanSource>);
        let caps = r.current();
        assert!(caps.has(Capability::CognitiveRouting));
        assert!(caps.has(Capability::TeamPipeline));
        assert!(caps.has(Capability::SealAuditExtended));
        assert!(caps.has(Capability::ForgeBuild));
    }

    #[test]
    fn empty_set_does_not_have_forge_build() {
        assert!(!CapabilitySet::empty().has(Capability::ForgeBuild));
    }

    #[test]
    fn forge_build_is_not_in_free() {
        assert!(!capabilities_for_plan(Plan::Free).has(Capability::ForgeBuild));
    }

    #[test]
    fn refresh_fires_subscribers_only_on_change() {
        let src = Arc::new(MemSource::default());
        let r = PlanResolver::new(src.clone() as Arc<dyn PlanSource>);
        let hits: Arc<Mutex<Vec<CapabilitySet>>> = Arc::new(Mutex::new(Vec::new()));
        let hits_cb = hits.clone();
        r.subscribe(Box::new(move |c| hits_cb.lock().unwrap().push(c)));

        src.set("solo-pro");
        r.refresh();
        assert_eq!(hits.lock().unwrap().len(), 1);

        // Same state — no second event.
        r.refresh();
        assert_eq!(hits.lock().unwrap().len(), 1);

        // Plan changed → fires.
        src.set("team");
        r.refresh();
        assert_eq!(hits.lock().unwrap().len(), 2);

        // Logout → fires.
        src.clear();
        r.refresh();
        assert_eq!(hits.lock().unwrap().len(), 3);
    }
}
