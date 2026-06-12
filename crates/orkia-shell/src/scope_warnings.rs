// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Session-scoped tracker for scope-related warnings.
//!
//! team membership is legal — the artifact stays local and acts as a
//! declarative intent — but the user should be told once. Surfacing
//! the same warning on every subsequent action would devolve into
//! spam, so we dedupe per-session by `(artifact_id, kind)`.
//!
//! `kind` is a free-form string so callers can distinguish, e.g.,
//! `"team-no-membership"` from a future `"public-no-auth"` warning.

use std::collections::HashSet;
use std::sync::Mutex;

/// Pre-built message bodies for known scope warnings. Centralised so
/// every call site emits the same wording.
pub mod messages {
    pub const TEAM_NO_MEMBERSHIP: &str = "\u{26a0} scope=team declared but you are not a member of any team.\n  Run `orkia team join <invite>` once you have an invite, then\n  scope=team artifacts will sync. Until then the scope is\n  declarative and the artifact stays local.";
}

#[derive(Default)]
pub struct ScopeWarningTracker {
    warned: Mutex<HashSet<String>>,
}

impl ScopeWarningTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if this `(artifact_id, kind)` was emitted for the
    /// first time (caller should render the warning), `false` if it
    /// has already fired in this session.
    pub fn should_warn(&self, artifact_id: &str, kind: &str) -> bool {
        let key = format!("{artifact_id}\u{0}{kind}");
        let mut set = match self.warned.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        set.insert(key)
    }

    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.warned.lock().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_warning_fires_subsequent_are_deduped() {
        let t = ScopeWarningTracker::new();
        assert!(t.should_warn("proj-1", "team-no-membership"));
        assert!(!t.should_warn("proj-1", "team-no-membership"));
        assert!(!t.should_warn("proj-1", "team-no-membership"));
        assert_eq!(t.count(), 1);
    }

    #[test]
    fn distinct_artifacts_warn_independently() {
        let t = ScopeWarningTracker::new();
        assert!(t.should_warn("proj-1", "team-no-membership"));
        assert!(t.should_warn("proj-2", "team-no-membership"));
        assert!(!t.should_warn("proj-1", "team-no-membership"));
        assert_eq!(t.count(), 2);
    }

    #[test]
    fn distinct_kinds_warn_independently() {
        let t = ScopeWarningTracker::new();
        assert!(t.should_warn("proj-1", "team-no-membership"));
        assert!(t.should_warn("proj-1", "public-no-auth"));
        assert_eq!(t.count(), 2);
    }
}
