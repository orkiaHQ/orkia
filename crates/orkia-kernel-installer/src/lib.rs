// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Download, verify, install, and register the `orkia-kernel` daemon.
//!
//!
//! 1. [`Installer::fetch_manifest`] asks the Orkia API for the latest
//!    kernel build for the current platform. The response includes a
//!    pre-signed URL, SHA-256, and detached Ed25519 signature.
//! 2. [`Installer::download`] streams the binary to a `.partial` file
//!    next to the install path.
//! 3. [`Installer::verify`] (via `orkia-kernel-trust`) checks SHA-256
//!    plus Ed25519 against the embedded pubkey. Mismatch aborts and
//!    leaves the existing install untouched.
//! 4. [`Installer::install`] atomically renames `.partial` over the
//!    real binary path, sets executable bits.
//! 5. [`daemon::register`] writes a LaunchAgent (macOS) / systemd
//!    user unit (Linux) and starts it.
//!
//! All filesystem mutation goes through this crate. The shell only
//! ever calls the high-level entry points.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod daemon;
pub mod platform;

pub use platform::{Platform, current_platform};

/// Wire format returned by `GET /v1/kernel/manifest`. Schema v1 only;
/// later versions extend additively + bump `schema_version`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub kernel_version: String,
    pub minimum_shell_version: String,
    pub platform: String,
    pub binary: BinaryRef,
    /// Native runtime dependencies the binary loads via `@rpath` /
    /// `LD_LIBRARY_PATH` (e.g. the ggml / llama shared libraries). Each
    /// is co-installed next to the binary and the daemon unit points
    /// `DYLD_FALLBACK_LIBRARY_PATH` / `LD_LIBRARY_PATH` at that dir.
    /// Additive since schema v1; empty for statically-linked builds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_files: Vec<ExtraFile>,
    /// follow this to download GGUFs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_url: Option<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryRef {
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
    /// Base64-encoded Ed25519 signature over the raw binary bytes.
    pub signature: String,
}

/// A non-executable runtime artifact (shared library) co-installed
/// alongside the kernel binary. Independently signed: native code the
/// kernel dlopen's is as load-bearing as the binary itself, so it gets
/// the same Ed25519 + SHA-256 check (fail-closed, every byte untrusted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraFile {
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
    /// Base64-encoded Ed25519 signature over the raw file bytes.
    pub signature: String,
    /// Leaf filename written into the bin dir. Validated against path
    /// traversal before any filesystem touch.
    pub install_name: String,
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("manifest fetch failed: {0}")]
    Manifest(String),
    #[error("download failed: {0}")]
    Download(String),
    #[error("trust check failed: {0}")]
    Trust(#[from] orkia_kernel_trust::TrustError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest expired at {0}")]
    Expired(chrono::DateTime<chrono::Utc>),
    #[error("schema {0} not supported by this shell")]
    UnsupportedSchema(u32),
    #[error("invalid base64 signature: {0}")]
    BadSignature(String),
    #[error("HTTP client init failed: {0}")]
    HttpInit(String),
    #[error("invalid extra-file install_name: {0}")]
    BadInstallName(String),
}

/// Filesystem layout managed by the installer. Build with
/// [`Self::default_for_user`] or override paths in tests.
#[derive(Debug, Clone)]
pub struct InstallLayout {
    pub root: PathBuf,
}

impl InstallLayout {
    pub fn default_for_user() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            root: home.join(".orkia").join("kernel"),
        }
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    pub fn binary_path(&self) -> PathBuf {
        self.bin_dir().join("orkia-kernel")
    }

    pub fn partial_path(&self) -> PathBuf {
        self.bin_dir().join("orkia-kernel.new")
    }

    pub fn backup_path(&self) -> PathBuf {
        self.bin_dir().join("orkia-kernel.bak")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.bin_dir())?;
        std::fs::create_dir_all(self.logs_dir())?;
        Ok(())
    }
}

/// High-level installer entry point.
pub struct Installer {
    layout: InstallLayout,
    http: reqwest::Client,
}

impl Installer {
    /// # Errors
    ///
    /// Returns [`InstallError::HttpInit`] if the HTTP client cannot be built
    /// (e.g. TLS backend init failure). The shell is infrastructure: a library
    /// constructor must never panic, so we surface the failure instead of the
    /// old `Client::new()` fallback, which could itself panic in exactly this
    /// case (BUG-N02).
    pub fn new(layout: InstallLayout) -> Result<Self, InstallError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("orkia-shell/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| InstallError::HttpInit(e.to_string()))?;
        Ok(Self { layout, http })
    }

    pub fn layout(&self) -> &InstallLayout {
        &self.layout
    }

    /// Whether the binary currently lives at the expected install path.
    pub fn is_installed(&self) -> bool {
        self.layout.binary_path().exists()
    }

    /// `GET <api_base>/v1/kernel/manifest?platform=<p>&current_version=<v>`.
    /// Returns the parsed manifest. The Bearer token is supplied by
    /// the caller — the installer has no opinions on how the OAuth
    /// flow runs.
    pub async fn fetch_manifest(
        &self,
        api_base: &str,
        bearer: &str,
        platform: Platform,
        current_version: Option<&str>,
    ) -> Result<Manifest, InstallError> {
        let mut url = format!(
            "{}/v1/kernel/manifest?platform={}",
            api_base.trim_end_matches('/'),
            platform.as_str()
        );
        if let Some(v) = current_version {
            // SEC-051: validate before concatenating into the URL query string.
            // Allow only version-safe chars: [A-Za-z0-9.+-]
            if !v
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
            {
                return Err(InstallError::Manifest(format!(
                    "current_version contains invalid characters: {v:?}"
                )));
            }
            url.push_str("&current_version=");
            url.push_str(v);
        }
        let resp = self
            .http
            .get(&url)
            .bearer_auth(bearer)
            .send()
            .await
            .map_err(|e| InstallError::Manifest(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Err(InstallError::Manifest("up to date (304)".into()));
        }
        if !resp.status().is_success() {
            return Err(InstallError::Manifest(format!(
                "status {} from {}",
                resp.status(),
                url
            )));
        }
        let parsed: Manifest = resp
            .json()
            .await
            .map_err(|e| InstallError::Manifest(format!("json: {e}")))?;
        if parsed.schema_version != 1 {
            return Err(InstallError::UnsupportedSchema(parsed.schema_version));
        }
        if parsed.expires_at < chrono::Utc::now() {
            return Err(InstallError::Expired(parsed.expires_at));
        }
        Ok(parsed)
    }

    /// Stream the binary to `partial_path`. Pre-existing partial files
    /// are truncated. The caller controls retries. The same bearer as
    /// the manifest fetch — the artifacts route is gated identically.
    pub async fn download(
        &self,
        manifest: &Manifest,
        bearer: &str,
    ) -> Result<Vec<u8>, InstallError> {
        self.layout.ensure_dirs()?;
        self.fetch_bytes(&manifest.binary.url, manifest.binary.size_bytes, bearer)
            .await
    }

    /// Fetch `url`, enforcing the 512 MiB hard cap and an exact match
    /// against the manifest's declared `size` (both Content-Length and
    /// the buffered length). Shared by the binary and every extra file.
    async fn fetch_bytes(
        &self,
        url: &str,
        size: u64,
        bearer: &str,
    ) -> Result<Vec<u8>, InstallError> {
        // SEC-050: hard cap — no legitimate kernel artifact exceeds 512 MiB.
        const MAX_BYTES: u64 = 512 * 1024 * 1024;
        if size > MAX_BYTES {
            return Err(InstallError::Download(format!(
                "size_bytes {size} exceeds hard cap {MAX_BYTES}"
            )));
        }

        let resp = self
            .http
            .get(url)
            .bearer_auth(bearer)
            .send()
            .await
            .map_err(|e| InstallError::Download(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(InstallError::Download(format!(
                "status {} from {url}",
                resp.status()
            )));
        }

        // SEC-050: check Content-Length against the manifest before buffering.
        if let Some(content_len) = resp.content_length()
            && content_len != size
        {
            return Err(InstallError::Download(format!(
                "Content-Length {content_len} does not match size_bytes {size}"
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| InstallError::Download(e.to_string()))?;
        if (bytes.len() as u64) != size {
            return Err(InstallError::Download(format!(
                "size mismatch (expected {size} bytes, got {})",
                bytes.len()
            )));
        }
        Ok(bytes.to_vec())
    }

    /// Run SHA-256 + Ed25519 verification against the embedded
    /// pubkey. Returns Ok iff both checks pass.
    pub fn verify(&self, bytes: &[u8], manifest: &Manifest) -> Result<(), InstallError> {
        verify_signed(bytes, &manifest.binary.sha256, &manifest.binary.signature)
    }

    /// Download, verify, and place every [`ExtraFile`] into the bin dir
    /// (mode 644 — shared libraries, not executables). Each artifact is
    /// SHA-256 + Ed25519 checked exactly like the binary; a failure on
    /// any one aborts before it touches the filesystem.
    pub async fn install_extra_files(
        &self,
        manifest: &Manifest,
        bearer: &str,
    ) -> Result<(), InstallError> {
        if manifest.extra_files.is_empty() {
            return Ok(());
        }
        self.layout.ensure_dirs()?;
        for ef in &manifest.extra_files {
            let name = sanitize_install_name(&ef.install_name)?;
            let bytes = self.fetch_bytes(&ef.url, ef.size_bytes, bearer).await?;
            verify_signed(&bytes, &ef.sha256, &ef.signature)?;
            write_file(&self.layout.bin_dir().join(name), &bytes, 0o644)?;
        }
        Ok(())
    }

    /// Atomically install verified bytes at `binary_path`. The
    /// previous binary (if any) is rotated to `.bak` so a manual
    /// rollback is one mv away.
    pub fn install(&self, bytes: &[u8]) -> Result<(), InstallError> {
        self.layout.ensure_dirs()?;
        let partial = self.layout.partial_path();
        let final_ = self.layout.binary_path();
        write_executable(&partial, bytes)?;
        if final_.exists() {
            // Fail closed: the docstring promises a `.bak` rollback. The old
            // `let _ =` swallowed a failed rotation, then the rename below
            // silently overwrote the old binary with no rollback (BUG-081).
            std::fs::rename(&final_, self.layout.backup_path())?;
        }
        std::fs::rename(&partial, &final_)?;
        Ok(())
    }

    /// End-to-end happy path: fetch → download → verify → install.
    /// Returns the freshly installed manifest.
    pub async fn install_latest(
        &self,
        api_base: &str,
        bearer: &str,
        platform: Platform,
    ) -> Result<Manifest, InstallError> {
        let manifest = self
            .fetch_manifest(api_base, bearer, platform, None)
            .await?;
        let bytes = self.download(&manifest, bearer).await?;
        self.verify(&bytes, &manifest)?;
        self.install(&bytes)?;
        self.install_extra_files(&manifest, bearer).await?;
        Ok(manifest)
    }
}

/// SHA-256 + Ed25519 check against the embedded trust anchor. The
/// signature is base64 (STANDARD) over the raw bytes.
fn verify_signed(bytes: &[u8], sha256: &str, signature_b64: &str) -> Result<(), InstallError> {
    use base64::Engine as _;
    let sig = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .map_err(|e| InstallError::BadSignature(e.to_string()))?;
    orkia_kernel_trust::verify_with_hash(bytes, sha256, &sig)?;
    Ok(())
}

/// Reject anything that isn't a bare leaf filename: no separators, no
/// `..`, no absolute paths, no empties. Fail-closed against an attacker
/// who controls the manifest writing outside the bin dir.
fn sanitize_install_name(name: &str) -> Result<&str, InstallError> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0');
    if bad {
        return Err(InstallError::BadInstallName(name.to_string()));
    }
    Ok(name)
}

fn write_executable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_file(path, bytes, 0o755)
}

fn write_file(path: &Path, bytes: &[u8], _mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(_mode)
            .open(path)?;
        f.write_all(bytes)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

// base64 is re-exported behind a small wrapper to avoid bumping the
// orkia workspace lockfile for a single use site.
mod base64 {
    pub use base64::*;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use tempfile::TempDir;

    fn dev_sk() -> SigningKey {
        // Must match the dev key in orkia-kernel-trust (sha256-derived, not zero-seed).
        use sha2::{Digest, Sha256};
        let seed: [u8; 32] = Sha256::digest(b"orkia-dev-kernel-trust-v1").into();
        SigningKey::from_bytes(&seed)
    }

    fn manifest_for(bytes: &[u8]) -> Manifest {
        use ::base64::Engine as _;
        let sk = dev_sk();
        let sig = sk.sign(bytes);
        Manifest {
            schema_version: 1,
            kernel_version: "0.1.0-test".into(),
            minimum_shell_version: "0.0.1".into(),
            platform: "test".into(),
            binary: BinaryRef {
                url: "file:///dev/null".into(),
                sha256: orkia_kernel_trust::sha256_hex(bytes),
                size_bytes: bytes.len() as u64,
                signature: ::base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
            },
            extra_files: vec![],
            models_url: None,
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        }
    }

    fn signed_extra(install_name: &str, bytes: &[u8]) -> ExtraFile {
        use ::base64::Engine as _;
        let sk = dev_sk();
        ExtraFile {
            url: "file:///dev/null".into(),
            sha256: orkia_kernel_trust::sha256_hex(bytes),
            size_bytes: bytes.len() as u64,
            signature: ::base64::engine::general_purpose::STANDARD
                .encode(sk.sign(bytes).to_bytes()),
            install_name: install_name.into(),
        }
    }

    #[test]
    fn verify_happy_path() {
        let tmp = TempDir::new().unwrap();
        let layout = InstallLayout {
            root: tmp.path().to_path_buf(),
        };
        let inst = Installer::new(layout).unwrap();
        let bytes = b"hello kernel";
        let m = manifest_for(bytes);
        inst.verify(bytes, &m).unwrap();
    }

    #[test]
    fn verify_detects_tampered_bytes() {
        let tmp = TempDir::new().unwrap();
        let layout = InstallLayout {
            root: tmp.path().to_path_buf(),
        };
        let inst = Installer::new(layout).unwrap();
        let original = b"hello kernel";
        let mut m = manifest_for(original);
        // Manifest signed against `original` but we hand in tampered bytes.
        let tampered = b"hello kernal"; // single-byte change
        m.binary.size_bytes = tampered.len() as u64;
        m.binary.sha256 = orkia_kernel_trust::sha256_hex(tampered);
        let err = inst.verify(tampered, &m).unwrap_err();
        // Hash matches (we updated it), but signature doesn't.
        assert!(matches!(
            err,
            InstallError::Trust(orkia_kernel_trust::TrustError::BadVerification)
        ));
    }

    /// One-shot loopback HTTP server: asserts the request carries the
    /// expected bearer, then serves `body`. The artifacts route is
    /// auth-gated, so an unauthenticated download is a regression.
    async fn serve_once_expecting_bearer(body: Vec<u8>, bearer: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = vec![0u8; 4096];
            let n = sock.read(&mut req).await.unwrap();
            let head = String::from_utf8_lossy(&req[..n]).to_lowercase();
            assert!(
                head.contains(&format!("authorization: bearer {bearer}")),
                "download request missing bearer auth"
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.write_all(&body).await.unwrap();
        });
        format!("http://{addr}/orkia-kernel")
    }

    #[tokio::test]
    async fn download_sends_bearer_auth() {
        let tmp = TempDir::new().unwrap();
        let layout = InstallLayout {
            root: tmp.path().to_path_buf(),
        };
        let inst = Installer::new(layout).unwrap();
        let bytes = b"kernel bytes".to_vec();
        let url = serve_once_expecting_bearer(bytes.clone(), "tok-123").await;
        let mut m = manifest_for(&bytes);
        m.binary.url = url;
        let got = inst.download(&m, "tok-123").await.unwrap();
        assert_eq!(got, bytes);
    }

    #[test]
    fn sanitize_install_name_rejects_traversal() {
        assert!(sanitize_install_name("libggml.0.dylib").is_ok());
        for bad in ["", ".", "..", "../evil", "a/b", "a\\b", "x\0y"] {
            assert!(
                sanitize_install_name(bad).is_err(),
                "expected reject for {bad:?}"
            );
        }
    }

    #[test]
    fn extra_file_verify_rejects_tamper() {
        let bytes = b"libggml bytes";
        let ef = signed_extra("libggml.0.dylib", bytes);
        verify_signed(bytes, &ef.sha256, &ef.signature).unwrap();
        let err = verify_signed(b"tampered", &ef.sha256, &ef.signature).unwrap_err();
        assert!(matches!(err, InstallError::Trust(_)));
    }

    #[test]
    fn install_atomically_swaps_binary() {
        let tmp = TempDir::new().unwrap();
        let layout = InstallLayout {
            root: tmp.path().to_path_buf(),
        };
        let inst = Installer::new(layout.clone()).unwrap();

        // First install
        inst.install(b"v1").unwrap();
        assert_eq!(std::fs::read(layout.binary_path()).unwrap(), b"v1");

        // Second install → previous moved to .bak
        inst.install(b"v2").unwrap();
        assert_eq!(std::fs::read(layout.binary_path()).unwrap(), b"v2");
        assert_eq!(std::fs::read(layout.backup_path()).unwrap(), b"v1");

        // Executable bit set
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(layout.binary_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755);
        }
    }
}
