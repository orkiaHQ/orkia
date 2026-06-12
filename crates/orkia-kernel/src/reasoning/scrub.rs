// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
//! Summary scrubbing + content hashing for the hot path. Every summary is
//! PII-scrubbed and length-capped *before* it is stored or leaves the
//! consumer. If scrubbing empties the text, the caller keeps a hash-only
//! record and drops the summary.

use sha2::{Digest, Sha256};

/// Max stored summary length in bytes. Cut on a char boundary.
const SUMMARY_CAP_BYTES: usize = 1024;
/// A token at/above this length that mixes letters and digits is treated as a
/// secret/opaque id and redacted.
const SECRET_MIN_LEN: usize = 24;

/// Redact obvious PII (emails, secret-like tokens) and cap the length.
/// Returns the scrubbed string, which may be empty.
pub fn scrub_summary(input: &str) -> String {
    let mut parts: Vec<String> = input.split_whitespace().map(redact_token).collect();
    // Rejoin with single spaces (also normalizes whitespace).
    let mut out = parts.join(" ");
    if out.len() > SUMMARY_CAP_BYTES {
        out = cut_on_char_boundary(&out, SUMMARY_CAP_BYTES).to_string();
    }
    parts.clear();
    out
}

fn redact_token(tok: &str) -> String {
    if looks_like_email(tok) {
        return "[redacted-email]".to_string();
    }
    if looks_like_secret(tok) {
        return "[redacted-token]".to_string();
    }
    tok.to_string()
}

fn looks_like_email(tok: &str) -> bool {
    match tok.split_once('@') {
        Some((user, domain)) => !user.is_empty() && domain.contains('.') && !domain.ends_with('.'),
        None => false,
    }
}

fn looks_like_secret(tok: &str) -> bool {
    let core = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if core.len() < SECRET_MIN_LEN {
        return false;
    }
    let has_digit = core.bytes().any(|b| b.is_ascii_digit());
    let has_alpha = core.bytes().any(|b| b.is_ascii_alphabetic());
    let all_token_chars = core
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'+' || b == b'/');
    has_digit && has_alpha && all_token_chars
}

fn cut_on_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// SHA-256 of the raw (pre-scrub) content, hex-encoded. Stored even when the
/// summary is dropped, so deduplication and integrity survive scrubbing.
pub fn content_hash(raw: &str) -> String {
    let mut h = Sha256::new();
    h.update(raw.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_email() {
        let out = scrub_summary("contact me at user@example.test please");
        assert!(out.contains("[redacted-email]"));
        assert!(!out.contains("example.test"));
    }

    #[test]
    fn redacts_secret_like_token() {
        let out = scrub_summary("token sk-live-abc123DEF456ghi789JKL0 used");
        assert!(out.contains("[redacted-token]"));
        assert!(!out.contains("abc123DEF456ghi789JKL0"));
    }

    #[test]
    fn keeps_ordinary_prose() {
        let out = scrub_summary("ran the build and it passed");
        assert_eq!(out, "ran the build and it passed");
    }

    #[test]
    fn caps_length_on_char_boundary() {
        let long = "é".repeat(2000); // 2-byte chars
        let out = scrub_summary(&long);
        assert!(out.len() <= SUMMARY_CAP_BYTES);
        // Still valid UTF-8 (no panic, no broken char).
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn hash_is_stable_and_hex() {
        let a = content_hash("hello");
        assert_eq!(a, content_hash("hello"));
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, content_hash("world"));
    }

    #[test]
    fn scrub_can_yield_empty() {
        assert_eq!(scrub_summary("   "), "");
    }

    /// A small deterministic LCG so the fuzz corpus is reproducible (no
    /// `rand` dep, and no nondeterministic test inputs).
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state
    }

    /// #7: every byte is untrusted. Scrub must never panic and must always
    /// honor the byte cap + UTF-8 validity, no matter the input — including
    /// control bytes, lone `@`, runs of digits/letters, and multibyte glyphs.
    #[test]
    fn scrub_never_panics_on_adversarial_input() {
        // A charset mixing the structural triggers (@, -, _, /, digits,
        // letters), whitespace variants, control chars, and multibyte glyphs.
        let charset: Vec<char> = "@.-_/+aZ09 \t\n\r\u{0}\u{7f}é🦀\u{200b}".chars().collect();
        let mut state: u64 = 0x0150_0DAD_u64; // fixed seed
        for _ in 0..5000 {
            let len = (lcg(&mut state) % 80) as usize;
            let s: String = (0..len)
                .map(|_| charset[(lcg(&mut state) as usize) % charset.len()])
                .collect();
            let out = scrub_summary(&s);
            // Never exceeds the cap; always valid UTF-8 (String guarantees it).
            assert!(out.len() <= SUMMARY_CAP_BYTES);
            // Scrubbing is idempotent: re-scrubbing scrubbed output is stable.
            assert_eq!(scrub_summary(&out), out, "non-idempotent on {s:?}");
        }
    }

    /// Pathologically long single tokens (no whitespace) must still be capped
    /// without panicking on a non-char-boundary cut.
    #[test]
    fn scrub_caps_giant_single_token() {
        let giant = "a".repeat(100_000);
        assert!(scrub_summary(&giant).len() <= SUMMARY_CAP_BYTES);
        let giant_multibyte = "🦀".repeat(50_000);
        let out = scrub_summary(&giant_multibyte);
        assert!(out.len() <= SUMMARY_CAP_BYTES);
        assert!(out.chars().all(|c| c == '🦀'));
    }
}
