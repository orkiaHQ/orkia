// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Canonical backend URL resolution for every Orkia HTTP client.
//!
//! One backend, one hostname, one env var. Clients of the Orkia
//! cloud service must resolve their base URL through
//! [`resolve_backend_url`] so the codebase has a single source of
//! truth for what URL we talk to.

use thiserror::Error;

/// Canonical fallback URL for the Orkia cloud service. Used when no
/// explicit override is provided and `ORKIA_BACKEND_URL` is unset.
pub const DEFAULT_BACKEND_URL: &str = "https://api.orkia.io";

/// Environment variable that overrides [`DEFAULT_BACKEND_URL`].
pub const ENV_VAR_BACKEND_URL: &str = "ORKIA_BACKEND_URL";

/// Legacy environment variable name. Honored for one release with a
/// deprecation warning so users migrating from `ORKIA_API_URL` have
/// time to update their environment. Will be removed in a future
/// release — new code should reference [`ENV_VAR_BACKEND_URL`].
pub const ENV_VAR_API_URL_LEGACY: &str = "ORKIA_API_URL";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BackendUrlError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("URL must use https scheme, got: {0}")]
    NotHttps(String),
}

/// Resolve the backend URL using the canonical order:
/// 1. Explicit override (caller-provided).
/// 2. Env var `ORKIA_BACKEND_URL`.
/// 3. Env var `ORKIA_API_URL` (deprecated, emits a `tracing::warn!`).
/// 4. Fallback [`DEFAULT_BACKEND_URL`].
///
/// Trailing slashes are stripped. The result is a well-formed
/// `https://` URL — or `http://` when (and only when) the host is
/// loopback, so local harnesses can target a compose backend.
pub fn resolve_backend_url(explicit: Option<&str>) -> Result<String, BackendUrlError> {
    let raw = if let Some(value) = explicit {
        value.to_string()
    } else if let Ok(value) = std::env::var(ENV_VAR_BACKEND_URL) {
        value
    } else if let Ok(value) = std::env::var(ENV_VAR_API_URL_LEGACY) {
        tracing::warn!(
            "ORKIA_API_URL is deprecated; use ORKIA_BACKEND_URL instead. \
             The legacy variable will be removed in a future release."
        );
        value
    } else {
        DEFAULT_BACKEND_URL.to_string()
    };

    // Plain `http://` is allowed for LOOPBACK ONLY (the e2e/compose
    // harnesses point the shell at `http://localhost:8080`) — same
    // exception OAuth makes for loopback redirects. Any other host
    // must be https; fail-closed, never silently downgrade.
    let scheme = if raw.starts_with("https://") {
        "https://"
    } else if raw.starts_with("http://") && is_loopback_host(&raw["http://".len()..]) {
        "http://"
    } else {
        return Err(BackendUrlError::NotHttps(raw));
    };

    // Strip trailing slashes only AFTER the scheme check, so
    // `"https://"` doesn't collapse to `"https:"` and mis-route to
    // NotHttps. Validate the host segment on the trimmed form.
    let trimmed = raw.trim_end_matches('/').to_string();

    // Reject obviously malformed forms early. A full `url::Url::parse`
    // would be stricter but pulls in a dependency this crate doesn't
    // otherwise need; the scheme check + a non-empty host check is
    // enough for our V1 needs (the HTTP client itself does the real
    // parse on every request).
    let after_scheme = trimmed.strip_prefix(scheme).unwrap_or("");
    let host = after_scheme.split('/').next().unwrap_or("");
    if host.is_empty() || host.contains(' ') {
        return Err(BackendUrlError::InvalidUrl(raw));
    }

    Ok(trimmed)
}

/// True when the authority part of a URL (everything after the scheme)
/// points at the local loopback interface. Port and path are ignored.
fn is_loopback_host(after_scheme: &str) -> bool {
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host = if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6 literal: `[::1]:8080`.
        rest.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env-var mutation is process-global. Serialize the tests in this
    // module so they don't race against each other under
    // `cargo test`'s default thread pool.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Snapshot both env vars, clear both for the duration of `f`, then
    /// restore. Holding ENV_LOCK serializes process-wide env mutation
    /// across this test module.
    fn with_env_unset<F: FnOnce() -> T, T>(f: F) -> T {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior_new = std::env::var(ENV_VAR_BACKEND_URL).ok();
        let prior_legacy = std::env::var(ENV_VAR_API_URL_LEGACY).ok();
        // SAFETY: env mutation is unsafe in 2024-edition Rust. The
        // test module serializes itself via ENV_LOCK so no concurrent
        // reader observes the half-set state.
        unsafe {
            std::env::remove_var(ENV_VAR_BACKEND_URL);
            std::env::remove_var(ENV_VAR_API_URL_LEGACY);
        }
        let out = f();
        unsafe {
            match prior_new {
                Some(v) => std::env::set_var(ENV_VAR_BACKEND_URL, v),
                None => std::env::remove_var(ENV_VAR_BACKEND_URL),
            }
            match prior_legacy {
                Some(v) => std::env::set_var(ENV_VAR_API_URL_LEGACY, v),
                None => std::env::remove_var(ENV_VAR_API_URL_LEGACY),
            }
        }
        out
    }

    /// Run `f` with `var = val` and both env vars cleared otherwise.
    fn with_env_set<F: FnOnce() -> T, T>(var: &str, val: &str, f: F) -> T {
        with_env_unset(|| {
            unsafe {
                std::env::set_var(var, val);
            }
            let out = f();
            unsafe {
                std::env::remove_var(var);
            }
            out
        })
    }

    #[test]
    fn resolve_uses_explicit_first() {
        let r = resolve_backend_url(Some("https://example.com")).unwrap();
        assert_eq!(r, "https://example.com");
    }

    #[test]
    fn resolve_uses_backend_url_env() {
        with_env_set(ENV_VAR_BACKEND_URL, "https://from-env.example", || {
            let r = resolve_backend_url(None).unwrap();
            assert_eq!(r, "https://from-env.example");
        });
    }

    #[test]
    fn resolve_falls_back_to_legacy_env_with_warning() {
        with_env_set(ENV_VAR_API_URL_LEGACY, "https://legacy.example", || {
            let r = resolve_backend_url(None).unwrap();
            assert_eq!(r, "https://legacy.example");
        });
    }

    #[test]
    fn resolve_prefers_new_env_over_legacy() {
        with_env_unset(|| {
            unsafe {
                std::env::set_var(ENV_VAR_BACKEND_URL, "https://new.example");
                std::env::set_var(ENV_VAR_API_URL_LEGACY, "https://legacy.example");
            }
            let r = resolve_backend_url(None).unwrap();
            unsafe {
                std::env::remove_var(ENV_VAR_BACKEND_URL);
                std::env::remove_var(ENV_VAR_API_URL_LEGACY);
            }
            assert_eq!(r, "https://new.example");
        });
    }

    #[test]
    fn resolve_falls_back_to_canonical() {
        with_env_unset(|| {
            let r = resolve_backend_url(None).unwrap();
            assert_eq!(r, DEFAULT_BACKEND_URL);
        });
    }

    #[test]
    fn resolve_strips_trailing_slash() {
        let r = resolve_backend_url(Some("https://example.com/")).unwrap();
        assert_eq!(r, "https://example.com");
    }

    #[test]
    fn resolve_strips_multiple_trailing_slashes() {
        let r = resolve_backend_url(Some("https://example.com///")).unwrap();
        assert_eq!(r, "https://example.com");
    }

    #[test]
    fn resolve_rejects_http() {
        let r = resolve_backend_url(Some("http://example.com"));
        assert!(matches!(r, Err(BackendUrlError::NotHttps(_))));
    }

    #[test]
    fn resolve_allows_http_loopback_only() {
        for url in [
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
            "http://localhost",
        ] {
            let r = resolve_backend_url(Some(url)).unwrap();
            assert_eq!(r, url);
        }
        // A hostname merely PREFIXED with a loopback name is not loopback.
        for url in [
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example",
            "http://192.168.1.10:8080",
        ] {
            let r = resolve_backend_url(Some(url));
            assert!(matches!(r, Err(BackendUrlError::NotHttps(_))), "{url}");
        }
    }

    #[test]
    fn resolve_rejects_missing_host() {
        let r = resolve_backend_url(Some("https://"));
        assert!(matches!(r, Err(BackendUrlError::InvalidUrl(_))));
    }

    #[test]
    fn resolve_explicit_overrides_env() {
        with_env_set(ENV_VAR_BACKEND_URL, "https://env.example", || {
            let r = resolve_backend_url(Some("https://explicit.example")).unwrap();
            assert_eq!(r, "https://explicit.example");
        });
    }

    #[test]
    fn default_backend_url_constant() {
        assert_eq!(DEFAULT_BACKEND_URL, "https://api.orkia.io");
    }
}
