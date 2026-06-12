// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Per-provider directory-trust integration.
//!
//! Agents (claude / codex / gemini / kimi) refuse to operate in a
//! directory the user hasn't marked "trusted", showing a blocking modal
//! on first launch. Orkia owns the trust decision (see
//! [`registry::TrustRegistry`] — it asks once per directory) and
//! projects it onto each provider two ways:
//!
//!   * **pre-trust** — write the provider's own trusted-dirs config so
//!     the modal never appears ([`TrustProvider::pretrust`]). Known for
//!     claude (`~/.claude.json`) and codex (`~/.codex/config.toml`).
//!   * **auto-answer** — if the modal appears anyway (gemini/kimi, or a
//!     pre-trust failure), send the accept keystroke
//!     ([`TrustProvider::answer_yes`]), bounded to the boot window.
//!
//! This module is the data layer only: read/write configs, no PTY, no
//! REPL. Wiring lives in the dispatch path (consent modal) and the
//! detector (auto-answer).

mod claude;
mod codex;
mod io;
pub mod registry;

use std::path::{Path, PathBuf};

pub use registry::TrustRegistry;

#[derive(thiserror::Error, Debug)]
pub enum TrustError {
    #[error("trust config io: {0}")]
    Io(#[from] std::io::Error),
    #[error("trust config parse: {0}")]
    Parse(String),
    /// Config directory is missing or cannot be created (setup-time failure,
    /// not an I/O read/write error on the config file itself).
    #[error("trust config setup: {0}")]
    Setup(String),
    /// Config serialization failed (not a parse/decode error).
    #[error("trust config serialize: {0}")]
    Serialize(String),
    /// Path contains non-UTF-8 bytes and cannot be stored as a trust key
    /// without loss of information. Fail-closed: treat as not trusted.
    #[error("trust path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
}

/// Outcome of [`TrustProvider::pretrust`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreTrust {
    /// The provider config now trusts the directory.
    Ensured,
    /// This provider has no known trusted-dirs config — the caller must
    /// fall back to auto-answering the live modal.
    Unsupported,
}

/// Read/write an agent provider's trusted-directories config and the
/// keystroke that accepts its trust modal.
pub trait TrustProvider {
    fn name(&self) -> &str;

    /// Does the provider's own config already trust `dir`?
    fn is_trusted(&self, dir: &Path) -> bool;

    /// Best-effort: make the provider trust `dir` by writing its config.
    /// `Unsupported` means we don't know this provider's format — the
    /// caller auto-answers the modal instead.
    fn pretrust(&self, dir: &Path) -> Result<PreTrust, TrustError>;

    /// Bytes that accept the provider's trust modal. Every known agent
    /// highlights "Yes" by default, so Enter (`\r`) confirms it.
    fn answer_yes(&self) -> &'static [u8] {
        b"\r"
    }
}

/// Resolve the trust integration for a provider, rooted at `home`
/// (the `$HOME` the agent will be spawned with — its config lives under
/// it). Providers without a known trusted-dirs config get the generic
/// no-config integration.
pub fn provider_for(
    provider: orkia_shell_types::ProviderId,
    home: PathBuf,
) -> Box<dyn TrustProvider> {
    use orkia_shell_types::ProviderId;
    match provider {
        ProviderId::Claude => Box::new(claude::ClaudeTrust::new(home)),
        ProviderId::Codex => Box::new(codex::CodexTrust::new(home)),
        ProviderId::Gemini | ProviderId::Kimi | ProviderId::Generic => Box::new(GenericTrust {
            name: provider.as_str(),
        }),
    }
}

/// Providers with no known trusted-dirs config (gemini, kimi, …). They
/// can only be handled by auto-answering the live modal.
struct GenericTrust {
    name: &'static str,
}

impl TrustProvider for GenericTrust {
    fn name(&self) -> &str {
        self.name
    }
    fn is_trusted(&self, _dir: &Path) -> bool {
        false
    }
    fn pretrust(&self, _dir: &Path) -> Result<PreTrust, TrustError> {
        Ok(PreTrust::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_provider_is_unsupported() {
        let p = provider_for(
            orkia_shell_types::ProviderId::Gemini,
            PathBuf::from("/tmp/nope"),
        );
        assert_eq!(p.name(), "gemini");
        assert!(!p.is_trusted(Path::new("/any/dir")));
        assert_eq!(
            p.pretrust(Path::new("/any/dir")).unwrap(),
            PreTrust::Unsupported
        );
        assert_eq!(p.answer_yes(), b"\r");
    }

    #[test]
    fn known_providers_resolve() {
        use orkia_shell_types::ProviderId;
        assert_eq!(
            provider_for(ProviderId::Claude, "/h".into()).name(),
            "claude"
        );
        assert_eq!(provider_for(ProviderId::Codex, "/h".into()).name(), "codex");
    }
}
