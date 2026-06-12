// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Implements `orkia login`, `orkia logout`, `orkia whoami` for the
//! public OSS binary.
//!
//! These delegate to the configured [`AuthProvider`]. The OSS build
//! ships [`MagicLinkAuthProvider`]: `orkia login` runs the email →
//! one-time-code → bearer flow and persists to the OS keychain (or to
//! `ORKIA_SESSION_FILE` for headless harnesses). Reads (`whoami`/`bearer`)
//! load that persisted session — there is no env-injected bypass.

use orkia_auth::{AuthError, AuthEvent, AuthProvider};
use orkia_magic_login::MagicLinkAuthProvider;
use orkia_shell_types::backend::resolve_backend_url;

fn provider() -> impl AuthProvider {
    // Fall back to the default backend URL if resolution fails; an actual
    // login against a bad URL surfaces a clear network error, and the
    // read paths only touch the persisted session store anyway.
    let base = resolve_backend_url(None)
        .unwrap_or_else(|_| orkia_shell_types::backend::DEFAULT_BACKEND_URL.to_string());
    MagicLinkAuthProvider::new(base)
}

/// `orkia login` — runs the magic-link flow and persists the session.
pub async fn run_login(_args: &[String]) -> i32 {
    let p = provider();
    let mut sink = |ev: AuthEvent| {
        if let AuthEvent::Completed { display_name } = ev {
            println!("  ✓ authenticated as @{display_name}");
        }
    };
    match tokio::task::spawn_blocking(move || p.login(&mut sink))
        .await
        .unwrap_or_else(|_| Err(AuthError::Backend("login worker join failed".into())))
    {
        Ok(s) => {
            println!("  ✓ session ready · plan: {} · email: {}", s.plan, s.email);
            0
        }
        Err(AuthError::Misconfigured(msg)) => {
            eprintln!("  ✗ orkia login: {msg}");
            1
        }
        Err(e) => {
            eprintln!("  ✗ login failed: {e}");
            1
        }
    }
}

/// `orkia logout` — clears the persisted session.
pub async fn run_logout(_args: &[String]) -> i32 {
    let p = provider();
    match tokio::task::spawn_blocking(move || p.logout())
        .await
        .unwrap_or_else(|_| Err(AuthError::Backend("logout worker join failed".into())))
    {
        Ok(()) => {
            println!("  ✓ logged out (keychain session cleared)");
            0
        }
        Err(e) => {
            eprintln!("  ⚠ logout reported: {e}");
            0
        }
    }
}

/// `orkia whoami` — prints the session as the configured provider
/// sees it. No network call.
pub async fn run_whoami(_args: &[String]) -> i32 {
    let p = provider();
    match p.current() {
        Some(s) => {
            println!("  user:  @{}", s.display_name);
            println!("  email: {}", s.email);
            println!("  plan:  {}", s.plan);
            0
        }
        None => {
            eprintln!("  ⓘ not logged in. Run `orkia login`.");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn whoami_without_session_reports_unauthenticated() {
        // No persisted session in the test env (keychain probe finds
        // nothing, and ORKIA_SESSION_FILE is unset) ⇒ not logged in.
        if std::env::var("ORKIA_SESSION_FILE").is_err() {
            assert_eq!(run_whoami(&[]).await, 1);
        }
    }
}
