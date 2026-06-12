// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Daemon registration: writes a LaunchAgent plist (macOS) or a
//! systemd user unit (Linux), then bootstraps it.
//!
//! The shell calls [`register`] after a successful install and
//! [`unregister`] on `$kernel --uninstall` / `$logout`. Both are
//! idempotent — running them twice is fine.

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::InstallLayout;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("launchctl/systemctl failed: {0}")]
    Bootstrap(String),
    #[error("unsupported platform")]
    Unsupported,
}

/// Service identifier used across plist / unit names. Lowercase
/// reverse-DNS to match macOS conventions.
pub const SERVICE_LABEL: &str = "dev.orkia.kernel";

/// Where the user-level service file lives on the current platform.
pub fn service_file_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    #[cfg(target_os = "macos")]
    {
        Some(
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{SERVICE_LABEL}.plist")),
        )
    }
    #[cfg(target_os = "linux")]
    {
        Some(
            home.join(".config")
                .join("systemd")
                .join("user")
                .join("orkia-kernel.service"),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = home;
        None
    }
}

/// Render the platform-appropriate service definition into a string.
/// Exposed for `$kernel logs` / manual inspection.
pub fn render_unit(layout: &InstallLayout) -> Result<String, DaemonError> {
    let exec = layout.binary_path();
    let logs = layout.logs_dir();
    // Co-installed shared libraries (ggml / llama) live next to the
    // binary; the kernel loads them via `@rpath` / soname with no
    // embedded rpath, so the loader must be told to search the bin dir.
    let bin_dir = layout.bin_dir();
    #[cfg(target_os = "macos")]
    {
        Ok(render_launchd_plist(&exec, &logs, &bin_dir))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(render_systemd_unit(&exec, &logs, &bin_dir))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (exec, logs, bin_dir);
        Err(DaemonError::Unsupported)
    }
}

/// Idempotent: write the unit, bootstrap it, start it. If the unit
/// already exists, bounce it so a freshly installed binary picks up.
pub fn register(layout: &InstallLayout) -> Result<(), DaemonError> {
    let path = service_file_path().ok_or(DaemonError::Unsupported)?;
    let unit = render_unit(layout)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, unit)?;
    bootstrap(&path)
}

/// Idempotent: stop the service and remove the unit file. Leaves the
/// binary and logs in place — `$kernel --uninstall` handles those.
pub fn unregister() -> Result<(), DaemonError> {
    let Some(path) = service_file_path() else {
        return Err(DaemonError::Unsupported);
    };
    teardown(&path)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DaemonError::Io(e)),
    }
}

#[cfg(target_os = "macos")]
fn bootstrap(path: &Path) -> Result<(), DaemonError> {
    // SAFETY: `libc::getuid` is FFI-safe, takes no arguments, and
    // returns the calling process UID — never fails.
    let uid = unsafe { libc::getuid() };
    // Unload first (ignore errors — first run won't have it loaded).
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}")])
        .arg(path)
        .status();
    let status = Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}")])
        .arg(path)
        .status()
        .map_err(|e| DaemonError::Bootstrap(e.to_string()))?;
    if !status.success() {
        return Err(DaemonError::Bootstrap(format!(
            "launchctl bootstrap exit {status}"
        )));
    }
    let _ = Command::new("launchctl")
        .args(["kickstart", "-k", &format!("gui/{uid}/{SERVICE_LABEL}")])
        .status();
    Ok(())
}

#[cfg(target_os = "macos")]
fn teardown(path: &Path) -> Result<(), DaemonError> {
    // SAFETY: `libc::getuid` is FFI-safe, takes no arguments, and
    // returns the calling process UID — never fails.
    let uid = unsafe { libc::getuid() };
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}")])
        .arg(path)
        .status();
    Ok(())
}

#[cfg(target_os = "linux")]
fn bootstrap(_path: &Path) -> Result<(), DaemonError> {
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let status = Command::new("systemctl")
        .args(["--user", "enable", "--now", "orkia-kernel.service"])
        .status()
        .map_err(|e| DaemonError::Bootstrap(e.to_string()))?;
    if !status.success() {
        return Err(DaemonError::Bootstrap(format!("systemctl exit {status}")));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn teardown(_path: &Path) -> Result<(), DaemonError> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "orkia-kernel.service"])
        .status();
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn bootstrap(_path: &Path) -> Result<(), DaemonError> {
    Err(DaemonError::Unsupported)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn teardown(_path: &Path) -> Result<(), DaemonError> {
    Err(DaemonError::Unsupported)
}

#[cfg(target_os = "macos")]
fn render_launchd_plist(exec: &Path, logs: &Path, bin_dir: &Path) -> String {
    let exec = exec.display();
    let stdout = logs.join("stdout.log");
    let stderr = logs.join("stderr.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVICE_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exec}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>DYLD_FALLBACK_LIBRARY_PATH</key>
        <string>{bin_dir}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
</dict>
</plist>
"#,
        exec = exec,
        bin_dir = bin_dir.display(),
        stdout = stdout.display(),
        stderr = stderr.display(),
    )
}

#[cfg(target_os = "linux")]
fn render_systemd_unit(exec: &Path, logs: &Path, bin_dir: &Path) -> String {
    let stdout = logs.join("stdout.log");
    format!(
        r#"[Unit]
Description=Orkia kernel daemon (local LLM inference + routing)
After=network.target

[Service]
Type=simple
ExecStart={exec}
Environment=LD_LIBRARY_PATH={bin_dir}
Restart=on-failure
RestartSec=5
StandardOutput=append:{stdout}
StandardError=inherit

[Install]
WantedBy=default.target
"#,
        exec = exec.display(),
        bin_dir = bin_dir.display(),
        stdout = stdout.display(),
    )
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn render_unit_mentions_binary_path() {
        let tmp = TempDir::new().unwrap();
        let layout = InstallLayout {
            root: tmp.path().to_path_buf(),
        };
        let unit = render_unit(&layout).unwrap();
        let bin = layout.binary_path();
        assert!(unit.contains(&bin.display().to_string()));
        assert!(unit.contains(SERVICE_LABEL) || unit.contains("orkia-kernel"));
        // The loader must be pointed at the bin dir for the co-installed
        // ggml / llama shared libraries (no embedded rpath).
        assert!(unit.contains(&layout.bin_dir().display().to_string()));
        #[cfg(target_os = "macos")]
        assert!(unit.contains("DYLD_FALLBACK_LIBRARY_PATH"));
        #[cfg(target_os = "linux")]
        assert!(unit.contains("LD_LIBRARY_PATH"));
    }
}
