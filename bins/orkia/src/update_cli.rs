// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia update` — self-updater for the shell binaries, plus
//! `--kernel` to update the premium kernel daemon.
//!
//! Trust model mirrors `curl orkia.dev/install | sh`: the release
//! manifest is signed with the release Ed25519 key (SSHSIG), verified
//! here through the preinstalled `ssh-keygen -Y verify` — the exact
//! same chain as install.sh, no separate Rust crypto path to keep in
//! sync. Each binary's sha256 is then checked against the verified
//! manifest. Fail-closed throughout: any verification gap aborts.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use orkia_shell::ShellConfig;

/// Release signing public key, compiled in from the repo-root canonical
/// file `release-pubkey.pub`. release.yml fail-closes the publish job if
/// the ORKIA_RELEASE_SSH_SK secret's public half drifts from that file;
/// only install.sh (landing repo) still embeds its own copy, by design —
/// it is the TLS-delivered trust root and must stay self-contained.
const PUBKEY_SSH: &str = include_str!("../../../../release-pubkey.pub");

/// Commit this binary was built from. Stamped by release.yml via the
/// ORKIA_BUILD_COMMIT env at compile time; `None` for local builds.
const BUILD_COMMIT: Option<&str> = option_env!("ORKIA_BUILD_COMMIT");

/// `manifest.json` as produced by release.yml's publish job.
#[derive(serde::Deserialize)]
struct ReleaseManifest {
    commit: String,
    platforms: HashMap<String, PlatformAsset>,
}

#[derive(serde::Deserialize)]
struct PlatformAsset {
    sha256: String,
}

pub(crate) async fn run(args: &[String]) -> i32 {
    let mut check_only = false;
    let mut kernel = false;
    for a in args {
        match a.as_str() {
            "--check" => check_only = true,
            "--kernel" => kernel = true,
            "-h" | "--help" => {
                print_help();
                return 0;
            }
            other => {
                eprintln!("orkia update: unknown argument: {other}");
                print_help();
                return 2;
            }
        }
    }
    if kernel {
        run_kernel_update().await
    } else {
        run_shell_update(check_only).await
    }
}

fn print_help() {
    eprintln!(
        "Usage: orkia update [--check] [--kernel]

  (no flags)   Download, verify and install the latest shell binaries.
  --check      Report whether an update is available; change nothing.
  --kernel     Update the premium kernel daemon instead (requires login).

ENV:
  ORKIA_REPO      GitHub repo to fetch from (default orkiaHQ/orkia).
  ORKIA_VERSION   Release tag (default `latest`, the rolling release)."
    );
}

/// `https://github.com/<repo>/releases/download/<tag>` — same override
/// env vars as install.sh so test repos work identically.
fn release_base() -> String {
    let repo = std::env::var("ORKIA_REPO").unwrap_or_else(|_| "orkiaHQ/orkia".into());
    let tag = std::env::var("ORKIA_VERSION").unwrap_or_else(|_| "latest".into());
    format!("https://github.com/{repo}/releases/download/{tag}")
}

async fn run_shell_update(check_only: bool) -> i32 {
    let Some(platform) = orkia_kernel_installer::current_platform() else {
        eprintln!("orkia update: unsupported platform");
        return 1;
    };
    let manifest = match fetch_verified_manifest().await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("orkia update: {e}");
            return 1;
        }
    };
    let Some(asset) = manifest.platforms.get(platform.as_str()) else {
        eprintln!(
            "orkia update: platform {} not in this release",
            platform.as_str()
        );
        return 1;
    };
    let up_to_date = BUILD_COMMIT == Some(manifest.commit.as_str());
    print_status(&manifest.commit, up_to_date);
    if check_only {
        print_kernel_status();
        return 0;
    }
    if up_to_date {
        return 0;
    }
    match download_and_install(platform.as_str(), asset).await {
        Ok(dest) => {
            println!("✓ installed to {}", dest.display());
            print_daemon_note();
            println!("Restart orkia to run the new version.");
            0
        }
        Err(e) => {
            eprintln!("orkia update: {e}");
            1
        }
    }
}

fn print_status(latest_commit: &str, up_to_date: bool) {
    let short: String = latest_commit.chars().take(7).collect();
    match BUILD_COMMIT {
        Some(c) if up_to_date => {
            println!(
                "orkia {} ({}) — up to date",
                env!("CARGO_PKG_VERSION"),
                &c[..7.min(c.len())]
            );
        }
        Some(c) => {
            println!(
                "orkia {} ({}) — update available ({short})",
                env!("CARGO_PKG_VERSION"),
                &c[..7.min(c.len())]
            );
        }
        None => {
            println!(
                "orkia {} (dev build, no embedded commit) — latest release is {short}",
                env!("CARGO_PKG_VERSION")
            );
        }
    }
}

/// `--check` extra: surface the local kernel daemon's handshake so the
/// user sees protocol compatibility at a glance. Best-effort — a missing
/// kernel is normal on OSS installs.
fn print_kernel_status() {
    match orkia_kernel_client::discover() {
        Some(rpc) => {
            let v = rpc.version();
            println!(
                "kernel {} (protocol {}, min_client {})",
                v.kernel,
                v.protocol,
                v.min_client
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "-".into())
            );
        }
        None => println!("kernel: not running (heuristic mode)"),
    }
}

/// Fetch manifest.json + manifest.json.sig and verify the SSHSIG before
/// parsing.
async fn fetch_verified_manifest() -> Result<ReleaseManifest, String> {
    let base = release_base();
    let manifest = http_get(&format!("{base}/manifest.json")).await?;
    let sig = http_get(&format!("{base}/manifest.json.sig")).await?;
    verify_sshsig(&manifest, &sig)?;
    serde_json::from_slice(&manifest)
        .map_err(|e| format!("manifest parse failed after verification: {e}"))
}

async fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| format!("http client init: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("GET {url}: body read: {e}"))?;
    Ok(bytes.to_vec())
}

/// Verify `payload` against an SSHSIG `sig` using the embedded release
/// pubkey, via `ssh-keygen -Y verify` (preinstalled on macOS 11+ and
/// every supported Linux). Fail-closed: missing ssh-keygen, bad sig,
/// any I/O error → Err.
fn verify_sshsig(payload: &[u8], sig: &[u8]) -> Result<(), String> {
    verify_sshsig_with(payload, sig, PUBKEY_SSH)
}

fn verify_sshsig_with(payload: &[u8], sig: &[u8], pubkey: &str) -> Result<(), String> {
    let dir = tempfile::TempDir::new().map_err(|e| format!("tempdir: {e}"))?;
    let signers = dir.path().join("allowed_signers");
    let sig_path = dir.path().join("manifest.sig");
    // Trim: PUBKEY_SSH comes from include_str! and carries the file's
    // trailing newline, which would corrupt the allowed_signers line.
    std::fs::write(&signers, format!("orkia {}\n", pubkey.trim()))
        .map_err(|e| format!("write allowed_signers: {e}"))?;
    std::fs::write(&sig_path, sig).map_err(|e| format!("write sig: {e}"))?;
    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-I", "orkia", "-n", "file"])
        .arg("-f")
        .arg(&signers)
        .arg("-s")
        .arg(&sig_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ssh-keygen unavailable ({e}) — aborting (fail-closed)"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(payload)
            .map_err(|e| format!("ssh-keygen stdin: {e}"))?;
    }
    drop(child.stdin.take());
    let status = child.wait().map_err(|e| format!("ssh-keygen wait: {e}"))?;
    if !status.success() {
        return Err("manifest signature INVALID — aborting (fail-closed)".into());
    }
    Ok(())
}

/// Download the platform tarball, check its sha256 against the verified
/// manifest, extract, and atomically swap the binaries next to the
/// currently running executable.
async fn download_and_install(platform: &str, asset: &PlatformAsset) -> Result<PathBuf, String> {
    let base = release_base();
    println!("downloading orkia-{platform}.tar.gz ...");
    let bytes = http_get(&format!("{base}/orkia-{platform}.tar.gz")).await?;
    let got = orkia_kernel_trust::sha256_hex(&bytes);
    if got != asset.sha256 {
        return Err(format!(
            "sha256 mismatch (got {got}, want {}) — aborting (fail-closed)",
            asset.sha256
        ));
    }
    println!("✓ verified ({} bytes)", bytes.len());
    let dir = tempfile::TempDir::new().map_err(|e| format!("tempdir: {e}"))?;
    let tarball = dir.path().join("orkia.tar.gz");
    std::fs::write(&tarball, &bytes).map_err(|e| format!("write tarball: {e}"))?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(dir.path())
        .status()
        .map_err(|e| format!("tar: {e}"))?;
    if !status.success() {
        return Err("tar extraction failed".into());
    }
    let dest_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .ok_or_else(|| "cannot resolve current executable directory".to_string())?;
    swap_binaries(dir.path(), &dest_dir)?;
    Ok(dest_dir)
}

/// Atomic per-binary swap: stage as `.{name}.new` in the destination
/// (same filesystem → rename is atomic), back the old one up as
/// `{name}.bak`, then rename into place. A running process keeps its
/// inode, so live shells/daemons are unaffected until restart.
fn swap_binaries(src_dir: &Path, dest_dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    for name in ["orkia", "orkia-cage", "orkia-sh"] {
        let src = src_dir.join(name);
        if !src.is_file() {
            continue;
        }
        let staged = dest_dir.join(format!(".{name}.new"));
        std::fs::copy(&src, &staged).map_err(|e| format!("stage {name}: {e}"))?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {name}: {e}"))?;
        let dest = dest_dir.join(name);
        if dest.exists() {
            std::fs::rename(&dest, dest_dir.join(format!("{name}.bak")))
                .map_err(|e| format!("backup {name}: {e}"))?;
        }
        std::fs::rename(&staged, &dest).map_err(|e| format!("install {name}: {e}"))?;
        println!("✓ {name}");
    }
    Ok(())
}

/// Post-install: daemon-owned jobs keep running on the old inode.
/// Surface a count + restart hint rather than blocking — the swap
/// kills nothing.
fn print_daemon_note() {
    let jobs = crate::pty_daemon::list(&ShellConfig::load());
    let running = jobs.iter().filter(|j| j.exit_code.is_none()).count();
    if running > 0 {
        println!(
            "⚠ {running} daemon job(s) still running on the previous version — \
             they continue unaffected; restart the daemon when idle to pick up \
             the new binary."
        );
    }
}

/// `orkia update --kernel` — thin CLI wrapper over the `$kernel update`
/// builtin: same auth provider, plan gate, manifest fetch, verified
/// install and daemon re-register.
async fn run_kernel_update() -> i32 {
    use orkia_shell_types::BlockContent;
    let (_classifier, _handle, resolver, auth) = crate::repl_helpers::build_capability_wiring();
    let blocks = orkia_shell::kernel_builtins::dispatch(
        &["update".to_string()],
        Some(&auth),
        Some(&resolver),
        None,
    )
    .await;
    let mut exit = 0;
    for b in blocks {
        match b {
            BlockContent::Error(msg) => {
                eprintln!("{msg}");
                exit = 1;
            }
            BlockContent::SystemInfo(msg) | BlockContent::Text(msg) => {
                if msg.contains('✗') {
                    exit = 1;
                }
                println!("{msg}");
            }
            other => println!("{other:?}"),
        }
    }
    if exit == 0 {
        // Post-install handshake — `connect` now fail-closes on protocol
        // skew, so a successful discover proves compatibility.
        print_kernel_status();
    }
    exit
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a throwaway Ed25519 key + SSHSIG over `payload`, in `dir`.
    /// Returns (pubkey_line, sig_bytes).
    fn sign_fixture(dir: &Path, payload: &[u8]) -> (String, Vec<u8>) {
        let key = dir.join("key");
        let status = Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-C", "test", "-q", "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(status.success());
        let payload_path = dir.join("payload");
        std::fs::write(&payload_path, payload).unwrap();
        let status = Command::new("ssh-keygen")
            .args(["-Y", "sign", "-n", "file", "-f"])
            .arg(&key)
            .arg(&payload_path)
            .status()
            .unwrap();
        assert!(status.success());
        let pubkey = std::fs::read_to_string(dir.join("key.pub"))
            .unwrap()
            .trim()
            .to_string();
        let sig = std::fs::read(dir.join("payload.sig")).unwrap();
        (pubkey, sig)
    }

    #[test]
    fn sshsig_round_trip_and_tamper_rejection() {
        let dir = tempfile::TempDir::new().unwrap();
        let payload = br#"{"version":"latest","commit":"abc"}"#;
        let (pubkey, sig) = sign_fixture(dir.path(), payload);
        verify_sshsig_with(payload, &sig, &pubkey).unwrap();
        // Tampered payload must be rejected (fail-closed).
        let tampered = br#"{"version":"latest","commit":"EVIL"}"#;
        assert!(verify_sshsig_with(tampered, &sig, &pubkey).is_err());
        // Wrong key must be rejected.
        assert!(verify_sshsig_with(payload, &sig, PUBKEY_SSH).is_err());
    }

    #[test]
    fn manifest_parses_release_yml_shape() {
        let raw = br#"{"version":"latest","commit":"deadbeefcafe","platforms":{"darwin-arm64":{"sha256":"aa","size":1},"linux-x86_64":{"sha256":"bb","size":2}}}"#;
        let m: ReleaseManifest = serde_json::from_slice(raw).unwrap();
        assert_eq!(m.commit, "deadbeefcafe");
        assert_eq!(m.platforms["darwin-arm64"].sha256, "aa");
        assert_eq!(m.platforms.len(), 2);
    }
}
