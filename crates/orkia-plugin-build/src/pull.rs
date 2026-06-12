// Copyright 2026 Orkia
// SPDX-License-Identifier: Apache-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Apache License 2.0; see https://www.apache.org/licenses/LICENSE-2.0
// for terms.

//!
//! Javy is the QuickJS-in-WASM compiler. It is pulled on
//! demand and cached, **fail-closed**: the downloaded artifact is verified
//! against a pinned SHA-256 before use; a mismatch is refused. Resolution
//! order: `$ORKIA_JAVY` → cache → download+verify.
//!
//! distribution; this implements the same pull-and-verify mechanism against
//! the upstream Javy release. Pinned to Javy v8.1.1.)

use std::io::Read;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::error::CompileError;

const JAVY_VERSION: &str = "8.1.1";
const BASE: &str = "https://github.com/bytecodealliance/javy/releases/download/v8.1.1";

/// Maximum compressed archive size accepted over the network (32 MiB).
/// Javy releases are ~3–5 MiB; this gives 6× headroom while bounding memory.
const MAX_COMPRESSED_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum decompressed binary size (64 MiB). Fail-closed above this.
const MAX_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;

/// `(gz_url, gz_sha256)` for the current platform, or `None` if unsupported.
fn asset() -> Option<(String, &'static str)> {
    let (file, sha) = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        (
            "javy-arm-macos-v8.1.1.gz",
            "0ae154f026371aae1e82fb39381fd58e67ca6b2a2985fbce51d305b138dad59f",
        )
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        (
            "javy-x86_64-linux-v8.1.1.gz",
            "4bec224e1d51827808fd063541a2b4d94cf474744636b692be63e7a29bec83a6",
        )
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        (
            "javy-arm-linux-v8.1.1.gz",
            "c160127f4f41000216790e24bcfda860c51b0c5cc12808eb3691a0de88faa53a",
        )
    } else {
        return None;
    };
    Some((format!("{BASE}/{file}"), sha))
}

/// `~/.orkia/cache/compiler`.
fn cache_dir() -> Result<PathBuf, CompileError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| CompileError::Pull("HOME not set".to_string()))?;
    Ok(home.join(".orkia").join("cache").join("compiler"))
}

/// Resolve the Javy binary: `$ORKIA_JAVY`, else the cache, else download +
/// verify + cache. The cached/env binary is trusted (verified at install).
pub fn ensure_javy() -> Result<PathBuf, CompileError> {
    if let Some(p) = std::env::var_os("ORKIA_JAVY") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    let dir = cache_dir()?;
    let bin = dir.join("javy");
    if bin.is_file() {
        return Ok(bin);
    }
    download_and_verify(&dir, &bin)?;
    Ok(bin)
}

fn download_and_verify(dir: &std::path::Path, bin: &std::path::Path) -> Result<(), CompileError> {
    let (url, expected_sha) = asset().ok_or_else(|| {
        CompileError::Pull(format!(
            "no pinned Javy {JAVY_VERSION} artifact for this platform; \
             set $ORKIA_JAVY to a Javy binary"
        ))
    })?;

    // Bound the network read: refuse bodies larger than MAX_COMPRESSED_BYTES.
    // An unbounded read_to_end lets a hostile server exhaust host memory before
    // the SHA-256 check runs. Read::take caps the byte count at the source.
    let mut gz = Vec::new();
    ureq::get(&url)
        .call()
        .map_err(|e| CompileError::Pull(format!("download {url}: {e}")))?
        .into_body()
        .into_reader()
        .take(MAX_COMPRESSED_BYTES)
        .read_to_end(&mut gz)
        .map_err(|e| CompileError::Pull(format!("read body: {e}")))?;

    // Fail-closed: verify the pinned SHA-256 of the compressed artifact.
    let actual = hex::encode(Sha256::digest(&gz));
    if actual != expected_sha {
        return Err(CompileError::Pull(format!(
            "Javy checksum mismatch (expected {expected_sha}, got {actual}) — refusing"
        )));
    }

    // Bound the decompressed output: a crafted gzip bomb could expand a small
    // archive into an arbitrarily large stream. take() caps the decompressor.
    let decoder = flate2::read::GzDecoder::new(&gz[..]);
    let mut bytes = Vec::new();
    decoder
        .take(MAX_DECOMPRESSED_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| CompileError::Pull(format!("gunzip: {e}")))?;

    std::fs::create_dir_all(dir).map_err(|e| CompileError::Pull(e.to_string()))?;
    // Install atomically: write+chmod a unique temp file, then rename it into
    // place. Two callers racing on a cold cache (cargo runs the compile_e2e
    // tests as concurrent threads; in production, two plugins compiling at
    // once) must never exec `bin` while another is mid-write — that surfaces
    // as ETXTBSY (Linux) or a truncated, unexecutable Mach-O (macOS). The temp
    // name carries the pid and a process-wide counter so threads never collide.
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("javy.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|e| CompileError::Pull(e.to_string()))?;
    set_executable(&tmp)?;
    std::fs::rename(&tmp, bin).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        CompileError::Pull(e.to_string())
    })?;
    Ok(())
}

/// Process-wide counter making each in-flight temp install name unique across
/// threads (cargo test runs tests as threads in one process, sharing the pid).
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(unix)]
fn set_executable(path: &std::path::Path) -> Result<(), CompileError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| CompileError::Pull(e.to_string()))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| CompileError::Pull(e.to_string()))
}

#[cfg(not(unix))]
fn set_executable(_path: &std::path::Path) -> Result<(), CompileError> {
    Ok(())
}
