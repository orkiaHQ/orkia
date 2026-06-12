// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Per-app permission model for Forge V2.
//!
//! The viewer's job is to refuse a privileged call (`agent.invoke`,
//! `network.fetch`, `notification.send`, even `storage.*`) before any
//! computation happens when the per-app manifest clearly forbids it.
//! This crate is the single source of truth for what "clearly forbids"
//! means. The server-side runtime makes the *final* policy decision; the
//! viewer's pre-check just saves a roundtrip and gives faster feedback.
//!
//! every browser sandbox security model has been broken by wildcard and
//! subdomain confusion attacks — V2 intentionally keeps matching strict
//! (exact host, no wildcards, no implicit subdomain allow) and the tests

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod domain;

use orkia_forge_types::BridgeError;
use url::Url;

/// Effective permissions for a Forge app, derived from
/// `manifest.toml`'s `[forge.permissions]` block.
///
/// This mirrors `orkia_forge_types::Permissions` shape but lives here so
/// the permission-checking logic doesn't need the types crate as its
/// only consumer (and we can evolve the runtime policy independently of
/// the wire shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permissions {
    pub storage: bool,
    pub agent: bool,
    /// Whitelist of exact-match hosts. Empty = no network allowed.
    pub network: Vec<String>,
    pub notification: bool,
}

impl Permissions {
    /// Construct from the on-disk manifest representation.
    pub fn from_manifest(perms: &orkia_forge_types::Permissions) -> Self {
        Self {
            storage: perms.storage,
            agent: perms.agent,
            network: perms.network.clone(),
            notification: perms.notification,
        }
    }

    /// Allow callers to construct directly without going through the
    /// types crate (useful for tests and the in-process orchestrator).
    pub fn new(storage: bool, agent: bool, network: Vec<String>, notification: bool) -> Self {
        Self {
            storage,
            agent,
            network,
            notification,
        }
    }

    pub fn check_storage(&self) -> Result<(), BridgeError> {
        if self.storage {
            Ok(())
        } else {
            Err(BridgeError::PermissionDenied(
                "manifest.permissions.storage = false".into(),
            ))
        }
    }

    pub fn check_agent(&self) -> Result<(), BridgeError> {
        if self.agent {
            Ok(())
        } else {
            Err(BridgeError::PermissionDenied(
                "manifest.permissions.agent = false".into(),
            ))
        }
    }

    pub fn check_notification(&self) -> Result<(), BridgeError> {
        if self.notification {
            Ok(())
        } else {
            Err(BridgeError::PermissionDenied(
                "manifest.permissions.notification = false".into(),
            ))
        }
    }

    ///
    /// Rejects (in priority order):
    /// 1. URLs that don't parse.
    /// 3. URLs without a host.
    /// 4. IP-literal hosts (IPv4 dotted-quad, IPv6 bracketed).
    /// 5. `localhost` / `127.0.0.1` / `::1` aliases.
    /// 6. Hosts not in the whitelist (case-insensitive, exact match).
    pub fn check_network(&self, url: &str) -> Result<(), BridgeError> {
        let parsed =
            Url::parse(url).map_err(|e| BridgeError::Invalid(format!("invalid URL: {e}")))?;

        if parsed.scheme() != "https" {
            return Err(BridgeError::PermissionDenied(format!(
                "only https:// URLs allowed (got {})",
                parsed.scheme()
            )));
        }

        let host = parsed
            .host_str()
            .ok_or_else(|| BridgeError::Invalid("URL must have a host".into()))?;

        if domain::is_ip_literal(host) {
            return Err(BridgeError::PermissionDenied(
                "IP literal hosts forbidden in V2".into(),
            ));
        }

        if domain::is_localhost(host) {
            return Err(BridgeError::PermissionDenied(
                "localhost forbidden in V2".into(),
            ));
        }

        if domain::host_in_whitelist(host, &self.network) {
            Ok(())
        } else {
            Err(BridgeError::PermissionDenied(format!(
                "host '{host}' not in app's network whitelist"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perms(network: Vec<&str>) -> Permissions {
        Permissions::new(
            true,
            true,
            network.into_iter().map(String::from).collect(),
            true,
        )
    }

    // ── check_storage / check_agent / check_notification ────────────

    #[test]
    fn storage_check_allows_when_true() {
        assert!(perms(vec![]).check_storage().is_ok());
    }

    #[test]
    fn storage_check_denies_when_false() {
        let p = Permissions::new(false, true, vec![], true);
        assert!(matches!(
            p.check_storage(),
            Err(BridgeError::PermissionDenied(_))
        ));
    }

    #[test]
    fn agent_check_denies_when_false() {
        let p = Permissions::new(true, false, vec![], true);
        assert!(matches!(
            p.check_agent(),
            Err(BridgeError::PermissionDenied(_))
        ));
    }

    #[test]
    fn notification_check_denies_when_false() {
        let p = Permissions::new(true, true, vec![], false);
        assert!(matches!(
            p.check_notification(),
            Err(BridgeError::PermissionDenied(_))
        ));
    }

    // ── check_network: scheme/host enforcement ──────────────────────

    #[test]
    fn rejects_http_scheme() {
        let err = perms(vec!["api.x.com"])
            .check_network("http://api.x.com/x")
            .unwrap_err();
        assert!(matches!(err, BridgeError::PermissionDenied(_)));
    }

    #[test]
    fn rejects_ws_scheme() {
        let err = perms(vec!["api.x.com"])
            .check_network("ws://api.x.com/")
            .unwrap_err();
        assert!(matches!(err, BridgeError::PermissionDenied(_)));
    }

    #[test]
    fn rejects_invalid_url() {
        let err = perms(vec!["api.x.com"])
            .check_network("not a url")
            .unwrap_err();
        assert!(matches!(err, BridgeError::Invalid(_)));
    }

    #[test]
    fn rejects_ipv4_literal() {
        let err = perms(vec!["1.2.3.4"])
            .check_network("https://1.2.3.4/")
            .unwrap_err();
        // Note: even though "1.2.3.4" is in the whitelist, the IP-literal
        assert!(matches!(err, BridgeError::PermissionDenied(_)));
    }

    #[test]
    fn rejects_ipv6_literal() {
        let err = perms(vec!["::1"])
            .check_network("https://[::1]/")
            .unwrap_err();
        assert!(matches!(err, BridgeError::PermissionDenied(_)));
    }

    #[test]
    fn rejects_localhost() {
        let err = perms(vec!["localhost"])
            .check_network("https://localhost/x")
            .unwrap_err();
        assert!(matches!(err, BridgeError::PermissionDenied(_)));
    }

    #[test]
    fn rejects_loopback_127() {
        let err = perms(vec!["127.0.0.1"])
            .check_network("https://127.0.0.1/")
            .unwrap_err();
        assert!(matches!(err, BridgeError::PermissionDenied(_)));
    }

    // ── check_network: whitelist semantics ──────────────────────────

    #[test]
    fn allows_exact_whitelisted_host() {
        assert!(
            perms(vec!["api.stripe.com"])
                .check_network("https://api.stripe.com/v1/prices")
                .is_ok()
        );
    }

    #[test]
    fn case_insensitive_match() {
        assert!(
            perms(vec!["API.STRIPE.COM"])
                .check_network("https://api.stripe.com/v1/prices")
                .is_ok()
        );
        assert!(
            perms(vec!["api.stripe.com"])
                .check_network("https://API.Stripe.COM/v1/prices")
                .is_ok()
        );
    }

    /// `stripe.com` (parent domain).
    #[test]
    fn rejects_parent_domain_when_only_subdomain_whitelisted() {
        let err = perms(vec!["api.stripe.com"])
            .check_network("https://stripe.com/")
            .unwrap_err();
        assert!(matches!(err, BridgeError::PermissionDenied(_)));
    }

    /// (sibling subdomain).
    #[test]
    fn rejects_sibling_subdomain() {
        let err = perms(vec!["api.stripe.com"])
            .check_network("https://dashboard.stripe.com/")
            .unwrap_err();
        assert!(matches!(err, BridgeError::PermissionDenied(_)));
    }

    /// `api.stripe.com` must NOT allow `evil-api.stripe.com.attacker.com`.
    #[test]
    fn rejects_subdomain_confusion_attack() {
        let err = perms(vec!["api.stripe.com"])
            .check_network("https://evil-api.stripe.com.attacker.com/")
            .unwrap_err();
        assert!(matches!(err, BridgeError::PermissionDenied(_)));
    }

    #[test]
    fn rejects_empty_whitelist() {
        let err = perms(vec![])
            .check_network("https://api.stripe.com/")
            .unwrap_err();
        assert!(matches!(err, BridgeError::PermissionDenied(_)));
    }

    #[test]
    fn whitelist_match_is_string_not_substring() {
        // `apple.com` is in the whitelist; `pineapple.com` ends with the
        // same suffix but must not match.
        let err = perms(vec!["apple.com"])
            .check_network("https://pineapple.com/")
            .unwrap_err();
        assert!(matches!(err, BridgeError::PermissionDenied(_)));
    }

    // ── interaction: multiple hosts ─────────────────────────────────

    #[test]
    fn matches_against_multi_host_whitelist() {
        let p = perms(vec!["api.x.com", "api.y.com", "api.z.com"]);
        assert!(p.check_network("https://api.y.com/v1/items").is_ok());
        assert!(p.check_network("https://api.x.com/v1/items").is_ok());
        assert!(p.check_network("https://api.w.com/v1/items").is_err());
    }

    #[test]
    fn from_manifest_round_trips() {
        let m = orkia_forge_types::Permissions {
            storage: true,
            agent: false,
            network: vec!["api.x.com".into()],
            notification: true,
        };
        let p = Permissions::from_manifest(&m);
        assert!(p.check_storage().is_ok());
        assert!(p.check_agent().is_err());
        assert!(p.check_notification().is_ok());
        assert!(p.check_network("https://api.x.com/").is_ok());
    }
}
