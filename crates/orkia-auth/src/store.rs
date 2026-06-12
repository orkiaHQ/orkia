// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Token storage. Keychain on macOS / Linux / Windows; file fallback on
//! systems where the keyring crate's native backend is unavailable.
//!
//! The token never appears in logs, env vars, or shell history. The file
//! fallback writes mode 0600 on Unix.
//!
//! The trait is generic over the metadata payload `M`, so consumers
//! choose their own shape (typically a struct shared with their
//! backend). `orkia-auth` itself does not impose a metadata schema.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

/// Initialize the platform-native keyring store exactly once. keyring v4
/// requires an explicit default store registration before `Entry::new`
/// succeeds.
fn ensure_default_store() -> bool {
    static INIT: OnceLock<bool> = OnceLock::new();
    *INIT.get_or_init(|| keyring::use_native_store(false).is_ok())
}

#[derive(Debug, Error)]
pub enum TokenStoreError {
    #[error("keyring: {0}")]
    Keyring(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(String),
}

/// Storage primitive for `(token, metadata)` pairs. Implementations
/// persist the bearer token securely and serialize `M` alongside it.
///
/// `M` carries whatever profile data the consumer needs (plan, user
/// identifiers, expiry). The trait stays neutral — it does not parse,
/// validate, or interpret the content.
pub trait TokenStore<M>: Send + Sync
where
    M: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn save(&self, token: &str, metadata: &M) -> Result<(), TokenStoreError>;
    fn load(&self) -> Result<Option<(String, M)>, TokenStoreError>;
    fn clear(&self) -> Result<(), TokenStoreError>;
}

/// Env var forcing a file-backed session store at the given path. Set by
/// headless harnesses (e2e / qa / demos) that can't reach a desktop
/// keychain in CI: they write a REAL backend session (signed JWT + plan)
/// there, which the shell loads exactly like a keychain login. Not a
/// bypass — the plan still comes from the signed token, re-validated by
/// the kernel manifest/heartbeat, never from a client-side assertion.
pub const SESSION_FILE_ENV: &str = "ORKIA_SESSION_FILE";

/// Default store. `ORKIA_SESSION_FILE` (if set) forces a file store at
/// that path; otherwise keychain when supported, else the `~/.orkia`
/// file fallback. `service` becomes the keychain entry name
/// (e.g. `"dev.orkia.cli"`).
pub fn default_store<M>(service: &str) -> Box<dyn TokenStore<M>>
where
    M: Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
{
    if let Some(path) = std::env::var_os(SESSION_FILE_ENV) {
        return Box::new(FileStore::<M>::new(PathBuf::from(path)));
    }
    if KeyringStore::<M>::is_supported() {
        Box::new(KeyringStore::<M>::new(service))
    } else {
        Box::new(FileStore::<M>::default_path())
    }
}

/// `~/.orkia/auth.toml` — the file fallback path.
pub fn file_store_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".orkia").join("auth.toml")
}

// ── KeyringStore ─────────────────────────────────────────────────────

pub struct KeyringStore<M> {
    service: String,
    token_user: &'static str,
    metadata_user: &'static str,
    _marker: PhantomData<fn() -> M>,
}

impl<M> KeyringStore<M> {
    pub fn new(service: &str) -> Self {
        Self {
            service: service.into(),
            token_user: "api-token",
            metadata_user: "api-token-metadata",
            _marker: PhantomData,
        }
    }

    /// Whether the running platform has a usable backend. Used by
    /// `default_store` to choose between keychain and file storage.
    pub fn is_supported() -> bool {
        // keyring v4 requires a default store to be registered before
        // `Entry::new` works. If registration fails (e.g. sandbox blocks
        // the platform backend), the keyring path is unsupported here.
        if !ensure_default_store() {
            return false;
        }
        keyring_core::Entry::new("dev.orkia.cli.probe", "probe")
            .map(|e| {
                // Touch the backend without writing anything.
                e.get_password().map(|_| ()).err();
                true
            })
            .unwrap_or(false)
    }

    fn token_entry(&self) -> Result<keyring_core::Entry, TokenStoreError> {
        ensure_default_store();
        keyring_core::Entry::new(&self.service, self.token_user)
            .map_err(|e| TokenStoreError::Keyring(e.to_string()))
    }

    fn metadata_entry(&self) -> Result<keyring_core::Entry, TokenStoreError> {
        ensure_default_store();
        keyring_core::Entry::new(&self.service, self.metadata_user)
            .map_err(|e| TokenStoreError::Keyring(e.to_string()))
    }
}

impl<M> TokenStore<M> for KeyringStore<M>
where
    M: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn save(&self, token: &str, metadata: &M) -> Result<(), TokenStoreError> {
        let meta_json =
            serde_json::to_string(metadata).map_err(|e| TokenStoreError::Serde(e.to_string()))?;
        self.token_entry()?
            .set_password(token)
            .map_err(|e| TokenStoreError::Keyring(e.to_string()))?;
        self.metadata_entry()?
            .set_password(&meta_json)
            .map_err(|e| TokenStoreError::Keyring(e.to_string()))?;
        Ok(())
    }

    fn load(&self) -> Result<Option<(String, M)>, TokenStoreError> {
        let token = match self.token_entry()?.get_password() {
            Ok(t) => t,
            Err(keyring_core::Error::NoEntry) => return Ok(None),
            Err(e) => return Err(TokenStoreError::Keyring(e.to_string())),
        };
        let meta_raw = match self.metadata_entry()?.get_password() {
            Ok(m) => m,
            Err(keyring_core::Error::NoEntry) => {
                // Token present but metadata gone (non-atomic save/clear, or a
                // lost metadata entry). Clean up the orphan secret rather than
                // leaving it in the keychain while reporting "logged out"
                // (BUG-083).
                tracing::warn!("auth: token present without metadata; removing orphan token");
                if let Err(e) = self.token_entry()?.delete_credential() {
                    if !matches!(e, keyring_core::Error::NoEntry) {
                        tracing::warn!(error = %e, "auth: failed to remove orphan token");
                    }
                }
                return Ok(None);
            }
            Err(e) => return Err(TokenStoreError::Keyring(e.to_string())),
        };
        let metadata: M =
            serde_json::from_str(&meta_raw).map_err(|e| TokenStoreError::Serde(e.to_string()))?;
        Ok(Some((token, metadata)))
    }

    fn clear(&self) -> Result<(), TokenStoreError> {
        match self.token_entry()?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => {}
            Err(e) => return Err(TokenStoreError::Keyring(e.to_string())),
        }
        match self.metadata_entry()?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => {}
            Err(e) => return Err(TokenStoreError::Keyring(e.to_string())),
        }
        Ok(())
    }
}

// ── FileStore ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct OnDisk<M> {
    token: String,
    #[serde(flatten)]
    metadata: M,
}

pub struct FileStore<M> {
    path: PathBuf,
    _marker: PhantomData<fn() -> M>,
}

impl<M> FileStore<M> {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            _marker: PhantomData,
        }
    }

    pub fn default_path() -> Self {
        Self::new(file_store_path())
    }
}

impl<M> TokenStore<M> for FileStore<M>
where
    M: Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
{
    fn save(&self, token: &str, metadata: &M) -> Result<(), TokenStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let on_disk = OnDisk::<M> {
            token: token.into(),
            metadata: metadata.clone(),
        };
        let raw =
            toml::to_string_pretty(&on_disk).map_err(|e| TokenStoreError::Serde(e.to_string()))?;
        write_secure(&self.path, raw.as_bytes())
    }

    fn load(&self) -> Result<Option<(String, M)>, TokenStoreError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&self.path)?;
        let on_disk: OnDisk<M> =
            toml::from_str(&raw).map_err(|e| TokenStoreError::Serde(e.to_string()))?;
        Ok(Some((on_disk.token, on_disk.metadata)))
    }

    fn clear(&self) -> Result<(), TokenStoreError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(TokenStoreError::Io(e)),
        }
    }
}

/// Write `data` to `path` with restrictive permissions (Unix mode 0600).
/// On Windows the call falls back to a plain write — ACL handling there is
/// out of scope for V1.
fn write_secure(path: &Path, data: &[u8]) -> Result<(), TokenStoreError> {
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
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use tempfile::TempDir;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct TestMeta {
        username: String,
        plan: String,
    }

    fn fixture() -> TestMeta {
        TestMeta {
            username: "testuser".into(),
            plan: "free".into(),
        }
    }

    #[test]
    fn file_store_round_trip() {
        let tmp = TempDir::new().unwrap();
        let s = FileStore::<TestMeta>::new(tmp.path().join("auth.toml"));
        assert!(s.load().unwrap().is_none());
        let meta = fixture();
        s.save("tok-abc", &meta).unwrap();
        let loaded = s.load().unwrap().unwrap();
        assert_eq!(loaded.0, "tok-abc");
        assert_eq!(loaded.1, meta);
        s.clear().unwrap();
        assert!(s.load().unwrap().is_none());
    }

    #[test]
    fn file_store_clear_missing_is_noop() {
        let tmp = TempDir::new().unwrap();
        let s = FileStore::<TestMeta>::new(tmp.path().join("never-existed.toml"));
        s.clear().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_store_uses_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("auth.toml");
        let s = FileStore::<TestMeta>::new(p.clone());
        s.save("tok", &fixture()).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
