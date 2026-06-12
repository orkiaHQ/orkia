// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Low-level host classifiers used by `Permissions::check_network`.
//!
//! These are split out so they can be tested independently of `url::Url`
//! parsing — bugs in this layer are the bugs that wreck a permission
//! model in production, so they get their own seam.

use std::net::IpAddr;

/// Whether `host` is a literal IPv4 or IPv6 address.
///
/// `url::Url::host_str` returns IPv6 hosts in bracketed form (`[::1]`);
/// when the URL crate parses the host it gives us the bare form already.
/// We accept both because callers may invoke this with either shape.
pub fn is_ip_literal(host: &str) -> bool {
    let trimmed = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    trimmed.parse::<IpAddr>().is_ok()
}

/// Whether `host` resolves to a loopback alias. We do not do DNS
/// resolution — we match on the well-known string aliases. DNS
/// rebinding attacks are out of scope at this layer (the runtime
/// fetcher does the actual connect and is what enforces).
///
/// **Documented limitation (SEC-077):** this list covers common aliases
/// (`127.0.0.1`, `::1`, `localhost`). Other loopback addresses in the
/// `127.0.0.0/8` CIDR range (e.g. `127.0.0.2`) are *not* matched here.
/// However, those are already rejected by the `is_ip_literal` guard in
/// `check_network`, which runs before this function and blocks all literal
/// IP addresses. Full loopback-CIDR matching is a V3 enhancement.
pub fn is_localhost(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "localhost"
            | "127.0.0.1"
            | "::1"
            | "[::1]"
            // IPv4 mapped IPv6 loopback
            | "::ffff:127.0.0.1"
    )
}

/// Exact-match, case-insensitive host check against the whitelist.
pub fn host_in_whitelist(host: &str, whitelist: &[String]) -> bool {
    whitelist.iter().any(|w| w.eq_ignore_ascii_case(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_literal_detects_ipv4() {
        assert!(is_ip_literal("1.2.3.4"));
        assert!(is_ip_literal("255.255.255.255"));
    }

    #[test]
    fn ip_literal_detects_ipv6() {
        assert!(is_ip_literal("::1"));
        assert!(is_ip_literal("[::1]"));
        assert!(is_ip_literal("2001:db8::1"));
    }

    #[test]
    fn ip_literal_rejects_hostname() {
        assert!(!is_ip_literal("api.stripe.com"));
        assert!(!is_ip_literal("evil.com"));
    }

    #[test]
    fn localhost_aliases() {
        assert!(is_localhost("localhost"));
        assert!(is_localhost("LOCALHOST"));
        assert!(is_localhost("127.0.0.1"));
        assert!(is_localhost("::1"));
        assert!(is_localhost("[::1]"));
        assert!(is_localhost("::ffff:127.0.0.1"));
    }

    #[test]
    fn localhost_rejects_other_loopback_writes() {
        // 127.x.y.z (other loopback ranges) is not in the alias list;
        // we'd need to do real IP parsing to catch them. Documented
        // limitation: V2 catches the common literal strings; full
        // loopback-CIDR matching is V3.
        assert!(!is_localhost("127.0.0.2"));
    }

    #[test]
    fn whitelist_exact_match() {
        let wl: Vec<String> = vec!["api.x.com".into()];
        assert!(host_in_whitelist("api.x.com", &wl));
        assert!(host_in_whitelist("API.X.COM", &wl));
        assert!(!host_in_whitelist("x.com", &wl));
        assert!(!host_in_whitelist("api.x.com.evil.com", &wl));
    }
}
