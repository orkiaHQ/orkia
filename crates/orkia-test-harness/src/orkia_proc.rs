// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Locate and spawn the compiled `orkia` shell binary inside a PTY.
//!
//! Binary discovery is intentionally lazy and forgiving so the harness
//! works in three common situations:
//!
//!   1. CI sets `ORKIA_TEST_BIN=/abs/path/to/orkia` explicitly.
//!   2. Running `cargo test -p orkia-test-harness` inside the workspace
//!      — we look for `target/{debug,release}/orkia` relative to the
//!      crate manifest dir.
//!   3. The harness is consumed by an external crate that has
//!      `orkia` on `$PATH`.
//!
//! If none of those find a binary, `OrkiaBinary::resolve` can optionally
//! shell out to `cargo build -p orkia` (off by default to keep test
//! runs fast and predictable).
//!
//! Once located, `OrkiaProcess::spawn` wraps the binary in a PTY via
//! `PtyDriver` and exposes the standard handles.

use std::path::{Path, PathBuf};

use portable_pty::CommandBuilder;

use crate::pty::{PtyDriver, PtyShape};
use crate::sandbox::OrkiaSandbox;

#[derive(Clone, Debug)]
pub struct OrkiaBinary(pub PathBuf);

impl OrkiaBinary {
    /// Resolve `orkia` using env var → workspace target dir → `$PATH`.
    /// Does NOT auto-build; pass `auto_build = true` to fall back to
    /// `cargo build -p orkia`.
    pub fn resolve(auto_build: bool) -> anyhow::Result<Self> {
        if let Some(p) = std::env::var_os("ORKIA_TEST_BIN") {
            let pb = PathBuf::from(p);
            if pb.is_file() {
                return Ok(Self(pb));
            }
        }
        if let Some(p) = workspace_target_lookup("orkia") {
            return Ok(Self(p));
        }
        if let Some(p) = which("orkia") {
            return Ok(Self(p));
        }
        if auto_build {
            cargo_build("orkia")?;
            if let Some(p) = workspace_target_lookup("orkia") {
                return Ok(Self(p));
            }
        }
        anyhow::bail!(
            "could not locate `orkia` binary. Set ORKIA_TEST_BIN, build the workspace, or call resolve(true)"
        )
    }

    /// Same lookup logic for the fake-agent binary.
    pub fn resolve_fake_agent(auto_build: bool) -> anyhow::Result<PathBuf> {
        if let Some(p) = std::env::var_os("ORKIA_TEST_FAKE_AGENT_BIN") {
            let pb = PathBuf::from(p);
            if pb.is_file() {
                return Ok(pb);
            }
        }
        if let Some(p) = workspace_target_lookup("orkia-fake-agent") {
            return Ok(p);
        }
        if let Some(p) = which("orkia-fake-agent") {
            return Ok(p);
        }
        if auto_build {
            cargo_build("orkia-fake-agent")?;
            if let Some(p) = workspace_target_lookup("orkia-fake-agent") {
                return Ok(p);
            }
        }
        anyhow::bail!("could not locate `orkia-fake-agent` binary")
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Resolve both harness binaries and skip the calling test (print a
/// notice and return `None`) if either is missing.
///
/// Lets `cargo test --workspace` stay green on a fresh checkout — the
/// `e2e-real-agent` CI job sets both env vars, so the tests fully run
/// there. Local runs need `cargo build --bin orkia --bin orkia-fake-agent`
/// (or `ORKIA_TEST_BIN` + `ORKIA_TEST_FAKE_AGENT_BIN` exported) to
/// exercise the assertions.
pub fn resolve_or_skip(test_name: &str) -> Option<(OrkiaBinary, PathBuf)> {
    let orkia = match OrkiaBinary::resolve(false) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "[skip] {test_name}: {e}\n  hint: cargo build --bin orkia --bin orkia-fake-agent"
            );
            return None;
        }
    };
    let fake = match OrkiaBinary::resolve_fake_agent(false) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "[skip] {test_name}: {e}\n  hint: cargo build --bin orkia --bin orkia-fake-agent"
            );
            return None;
        }
    };
    Some((orkia, fake))
}

/// A running `orkia` shell process. Wraps a `PtyDriver` and applies
/// the env vars the shell needs to find the sandbox (`HOME`).
pub struct OrkiaProcess {
    pub pty: PtyDriver,
}

impl OrkiaProcess {
    /// Spawn `orkia` with the sandbox as `HOME`. Additional `args` are
    /// appended verbatim. Additional `env` overrides are applied last
    /// (they win over harness defaults).
    pub fn spawn(
        binary: &OrkiaBinary,
        sandbox: &OrkiaSandbox,
        args: &[&str],
        env: &[(&str, &str)],
        shape: PtyShape,
    ) -> anyhow::Result<Self> {
        let mut cmd = CommandBuilder::new(binary.path());
        for a in args {
            cmd.arg(a);
        }
        cmd.env("HOME", sandbox.home());
        // Force a known TERM so the shell's renderer makes consistent
        // choices across CI hosts.
        cmd.env("TERM", "xterm-256color");
        // Disable any user-level rc-file loading the shell might do
        // outside the sandbox.
        cmd.env("ORKIA_NO_USER_RC", "1");
        // Run from the sandbox so relative paths don't leak the
        // developer's cwd.
        cmd.cwd(sandbox.home());
        for (k, v) in env {
            cmd.env(*k, *v);
        }
        let pty = PtyDriver::spawn(cmd, shape)?;
        Ok(Self { pty })
    }
}

fn workspace_target_lookup(name: &str) -> Option<PathBuf> {
    // Look upward from the test-harness manifest. `target/` lives next
    // to the workspace root Cargo.toml — three levels up from us
    // (`crates/orkia-test-harness/Cargo.toml`).
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cargo_target_dir = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
    [
        manifest.join("../../target/debug").join(name),
        manifest.join("../../target/release").join(name),
        // Workspaces sometimes set CARGO_TARGET_DIR.
        cargo_target_dir
            .as_ref()
            .map(|d| d.join("debug").join(name))
            .unwrap_or_default(),
        cargo_target_dir
            .as_ref()
            .map(|d| d.join("release").join(name))
            .unwrap_or_default(),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn cargo_build(pkg: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("cargo")
        .args(["build", "-p", pkg])
        .status()?;
    if !status.success() {
        anyhow::bail!("cargo build -p {pkg} failed: {status}");
    }
    Ok(())
}
