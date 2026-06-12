// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Trust anchor for orkia-kernel downloads.
//!
//! Every `orkia-kernel` binary downloaded by [`orkia-kernel-installer`]
//! is verified against the Ed25519 public key embedded here at compile
//! time. The shipping build sets `ORKIA_KERNEL_PUBKEY_HEX` (64 hex
//! chars = 32 raw bytes) via `build.rs` / `cargo:rustc-env=`; absent
//! that, a development-only key takes its place so the OSS workspace
//! builds without secrets.
//!
//! **Production policy.** Replace the dev key by exporting
//! `ORKIA_KERNEL_PUBKEY_HEX=<hex>` in CI before `cargo build --release`.
//! A release build without `ORKIA_KERNEL_PUBKEY_HEX` set will fail to
//! compile (see the `#[cfg(not(debug_assertions))]` guard below).
//! Pubkey rotation requires shipping a new shell binary — that's
//! intentional: the trust root must not be mutable at runtime.
//!
//! **Dev key.** The dev keypair is derived from the seed
//! `sha256("orkia-dev-kernel-trust-v1")` =
//! `7612fde90b9a8b8558c7e24648cbd50eff60a6136f3570be1558f0a66c668944`.
//! The seed is NOT committed to the repo; the dev private key is
//! discarded after deriving `DEV_PUBKEY_HEX`. For local signing in
//! tests, the seed is re-derived inline (see `dev_signing_key` below).
//! **NEVER use this key for real distribution.**

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use thiserror::Error;

// SEC-003: non-zero dev key derived from sha256("orkia-dev-kernel-trust-v1").
// The matching 32-byte seed is NOT stored in the repo; tests re-derive it
// inline.  Production builds must override via ORKIA_KERNEL_PUBKEY_HEX.
const DEV_PUBKEY_HEX: &str = "6f5ca4313022f5e636b0cc6b176312c93e13e8fdfa0c233e82f0568032dd9677";

/// 64 hex chars (32 raw bytes) selected at compile time.
pub const EMBEDDED_PUBKEY_HEX: &str = match option_env!("ORKIA_KERNEL_PUBKEY_HEX") {
    Some(p) => p,
    None => DEV_PUBKEY_HEX,
};

// SEC-003: release builds without an injected key must not compile.
// This is a compile-time assert: option_env! evaluates at compile time.
#[cfg(not(debug_assertions))]
const _: () = {
    assert!(
        option_env!("ORKIA_KERNEL_PUBKEY_HEX").is_some(),
        "production build requires ORKIA_KERNEL_PUBKEY_HEX to be set; \
         set it to the 64-hex-char Ed25519 public key before `cargo build --release`"
    );
};

#[derive(Debug, Error)]
pub enum TrustError {
    #[error("embedded pubkey is malformed: {0}")]
    BadPubkey(String),
    #[error("signature is malformed: {0}")]
    BadSignature(String),
    #[error("signature verification failed")]
    BadVerification,
    #[error("hash mismatch (expected {expected}, actual {actual})")]
    HashMismatch { expected: String, actual: String },
}

/// Verify that `signature` (raw 64 bytes) is a valid Ed25519 sig over
/// `data`, produced by the holder of the private half of
/// [`EMBEDDED_PUBKEY_HEX`].
///
/// `data` is the binary bytes themselves — the installer signs the
/// raw file, not its hash, so a wrong-pubkey signature is detected
/// even if the SHA-256 happens to be misreported.
pub fn verify(data: &[u8], signature: &[u8]) -> Result<(), TrustError> {
    let key = load_embedded_pubkey()?;
    let sig_bytes: [u8; 64] = signature.try_into().map_err(|_| {
        TrustError::BadSignature(format!("expected 64 bytes, got {}", signature.len()))
    })?;
    let sig = Signature::from_bytes(&sig_bytes);
    // SEC-026: use verify_strict to reject weak-group points and non-canonical
    // signature components (RFC 8032 conformant; trust-anchor hardening).
    key.verify_strict(data, &sig)
        .map_err(|_| TrustError::BadVerification)
}

/// Convenience: verify both the SHA-256 (cheap integrity check) and
/// the Ed25519 signature (auth check). The installer calls this so a
/// single mismatch error covers both bait paths.
pub fn verify_with_hash(
    data: &[u8],
    expected_sha256_hex: &str,
    signature: &[u8],
) -> Result<(), TrustError> {
    let actual_hex = sha256_hex(data);
    if !actual_hex.eq_ignore_ascii_case(expected_sha256_hex) {
        return Err(TrustError::HashMismatch {
            expected: expected_sha256_hex.to_lowercase(),
            actual: actual_hex,
        });
    }
    verify(data, signature)
}

/// Hex SHA-256 of `data`. Exposed so callers can pre-compute and
/// surface a fingerprint without re-importing sha2.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex::encode(digest)
}

fn load_embedded_pubkey() -> Result<VerifyingKey, TrustError> {
    let raw = hex::decode(EMBEDDED_PUBKEY_HEX)
        .map_err(|e| TrustError::BadPubkey(format!("hex decode: {e}")))?;
    let arr: [u8; 32] = raw
        .as_slice()
        .try_into()
        .map_err(|_| TrustError::BadPubkey(format!("expected 32 bytes, got {}", raw.len())))?;
    VerifyingKey::from_bytes(&arr).map_err(|e| TrustError::BadPubkey(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// Reconstruct the dev signing key from the documented seed derivation.
    /// Seed = sha256("orkia-dev-kernel-trust-v1") — NOT [0u8;32].
    /// The private seed is never committed; tests derive it here on demand.
    fn dev_signing_key() -> SigningKey {
        use sha2::{Digest, Sha256};
        let seed: [u8; 32] = Sha256::digest(b"orkia-dev-kernel-trust-v1").into();
        SigningKey::from_bytes(&seed)
    }

    #[test]
    fn embedded_key_matches_dev_signing_key() {
        let sk = dev_signing_key();
        let derived_hex = hex::encode(sk.verifying_key().to_bytes());
        assert_eq!(derived_hex, DEV_PUBKEY_HEX);
        // When no env override is in effect, EMBEDDED_PUBKEY_HEX == DEV_PUBKEY_HEX.
        // (In CI with ORKIA_KERNEL_PUBKEY_HEX set, this test is about the dev key only.)
        if option_env!("ORKIA_KERNEL_PUBKEY_HEX").is_none() {
            assert_eq!(derived_hex, EMBEDDED_PUBKEY_HEX);
        }
    }

    #[test]
    fn good_signature_verifies() {
        let sk = dev_signing_key();
        let data = b"orkia-kernel build artifact bytes";
        let sig = sk.sign(data);
        verify(data, &sig.to_bytes()).unwrap();
    }

    #[test]
    fn tampered_data_fails() {
        let sk = dev_signing_key();
        let data = b"original";
        let sig = sk.sign(data);
        let err = verify(b"tampered", &sig.to_bytes()).unwrap_err();
        assert!(matches!(err, TrustError::BadVerification));
    }

    #[test]
    fn wrong_signature_length_is_clean_error() {
        let err = verify(b"data", &[0u8; 32]).unwrap_err();
        assert!(matches!(err, TrustError::BadSignature(_)));
    }

    #[test]
    fn verify_with_hash_catches_mismatch_before_sig() {
        let sk = dev_signing_key();
        let data = b"hello";
        let sig = sk.sign(data);
        let err = verify_with_hash(data, "deadbeef", &sig.to_bytes()).unwrap_err();
        assert!(matches!(err, TrustError::HashMismatch { .. }));
    }

    #[test]
    fn verify_with_hash_happy_path() {
        let sk = dev_signing_key();
        let data = b"hello";
        let sig = sk.sign(data);
        let h = sha256_hex(data);
        verify_with_hash(data, &h, &sig.to_bytes()).unwrap();
    }
}
