// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Both entry modes use this: the Linux shell-shim (`orkia-sh -c …`) and the
//! macOS PreToolUse hook (`orkia-sh hook`). It recovers the command, loads the
//! policy, and resolves a verdict — **without** acting (each caller enforces +
//! records in its own way). Fail-closed (CLAUDE.md #8): an unrecoverable agent
//! envelope or an unavailable policy resolves to `Deny`.

use anyhow::{Context, Result};
use orkia_shell_types::{Policy, PolicyDecision};

use crate::decide::{Extracted, extract_command};

/// Absolute path to the policy TOML, injected by the cage launcher.
pub const POLICY_ENV: &str = "ORKIA_CAGE_POLICY";

/// A resolved decision about one command — the owned mirror of
/// [`orkia_shell_types::PolicyDecision`] (`capability`/`rule` are owned because
/// the borrowed `Policy` does not outlive evaluation, and a forced deny adds a
/// reason). `Allow`/`Deny` are terminal; `Ask` is the tier a future trust layer
/// would widen, so the enforcers gate it in its own arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow {
        command: String,
        capability: Option<String>,
        rule: Option<String>,
    },
    Deny {
        command: String,
        capability: Option<String>,
        rule: Option<String>,
        /// Why a `Deny` was forced regardless of policy (envelope/policy
        /// failure), for clearer operator messages. `None` for a policy deny.
        forced_reason: Option<&'static str>,
    },
    Ask {
        command: String,
        capability: Option<String>,
        rule: Option<String>,
    },
}

/// Resolve a decision for the raw `-c` / tool command string, loading the policy
/// from `ORKIA_CAGE_POLICY`. Never errors — failures collapse to a fail-closed
/// `Deny` with a `forced_reason`.
pub fn decide(raw_command: &str) -> Decision {
    match extract_command(raw_command) {
        Extracted::Unparseable => forced_deny(raw_command, "unparseable agent envelope"),
        Extracted::Command(command) => match load_policy() {
            Ok(policy) => evaluate(&command, &policy),
            Err(_) => forced_deny(&command, "policy unavailable"),
        },
    }
}

/// Pure evaluation of an already-extracted command against a loaded policy.
/// Split out from [`decide`] so it is testable without touching process env.
/// Tier-preserving: maps each [`PolicyDecision`] variant to the owned mirror.
///
/// is false the class is closed — every command is denied *before* any
/// `capabilities[]` rule is consulted (the `x`-on-a-directory rule). This is
/// fail-closed (CLAUDE.md #8) and matches the all-false `ClassCaps` default, so
/// a policy that omits `[caps]` runs nothing.
pub fn evaluate(command: &str, policy: &Policy) -> Decision {
    let command = command.to_string();
    if !policy.caps.exec {
        return Decision::Deny {
            command,
            capability: None,
            rule: None,
            forced_reason: Some("exec class disabled"),
        };
    }
    match policy.evaluate_match(&command) {
        PolicyDecision::Allow { capability, rule } => Decision::Allow {
            command,
            capability: capability.map(str::to_string),
            rule: rule.map(str::to_string),
        },
        PolicyDecision::Deny { capability, rule } => Decision::Deny {
            command,
            capability: capability.map(str::to_string),
            rule: rule.map(str::to_string),
            forced_reason: None,
        },
        PolicyDecision::Ask(a) => Decision::Ask {
            command,
            capability: a.capability.map(str::to_string),
            rule: a.rule.map(str::to_string),
        },
    }
}

fn forced_deny(command: &str, reason: &'static str) -> Decision {
    Decision::Deny {
        command: command.to_string(),
        capability: None,
        rule: None,
        forced_reason: Some(reason),
    }
}

/// Load the policy named by `ORKIA_CAGE_POLICY`. Absence or any read/parse error
/// is an `Err` — `decide` turns that into a fail-closed deny.
pub fn load_policy() -> Result<Policy> {
    let path = std::env::var(POLICY_ENV).with_context(|| format!("{POLICY_ENV} not set"))?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading policy {path}"))?;
    toml::from_str(&raw).with_context(|| format!("parsing policy {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = r#"
default_verdict = "allow"

[caps]
read = true
write = true
exec = true

[workspace]
root = "."

[[capabilities]]
name = "git.push"
matches = ["git push*"]
verdict = "deny"
"#;

    fn policy() -> Policy {
        toml::from_str(POLICY).expect("valid policy")
    }

    #[test]
    fn denies_matched_capability_with_rule() {
        match evaluate("git push origin main", &policy()) {
            Decision::Deny {
                command,
                capability,
                rule,
                forced_reason,
            } => {
                assert_eq!(command, "git push origin main");
                assert_eq!(capability.as_deref(), Some("git.push"));
                assert_eq!(rule.as_deref(), Some("git push*"));
                assert!(forced_reason.is_none());
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn allows_unmatched_under_default_allow() {
        match evaluate("ls -la", &policy()) {
            Decision::Allow { capability, .. } => assert!(capability.is_none()),
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[test]
    fn exec_off_closes_class_without_consulting_rules() {
        // exec=false denies even a command that would otherwise be allowed by
        // default_verdict, and never consults capabilities[] (capability/rule
        // are None, forced_reason names the class closure).
        let toml = r#"
default_verdict = "allow"

[caps]
exec = false

[workspace]
root = "."

[[capabilities]]
name = "git.commit"
matches = ["git commit*"]
verdict = "allow"
"#;
        let p: Policy = toml::from_str(toml).expect("valid policy");
        match evaluate("git commit -m x", &p) {
            Decision::Deny {
                capability,
                rule,
                forced_reason,
                ..
            } => {
                assert!(capability.is_none());
                assert!(rule.is_none());
                assert_eq!(forced_reason, Some("exec class disabled"));
            }
            other => panic!("expected exec-closed Deny, got {other:?}"),
        }
    }

    #[test]
    fn exec_off_when_caps_omitted_is_fail_closed() {
        // A policy with no [caps] block defaults exec=false → deny-all.
        let toml = "default_verdict = \"allow\"\n[workspace]\nroot = \".\"\n";
        let p: Policy = toml::from_str(toml).expect("valid policy");
        match evaluate("ls", &p) {
            Decision::Deny { forced_reason, .. } => {
                assert_eq!(forced_reason, Some("exec class disabled"));
            }
            other => panic!("expected fail-closed Deny, got {other:?}"),
        }
    }

    #[test]
    fn unparseable_envelope_forces_deny() {
        // Reaches `forced_deny` before any policy/env access.
        match decide("source x && eval 'git status") {
            Decision::Deny { forced_reason, .. } => {
                assert_eq!(forced_reason, Some("unparseable agent envelope"));
            }
            other => panic!("expected forced Deny, got {other:?}"),
        }
    }
}
