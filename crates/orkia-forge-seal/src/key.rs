// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Per-app ECDSA P-256 signing key.
//!
//! On first use, [`SealKey::load_or_generate`] creates a fresh key and
//! writes it to `<app-dir>/seal/signing.pem` with restrictive permissions
//! (Unix mode 0600). Subsequent loads read the existing PEM.
//!
//! V2 design choice: each app has its own key, generated locally. There
//! is no central PKI. The signature is for tamper-detection on the local
//! JSONL, not for remote attestation (remote attestation is V3 +
//! cross-reference to the global runtime SEAL log).

use std::path::{Path, PathBuf};

use p256::ecdsa::{SigningKey, VerifyingKey};
use p256::pkcs8::{
    DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey, LineEnding,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SealKeyError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("pem parse: {0}")]
    Pem(String),
    #[error("path has no parent directory")]
    NoParent,
}

/// Per-app signing key. Carries the private key for signing + the
/// derived verifying key for in-process verification.
pub struct SealKey {
    signing: SigningKey,
    verifying: VerifyingKey,
    path: PathBuf,
}

impl SealKey {
    /// Load `<seal-dir>/signing.pem` if present; otherwise generate a
    /// fresh key and persist it. Sets Unix mode 0600 on write.
    pub fn load_or_generate(seal_dir: &Path) -> Result<Self, SealKeyError> {
        let path = seal_dir.join("signing.pem");
        if path.exists() {
            return Self::load(&path);
        }
        Self::generate_and_save(&path)
    }

    /// Load an existing PEM file.
    pub fn load(path: &Path) -> Result<Self, SealKeyError> {
        let pem = std::fs::read_to_string(path)?;
        let signing =
            SigningKey::from_pkcs8_pem(&pem).map_err(|e| SealKeyError::Pem(e.to_string()))?;
        let verifying = *signing.verifying_key();
        Ok(Self {
            signing,
            verifying,
            path: path.to_path_buf(),
        })
    }

    /// Generate + save a new key. Used when no existing key file is found.
    pub fn generate_and_save(path: &Path) -> Result<Self, SealKeyError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        } else {
            return Err(SealKeyError::NoParent);
        }
        // p256 0.13 uses `rand_core` 0.6 via its elliptic-curve re-export;
        // workspace `rand` 0.9 implements a different RngCore trait so we
        // can't pass `rand::rng()` here. p256 re-exports `OsRng` which
        // implements the older `CryptoRngCore` that `SigningKey::random`
        // wants — that's the path that works without bridging rng_core
        // versions ourselves.
        use p256::elliptic_curve::rand_core::OsRng;
        let signing = SigningKey::random(&mut OsRng);
        let verifying = *signing.verifying_key();
        let pem = signing
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| SealKeyError::Pem(e.to_string()))?;
        write_secure(path, pem.as_bytes())?;
        Ok(Self {
            signing,
            verifying,
            path: path.to_path_buf(),
        })
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.signing
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying
    }

    /// PEM-encoded SubjectPublicKeyInfo for the public half. Used by
    /// `orkia app seal --verify` to cache the pubkey for offline checks.
    pub fn public_key_pem(&self) -> Result<String, SealKeyError> {
        self.verifying
            .to_public_key_pem(LineEnding::LF)
            .map_err(|e| SealKeyError::Pem(e.to_string()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Helper for verifier code paths that already have a public PEM in
    /// hand (e.g. cached from a previous check).
    pub fn verifying_from_public_pem(pem: &str) -> Result<VerifyingKey, SealKeyError> {
        VerifyingKey::from_public_key_pem(pem).map_err(|e| SealKeyError::Pem(e.to_string()))
    }
}

fn write_secure(path: &Path, data: &[u8]) -> Result<(), SealKeyError> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn generate_persists_pem() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("signing.pem");
        let _k = SealKey::generate_and_save(&p).unwrap();
        let pem = std::fs::read_to_string(&p).unwrap();
        assert!(pem.contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn load_or_generate_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let k1 = SealKey::load_or_generate(tmp.path()).unwrap();
        let pem1 = std::fs::read_to_string(tmp.path().join("signing.pem")).unwrap();
        let k2 = SealKey::load_or_generate(tmp.path()).unwrap();
        let pem2 = std::fs::read_to_string(tmp.path().join("signing.pem")).unwrap();
        assert_eq!(pem1, pem2, "second load must not overwrite");
        // Public keys must match across loads.
        assert_eq!(k1.public_key_pem().unwrap(), k2.public_key_pem().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("signing.pem");
        SealKey::generate_and_save(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn public_pem_round_trip() {
        let tmp = TempDir::new().unwrap();
        let k = SealKey::load_or_generate(tmp.path()).unwrap();
        let pem = k.public_key_pem().unwrap();
        let v = SealKey::verifying_from_public_pem(&pem).unwrap();
        // Round-tripped public key matches.
        assert_eq!(
            v.to_public_key_pem(LineEnding::LF).unwrap(),
            k.public_key_pem().unwrap()
        );
    }
}
