// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! OSS [`PolicyProvider`] — loads a [`Policy`] from a hand-written TOML file.
//!
//! This is the open-core default: the TOML file *is* the resolved policy, so
//! [`PolicyContext`] is ignored. An enterprise build swaps in an RFC-driven
//! compiler behind the same trait with zero OSS edits — the same pattern as
//!
//! The load shape mirrors `orkia_forge_types::ForgeManifest::load`
//! (`read_to_string` → `toml::from_str` → typed error). TOML keeps the policy
//! file — which is the security perimeter — on the maintained `toml` crate
//! already used by every other Orkia config.

use std::path::PathBuf;

use orkia_shell_types::{Policy, PolicyContext, PolicyError, PolicyProvider};

/// Loads a cage [`Policy`] from a fixed TOML file path.
#[derive(Debug, Clone)]
pub struct TomlPolicyLoader {
    path: PathBuf,
}

impl TomlPolicyLoader {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl PolicyProvider for TomlPolicyLoader {
    fn resolve(&self, _ctx: &PolicyContext) -> Result<Policy, PolicyError> {
        if !self.path.exists() {
            return Err(PolicyError::NotFound(self.path.clone()));
        }
        let raw = std::fs::read_to_string(&self.path)?;
        toml::from_str(&raw).map_err(|e| PolicyError::Parse(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::{PolicyDecision, Verdict};

    fn ctx() -> PolicyContext {
        PolicyContext::new("faye", ".")
    }

    const SAMPLE_TOML: &str = r#"
default_verdict = "ask"

[workspace]
root = "."

[[capabilities]]
name = "git.push"
matches = ["git push*"]
verdict = "deny"
"#;

    #[test]
    fn resolves_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, SAMPLE_TOML).unwrap();

        let loader = TomlPolicyLoader::new(&path);
        let policy = loader.resolve(&ctx()).unwrap();

        assert_eq!(policy.capabilities.len(), 1);
        assert_eq!(policy.default_verdict, Verdict::Ask);
        assert!(matches!(
            policy.evaluate_match("git push origin"),
            PolicyDecision::Deny {
                capability: Some("git.push"),
                ..
            }
        ));
    }

    #[test]
    fn missing_file_is_not_found() {
        let loader = TomlPolicyLoader::new("/definitely/not/here/policy.toml");
        let err = loader.resolve(&ctx()).unwrap_err();
        assert!(matches!(err, PolicyError::NotFound(_)));
    }

    #[test]
    fn malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        // `verdict` is not a valid Verdict variant → parse error.
        let bad = "[workspace]\nroot = \".\"\n\n[[capabilities]]\nname = \"x\"\nmatches = [\"y*\"]\nverdict = \"nope\"\n";
        std::fs::write(&path, bad).unwrap();

        let loader = TomlPolicyLoader::new(&path);
        let err = loader.resolve(&ctx()).unwrap_err();
        assert!(matches!(err, PolicyError::Parse(_)));
    }
}
