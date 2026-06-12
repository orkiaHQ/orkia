// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Visibility scope primitive.
//!
//! Every artifact in Orkia (workspace, project, RFC, issue) carries a scope
//!
//! This module defines the enum and the resolution helpers.
//! PR1a ships the type only. PR1b wires it into the parsers and the emission helper.
//! PR2 wires it into the user-facing builtins and prompt.

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

/// Visibility scope of an artifact.
///
/// Ordering: `Private < Team < Public`. A scope is "more permissive" than another
/// if it ranks higher. Inheritance allows children to be **less permissive than
/// or equal to** their parent — never more permissive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Private,
    Team,
    Public,
}

impl Scope {
    /// Returns true if this scope is at least as permissive as `other`.
    /// e.g., `Public.is_at_least(Team) == true`; `Team.is_at_least(Public) == false`.
    pub fn is_at_least(self, other: Scope) -> bool {
        self >= other
    }

    /// Parse from a lowercase string. Returns an error on unknown values.
    pub fn parse(s: &str) -> Result<Self, ScopeParseError> {
        match s {
            "private" => Ok(Scope::Private),
            "team" => Ok(Scope::Team),
            "public" => Ok(Scope::Public),
            other => Err(ScopeParseError::Unknown(other.to_string())),
        }
    }

    /// String representation (matches the serde `rename_all = "lowercase"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Private => "private",
            Scope::Team => "team",
            Scope::Public => "public",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Default for Scope {
    /// The default scope for any artifact with no explicit value and no ancestor
    fn default() -> Self {
        Scope::Private
    }
}

#[derive(Debug, Error)]
pub enum ScopeParseError {
    #[error("unknown scope value: '{0}' (expected one of: private, team, public)")]
    Unknown(String),
}

#[derive(Debug, Error)]
#[error(
    "illegal scope override: parent is {parent}, proposed child is {proposed} \
     — child cannot be more permissive than parent"
)]
pub struct IllegalOverride {
    pub parent: Scope,
    pub proposed: Scope,
}

/// Resolve the effective scope of an artifact given the chain
/// issue → rfc → project → workspace → Private (terminal).
///
/// The first explicit (Some) value in the chain wins. If none, returns `Private`.
pub fn resolve_effective_scope(
    workspace: Option<Scope>,
    project: Option<Scope>,
    rfc: Option<Scope>,
    issue: Option<Scope>,
) -> Scope {
    issue
        .or(rfc)
        .or(project)
        .or(workspace)
        .unwrap_or(Scope::Private)
}

/// Validate that a proposed child scope does not exceed its parent's permissiveness.
/// Returns Ok if the override is legal (proposed <= parent), Err otherwise.
pub fn validate_override(parent: Scope, proposed: Scope) -> Result<(), IllegalOverride> {
    if proposed.is_at_least(parent) && proposed != parent {
        Err(IllegalOverride { parent, proposed })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trip() {
        for s in &[Scope::Private, Scope::Team, Scope::Public] {
            let parsed = Scope::parse(s.as_str()).unwrap();
            assert_eq!(parsed, *s);
        }
    }

    #[test]
    fn parse_unknown_fails() {
        assert!(matches!(
            Scope::parse("internal"),
            Err(ScopeParseError::Unknown(_))
        ));
    }

    #[test]
    fn ordering() {
        assert!(Scope::Public > Scope::Team);
        assert!(Scope::Team > Scope::Private);
        assert!(Scope::Public.is_at_least(Scope::Private));
        assert!(!Scope::Private.is_at_least(Scope::Team));
    }

    #[test]
    fn default_is_private() {
        assert_eq!(Scope::default(), Scope::Private);
    }

    #[test]
    fn resolve_chain_first_explicit_wins() {
        assert_eq!(
            resolve_effective_scope(Some(Scope::Public), Some(Scope::Team), None, None),
            Scope::Team
        );
        assert_eq!(
            resolve_effective_scope(Some(Scope::Public), None, Some(Scope::Private), None),
            Scope::Private
        );
        assert_eq!(
            resolve_effective_scope(None, None, None, None),
            Scope::Private
        );
        assert_eq!(
            resolve_effective_scope(None, None, None, Some(Scope::Public)),
            Scope::Public
        );
    }

    #[test]
    fn validate_override_rules() {
        // legal: child equal or less permissive
        assert!(validate_override(Scope::Public, Scope::Team).is_ok());
        assert!(validate_override(Scope::Public, Scope::Private).is_ok());
        assert!(validate_override(Scope::Public, Scope::Public).is_ok());
        assert!(validate_override(Scope::Team, Scope::Private).is_ok());
        assert!(validate_override(Scope::Private, Scope::Private).is_ok());

        // illegal: child more permissive than parent
        assert!(validate_override(Scope::Private, Scope::Team).is_err());
        assert!(validate_override(Scope::Private, Scope::Public).is_err());
        assert!(validate_override(Scope::Team, Scope::Public).is_err());
    }
}
