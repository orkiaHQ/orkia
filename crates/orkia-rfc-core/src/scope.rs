// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Visibility scope mirror for RFC frontmatter.
//!
//! This is a structural mirror of `orkia_shell_types::scope::Scope`.
//! Defined locally because `orkia-shell-types` already depends on
//! `orkia-rfc-core`; adding the reverse edge would form a cycle. Both
//! enums share identical TOML/JSON serialization (`"private"`,
//! `"team"`, `"public"`) so an RFC written through one path round-trips
//! cleanly through the other.
//!
//! Keep this file and `orkia-shell-types::scope` in sync — any new
//! variant or rename must land in both crates in the same PR.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Private,
    Team,
    Public,
}

impl Scope {
    /// String representation, matches the serde `rename_all = "lowercase"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Private => "private",
            Scope::Team => "team",
            Scope::Public => "public",
        }
    }
}

impl Default for Scope {
    /// Safe default for any artifact with no explicit value and no
    fn default() -> Self {
        Scope::Private
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_matches_shell_types_mirror() {
        // The contract is wire compatibility with orkia_shell_types::Scope:
        // both serialize variants as lowercase strings.
        for (variant, expected) in [
            (Scope::Private, "\"private\""),
            (Scope::Team, "\"team\""),
            (Scope::Public, "\"public\""),
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected);
            let back: Scope = serde_json::from_str(&s).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn default_is_private() {
        assert_eq!(Scope::default(), Scope::Private);
    }
}
