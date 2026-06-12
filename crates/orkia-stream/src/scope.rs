// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `ScopeGate` — fail-closed scope filter.
//!
//!
//! | Effective scope | Authenticated | Team member | Action          |
//! |-----------------|---------------|-------------|-----------------|
//! | private         | any           | any         | drop silently   |
//! | team            | yes           | yes         | publish         |
//! | team            | yes           | no          | drop with WARN  |
//! | team            | no            | any         | drop with WARN  |
//! | public          | yes           | any         | publish         |
//! | public          | no            | any         | drop with WARN  |
//!
//! The gate consults a live [`AuthContext`] snapshot on every decision so
//! that a mid-session `orkia auth login` or `orkia team join` immediately
//! changes outcomes. Per-artifact, per-session warning dedup keeps the
//! log readable when a chain emits many same-reason drops in a row.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use orkia_shell_types::journal::JournalEnvelope;
use orkia_shell_types::scope::Scope;
use orkia_shell_types::seal::SealRecord;

use crate::auth::{AuthContext, AuthSnapshot};

/// One decision for one event.
#[derive(Debug, Clone)]
pub struct Decision {
    pub publish: bool,
    pub scope: Option<Scope>,
    pub warn_reason: Option<String>,
}

impl Decision {
    pub fn scope_label(&self) -> &'static str {
        match self.scope {
            Some(Scope::Public) => "public",
            Some(Scope::Team) => "team",
            Some(Scope::Private) => "private",
            None => "private",
        }
    }
}

pub struct ScopeGate {
    auth: Arc<AuthContext>,
    /// Set of `{artifact_id}:{reason}` keys already warned for this
    /// session. Dedup is per `(artifact, reason)` pair so a chain that
    /// emits twenty `scope=team` records under a session with no team
    /// membership produces exactly one warning.
    warned: Mutex<HashSet<String>>,
}

impl ScopeGate {
    pub fn new(auth: Arc<AuthContext>) -> Self {
        Self {
            auth,
            warned: Mutex::new(HashSet::new()),
        }
    }

    /// Decide whether a SealRecord should leave the machine.
    /// `artifact_id` is used as the dedup key for warning logging —
    /// callers typically pass the chain id (project name or
    /// `job:<agent>/<id>`).
    pub fn evaluate_seal(&self, record: &SealRecord, artifact_id: &str) -> Decision {
        let raw = record.detail.get("scope").and_then(|v| v.as_str());
        self.decide(raw, &record.event_type, artifact_id)
    }

    /// Decide for a JournalEnvelope. The scope is sought in
    /// `extra["scope"]` (the catch-all map). Absent ⇒ private (drop).
    pub fn evaluate_journal(&self, env: &JournalEnvelope, artifact_id: &str) -> Decision {
        let raw = env.extra.get("scope").and_then(|v| v.as_str());
        let event_type_str =
            serde_json::to_string(&env.event_type).unwrap_or_else(|_| "\"hook\"".into());
        self.decide(raw, &event_type_str, artifact_id)
    }

    fn decide(&self, raw: Option<&str>, event_type: &str, artifact_id: &str) -> Decision {
        let scope = match raw {
            Some(s) => match Scope::parse(s) {
                Ok(s) => Some(s),
                Err(_) => {
                    return Decision {
                        publish: false,
                        scope: None,
                        warn_reason: self.warn_once(
                            artifact_id,
                            "malformed-scope",
                            format!(
                                "malformed scope '{s}' on event {event_type}; treating as private",
                            ),
                        ),
                    };
                }
            },
            None => None,
        };

        let auth: AuthSnapshot = self.auth.snapshot();

        match scope.unwrap_or(Scope::Private) {
            // Row 1: private always drops silently.
            Scope::Private => Decision {
                publish: false,
                scope,
                warn_reason: None,
            },
            // Rows 2-4: team requires authenticated AND member.
            Scope::Team => self.decide_team(scope, &auth, event_type, artifact_id),
            // Rows 5-6: public requires authenticated.
            Scope::Public => self.decide_public(scope, &auth, event_type, artifact_id),
        }
    }

    fn decide_team(
        &self,
        scope: Option<Scope>,
        auth: &AuthSnapshot,
        event_type: &str,
        artifact_id: &str,
    ) -> Decision {
        if !auth.authenticated {
            Decision {
                publish: false,
                scope,
                warn_reason: self.warn_once(
                    artifact_id,
                    "team-no-auth",
                    format!(
                        "event {event_type}: scope=team declared but session is not \
                         authenticated; run 'orkia auth login' first",
                    ),
                ),
            }
        } else if !auth.has_any_team_membership {
            Decision {
                publish: false,
                scope,
                warn_reason: self.warn_once(
                    artifact_id,
                    "team-no-membership",
                    format!(
                        "event {event_type}: scope=team declared but you are not a \
                         member of any team; run 'orkia team join <invite>' first",
                    ),
                ),
            }
        } else {
            Decision {
                publish: true,
                scope,
                warn_reason: None,
            }
        }
    }

    fn decide_public(
        &self,
        scope: Option<Scope>,
        auth: &AuthSnapshot,
        event_type: &str,
        artifact_id: &str,
    ) -> Decision {
        if !auth.authenticated {
            Decision {
                publish: false,
                scope,
                warn_reason: self.warn_once(
                    artifact_id,
                    "public-no-auth",
                    format!(
                        "event {event_type}: scope=public declared but session is not \
                         authenticated; run 'orkia auth login' first",
                    ),
                ),
            }
        } else {
            Decision {
                publish: true,
                scope,
                warn_reason: None,
            }
        }
    }

    /// Returns `Some(message)` only the first time `(artifact_id, reason)`
    /// is observed in this session — subsequent occurrences return `None`
    /// so the caller logs nothing.
    fn warn_once(&self, artifact_id: &str, reason: &str, message: String) -> Option<String> {
        let key = format!("{artifact_id}:{reason}");
        let mut warned = self.warned.lock().unwrap_or_else(|p| p.into_inner());
        if warned.insert(key) {
            Some(message)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_auth::{AuthError, AuthEventSink, AuthProvider, SessionInfo};
    use orkia_shell_types::journal::types::EventType;

    // ─── test doubles ────────────────────────────────────────────────────

    struct FixedAuthProvider {
        bearer: Option<String>,
    }
    impl AuthProvider for FixedAuthProvider {
        fn login(&self, _: &mut dyn AuthEventSink) -> Result<SessionInfo, AuthError> {
            unreachable!("test double: login not exercised")
        }
        fn logout(&self) -> Result<(), AuthError> {
            unreachable!("test double: logout not exercised")
        }
        fn current(&self) -> Option<SessionInfo> {
            None
        }
        fn bearer(&self) -> Option<String> {
            self.bearer.clone()
        }
    }

    fn make_auth(authenticated: bool, in_team: bool) -> Arc<AuthContext> {
        let provider = Arc::new(FixedAuthProvider {
            bearer: authenticated.then(|| "test-token".to_string()),
        });
        let probe: crate::auth::TeamMembershipProbe = Arc::new(move || in_team);
        Arc::new(AuthContext::new(provider).with_team_probe(probe))
    }

    fn seal_record(event_type: &str, scope: Option<&str>) -> SealRecord {
        let mut detail = serde_json::Map::new();
        if let Some(s) = scope {
            detail.insert("scope".into(), serde_json::Value::String(s.into()));
        }
        SealRecord {
            seq: 0,
            timestamp: "2026-05-26T00:00:00+00:00".into(),
            event_type: event_type.into(),
            detail: serde_json::Value::Object(detail),
            hash: "h".into(),
            prev_hash: "p".into(),
            rfc_id: None,
        }
    }

    fn journal_with_scope(scope: Option<&str>) -> JournalEnvelope {
        let mut e = JournalEnvelope::now(EventType::Hook);
        if let Some(s) = scope {
            e.extra
                .insert("scope".into(), serde_json::Value::String(s.into()));
        }
        e
    }

    // ─── decision table — six cells ─────────────────────────────────────

    #[test]
    fn private_drops_silently_regardless_of_auth() {
        for (authed, in_team) in [(true, true), (false, false), (true, false), (false, true)] {
            let g = ScopeGate::new(make_auth(authed, in_team));
            let d = g.evaluate_seal(&seal_record("rfc.create", Some("private")), "p");
            assert!(!d.publish);
            assert!(d.warn_reason.is_none(), "private must drop silently");
        }
    }

    #[test]
    fn team_authenticated_with_membership_publishes() {
        let g = ScopeGate::new(make_auth(true, true));
        let d = g.evaluate_seal(&seal_record("rfc.create", Some("team")), "p");
        assert!(d.publish);
        assert!(d.warn_reason.is_none());
    }

    #[test]
    fn team_authenticated_no_membership_drops_with_warn() {
        let g = ScopeGate::new(make_auth(true, false));
        let d = g.evaluate_seal(&seal_record("rfc.create", Some("team")), "p");
        assert!(!d.publish);
        let msg = d.warn_reason.expect("must warn");
        assert!(msg.contains("not a member"), "got: {msg}");
    }

    #[test]
    fn team_unauthenticated_drops_with_warn() {
        let g = ScopeGate::new(make_auth(false, false));
        let d = g.evaluate_seal(&seal_record("rfc.create", Some("team")), "p");
        assert!(!d.publish);
        let msg = d.warn_reason.expect("must warn");
        assert!(msg.contains("not authenticated"), "got: {msg}");
    }

    #[test]
    fn public_authenticated_publishes() {
        let g = ScopeGate::new(make_auth(true, false));
        let d = g.evaluate_seal(&seal_record("rfc.create", Some("public")), "p");
        assert!(d.publish);
        assert!(d.warn_reason.is_none());
    }

    #[test]
    fn public_unauthenticated_drops_with_warn() {
        let g = ScopeGate::new(make_auth(false, false));
        let d = g.evaluate_seal(&seal_record("rfc.create", Some("public")), "p");
        assert!(!d.publish);
        let msg = d.warn_reason.expect("must warn");
        assert!(msg.contains("not authenticated"), "got: {msg}");
    }

    // ─── dedup — one warning per artifact per session ───────────────────

    #[test]
    fn warning_is_deduped_per_artifact_and_reason() {
        let g = ScopeGate::new(make_auth(false, false));
        let r = seal_record("rfc.create", Some("team"));
        let first = g.evaluate_seal(&r, "rfc:foo");
        let second = g.evaluate_seal(&r, "rfc:foo");
        assert!(first.warn_reason.is_some(), "first decision warns");
        assert!(
            second.warn_reason.is_none(),
            "second same-(artifact,reason) suppressed"
        );
    }

    #[test]
    fn different_artifacts_each_warn_once() {
        let g = ScopeGate::new(make_auth(false, false));
        let r = seal_record("rfc.create", Some("team"));
        assert!(g.evaluate_seal(&r, "rfc:foo").warn_reason.is_some());
        assert!(g.evaluate_seal(&r, "rfc:bar").warn_reason.is_some());
    }

    // ─── existing coverage (parse errors + journal path) ─────────────────

    #[test]
    fn malformed_scope_drops_and_warns_once() {
        let g = ScopeGate::new(make_auth(true, true));
        let d = g.evaluate_seal(&seal_record("rfc.create", Some("internal")), "p");
        assert!(!d.publish);
        assert!(d.warn_reason.is_some());
        // Second hit deduped.
        assert!(
            g.evaluate_seal(&seal_record("rfc.create", Some("internal")), "p")
                .warn_reason
                .is_none(),
        );
    }

    #[test]
    fn journal_missing_scope_drops_silently() {
        let g = ScopeGate::new(make_auth(true, true));
        let d = g.evaluate_journal(&journal_with_scope(None), "art");
        assert!(!d.publish);
        assert!(d.warn_reason.is_none());
    }

    #[test]
    fn journal_public_authenticated_publishes() {
        let g = ScopeGate::new(make_auth(true, false));
        let d = g.evaluate_journal(&journal_with_scope(Some("public")), "art");
        assert!(d.publish);
    }
}
