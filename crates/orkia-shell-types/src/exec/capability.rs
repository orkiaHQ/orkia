// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! One type describes what a `Command` — native **or** plugin — may do:
//! filesystem read/write, network, environment, clock, randomness. It is
//! **fail-closed**: an absent capability is forbidden. `CommandCtx` carries the
//! *granted* set for an invocation; a native command consults it through the
//! verified accessors on `CommandCtx` (cooperative enforcement, since native
//! code runs in-process), while a plugin is constrained structurally by the
//! wasmtime linker (the import simply isn't provided).
//!
//! This type is deliberately **lightweight** — scopes over paths/hosts/vars,
//! nothing more — so adding it to `CommandCtx` pulls no heavy deps (no
//! keyring/turso/tantivy). Capabilities that need a credential store are a
//! future extension behind a feature, never on this base path.
//!
//! NB: distinct from `orkia_capabilities::CapabilitySet`, which models *plan
//! tier* features (cognitive routing, forge build…). This is the *effect /
//! sandbox* axis: what an invocation may touch.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// A single grant entry within a [`CapabilityScope::Scoped`] list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// A filesystem path prefix (a granted dir/file and everything under it).
    Path(std::path::PathBuf),
    /// A network host (exact match).
    Host(String),
    /// An environment variable name (exact match).
    Var(String),
}

/// How broadly one capability is granted. Fail-closed: `None` = forbidden.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CapabilityScope {
    /// No grant — every request is denied.
    #[default]
    None,
    /// Granted only for the listed scopes.
    Scoped(Vec<Scope>),
    /// Granted unconditionally.
    Any,
}

impl CapabilityScope {
    /// Whether this scope is the empty (deny-all) grant.
    pub fn is_none(&self) -> bool {
        matches!(self, CapabilityScope::None)
    }

    /// Does a path fall within this scope? (`Any`, or under a granted prefix.)
    fn allows_path(&self, path: &Path) -> bool {
        match self {
            CapabilityScope::None => false,
            CapabilityScope::Any => true,
            CapabilityScope::Scoped(scopes) => scopes.iter().any(|s| match s {
                Scope::Path(prefix) => path.starts_with(prefix),
                _ => false,
            }),
        }
    }

    /// Is `name` (a host or env var) granted by this scope?
    fn allows_named(&self, name: &str, want_host: bool) -> bool {
        match self {
            CapabilityScope::None => false,
            CapabilityScope::Any => true,
            CapabilityScope::Scoped(scopes) => scopes.iter().any(|s| match (s, want_host) {
                (Scope::Host(h), true) => h == name,
                (Scope::Var(v), false) => v == name,
                _ => false,
            }),
        }
    }
}

/// What a command invocation is *granted* to do. Fail-closed by default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CapabilitySet {
    pub fs_read: CapabilityScope,
    pub fs_write: CapabilityScope,
    pub net: CapabilityScope,
    pub env: CapabilityScope,
    pub clock: bool,
    pub random: bool,
}

impl CapabilitySet {
    /// A total-sandbox grant: nothing allowed. The fail-closed default — and
    /// what a plugin gets unless its manifest declares (user-approved) grants.
    pub fn sandbox() -> Self {
        Self::default()
    }

    /// The default grant for trusted native shell builtins: unrestricted
    /// filesystem + environment + clock + randomness, but **no network** (shell
    /// commands don't reach the network; agents/MCP do). Reflects that native
    /// fundamentals are Orkia's own trusted code.
    pub fn shell_default() -> Self {
        Self {
            fs_read: CapabilityScope::Any,
            fs_write: CapabilityScope::Any,
            net: CapabilityScope::None,
            env: CapabilityScope::Any,
            clock: true,
            random: true,
        }
    }

    /// `true` ⇒ no effect capability granted at all (pure compute / sandbox).
    pub fn is_total_sandbox(&self) -> bool {
        self.fs_read.is_none()
            && self.fs_write.is_none()
            && self.net.is_none()
            && self.env.is_none()
            && !self.clock
            && !self.random
    }

    pub fn allows_fs_read(&self, path: &Path) -> bool {
        self.fs_read.allows_path(path)
    }
    pub fn allows_fs_write(&self, path: &Path) -> bool {
        self.fs_write.allows_path(path)
    }
    pub fn allows_net(&self, host: &str) -> bool {
        self.net.allows_named(host, true)
    }
    pub fn allows_env(&self, var: &str) -> bool {
        self.env.allows_named(var, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sandbox_grants_nothing() {
        let c = CapabilitySet::sandbox();
        assert!(c.is_total_sandbox());
        assert!(!c.allows_fs_read(&PathBuf::from("/etc/passwd")));
        assert!(!c.allows_fs_write(&PathBuf::from("/tmp/x")));
        assert!(!c.allows_net("example.com"));
        assert!(!c.allows_env("HOME"));
    }

    #[test]
    fn shell_default_is_fs_and_env_but_not_net() {
        let c = CapabilitySet::shell_default();
        assert!(!c.is_total_sandbox());
        assert!(c.allows_fs_read(&PathBuf::from("/anywhere")));
        assert!(c.allows_fs_write(&PathBuf::from("/anywhere")));
        assert!(c.allows_env("PATH"));
        assert!(c.clock && c.random);
        // No network for native shell builtins (agents/MCP do that).
        assert!(!c.allows_net("example.com"));
    }

    #[test]
    fn scoped_fs_read_is_prefix_matched() {
        let c = CapabilitySet {
            fs_read: CapabilityScope::Scoped(vec![Scope::Path(PathBuf::from("/data"))]),
            ..CapabilitySet::sandbox()
        };
        assert!(c.allows_fs_read(&PathBuf::from("/data/a/b.txt")));
        assert!(c.allows_fs_read(&PathBuf::from("/data")));
        assert!(!c.allows_fs_read(&PathBuf::from("/etc/secret")));
        // A scope entry for fs_read does not leak into other capabilities.
        assert!(!c.allows_fs_write(&PathBuf::from("/data/a")));
    }

    #[test]
    fn scoped_net_is_exact_host() {
        let c = CapabilitySet {
            net: CapabilityScope::Scoped(vec![Scope::Host("api.example.com".into())]),
            ..CapabilitySet::sandbox()
        };
        assert!(c.allows_net("api.example.com"));
        assert!(!c.allows_net("evil.example.com"));
        assert!(!c.is_total_sandbox());
    }
}
