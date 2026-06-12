// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! The coarse V1 rootfs bound whole `/usr/bin`, `/sbin`, … read-only — exposing
//! ~1100 parasitic binaries the agent never needs. D1 tightens the **binary
//! surface** to an allowlist while keeping the **library/runtime closure
//! coarse** (the `/usr/lib*`, `/etc`, … binds the dynamic linker + NSS + ICU +
//! TLS need stay intact, so the agent runtime never breaks — the deliberate
//!
//! The allowlist is the union of (1) a hardcoded base set (coreutils + shell a
//! dev session cannot work without, plus the tools agents commonly shell out
//! to), (2) binaries derived from the policy capability names (`git.push` ⇒
//! `git`), (3) an optional `ORKIA_CAGE_EXTRA_TOOLS` list (the shell plumbs
//! `[cage].extra_tools` here — kept off the `Policy` verdict type), and
//! (4) the agent program itself. Names that resolve to no host binary are
//! silently dropped — fail-safe: a missing tool is simply absent, never an
//! error that blocks the launch.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use orkia_shell_types::Policy;

use crate::linux_sb::ToolBind;

/// Env var the shell sets from `[cage].extra_tools` (colon- or comma-separated
/// binary names). Kept off `Policy` so the open-core verdict type stays clean.
const EXTRA_TOOLS_ENV: &str = "ORKIA_CAGE_EXTRA_TOOLS";

/// Coreutils + shell a dev session cannot function without.
const BASE_TOOLS: &[&str] = &[
    "sh",
    "bash",
    "dash",
    "env",
    "printenv",
    "ls",
    "cat",
    "cp",
    "mv",
    "rm",
    "mkdir",
    "rmdir",
    "ln",
    "chmod",
    "chown",
    "chgrp",
    "touch",
    "pwd",
    "echo",
    "printf",
    "sed",
    "grep",
    "egrep",
    "fgrep",
    "awk",
    "gawk",
    "cut",
    "tr",
    "head",
    "tail",
    "sort",
    "uniq",
    "wc",
    "find",
    "xargs",
    "which",
    "basename",
    "dirname",
    "readlink",
    "realpath",
    "stat",
    "date",
    "sleep",
    "true",
    "false",
    "test",
    "expr",
    "tee",
    "diff",
    "cmp",
    "less",
    "more",
    "tar",
    "gzip",
    "gunzip",
    "zcat",
    "bzip2",
    "xz",
    "file",
    "id",
    "whoami",
    "groups",
    "uname",
    "hostname",
    "ps",
    "kill",
    "md5sum",
    "sha256sum",
    "base64",
    "seq",
    "tac",
    "nl",
    "paste",
    "comm",
    "split",
    "du",
    "df",
    "mktemp",
    "tty",
    "stty",
];

/// Non-network dev tools agents very commonly shell out to. Bound only when
/// present on the host (so the list never breaks a launch); included by default
/// to keep the agent working without forcing every deployment to set
/// `extra_tools`. **Network-egress tools (`curl`/`wget`/`ssh`) are deliberately
/// excluded** — under a minimal allowlist they are an exfiltration vector
/// (network is not namespaced in V1), so a deployment that needs them adds them
/// explicitly via `extra_tools` or a capability.
const AGENT_COMMON: &[&str] = &["git", "rg", "node", "python3", "jq"];

/// Bin dirs searched (in order) to resolve a tool name to a host binary. The
/// canonical merged location (`/usr/bin`, `/usr/sbin`) comes first so a name
/// found there is bound at its real path; the usr-merge symlinks (`/bin` →
/// `usr/bin`) reach the same file.
const BIN_SEARCH: &[&str] = &[
    "/usr/bin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/local/sbin",
    "/bin",
    "/sbin",
];

/// The host bin-dir layout, partitioned for the rootfs builder.
pub struct Layout {
    /// Library/data dirs bound read-only **coarse** (the runtime closure).
    pub coarse_ro: Vec<PathBuf>,
    /// Real bin dirs created **empty** so only allowlisted tools populate them.
    pub bin_dirs: Vec<PathBuf>,
    /// usr-merge compat symlinks to recreate: `(link_path, target)`.
    pub symlinks: Vec<(PathBuf, PathBuf)>,
}

/// Map a capability name to the binary it implies: the segment before the first
/// `.` (so `git.push` ⇒ `git`, bare `docker` ⇒ `docker`). Pure.
fn capability_binary(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// Build the allowlisted tool-name set: base ∪ agent-common ∪ capability-derived
/// ∪ extra. Pure over its inputs (the env list is read by the caller and passed
/// in) so it is unit-testable without touching the environment.
fn tool_names(capabilities: &[String], extra: &[String]) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = BASE_TOOLS.iter().map(|s| s.to_string()).collect();
    names.extend(AGENT_COMMON.iter().map(|s| s.to_string()));
    for cap in capabilities {
        names.insert(capability_binary(cap).to_string());
    }
    names.extend(extra.iter().filter(|s| !s.is_empty()).cloned());
    names
}

/// Read `ORKIA_CAGE_EXTRA_TOOLS` into a list (colon/comma separated).
fn extra_tools() -> Vec<String> {
    std::env::var(EXTRA_TOOLS_ENV)
        .unwrap_or_default()
        .split([':', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve a tool name to `(link, real)`: the first existing `BIN_SEARCH/<name>`
/// (the invocation path) and its canonicalized target (the file actually bound).
fn resolve_tool(name: &str) -> Option<ToolBind> {
    for dir in BIN_SEARCH {
        let link = Path::new(dir).join(name);
        if link.exists() {
            let real = std::fs::canonicalize(&link).unwrap_or_else(|_| link.clone());
            return Some(ToolBind { link, real });
        }
    }
    None
}

/// Resolve the agent program to a [`ToolBind`] so its own binary is always
/// exposed. Absolute/relative paths are canonicalized; a bare name is resolved
/// via the bin search path (and `~/.local/bin`, where agents like `claude`
/// install). `None` if it cannot be found — the launch then fails closed at exec.
fn resolve_program(program: &str, home: Option<&Path>) -> Option<ToolBind> {
    if program.contains('/') {
        let link = PathBuf::from(program);
        if !link.exists() {
            return None;
        }
        let real = std::fs::canonicalize(&link).unwrap_or_else(|_| link.clone());
        return Some(ToolBind { link, real });
    }
    if let Some(tb) = resolve_tool(program) {
        return Some(tb);
    }
    let link = home?.join(".local/bin").join(program);
    link.exists().then(|| {
        let real = std::fs::canonicalize(&link).unwrap_or_else(|_| link.clone());
        ToolBind { link, real }
    })
}

/// The full allowlisted tool set for this launch, resolved to host binaries.
/// Unresolvable names are dropped; duplicates (by real path) are not — binding
/// the same file at two invocation names is intentional and harmless.
pub fn resolve_tools(program: &str, policy: &Policy, home: Option<&Path>) -> Vec<ToolBind> {
    let caps: Vec<String> = policy.capabilities.iter().map(|c| c.name.clone()).collect();
    let mut tools: Vec<ToolBind> = tool_names(&caps, &extra_tools())
        .iter()
        .filter_map(|n| resolve_tool(n))
        .collect();
    if let Some(prog) = resolve_program(program, home)
        && !tools.iter().any(|t| t.link == prog.link)
    {
        tools.push(prog);
    }
    tools
}

/// Probe the host and partition the system dirs: lib/data dirs → coarse ro
/// binds; real bin dirs → per-binary (created empty); symlinked dirs (usr-merge)
/// → recreated as symlinks. Handles both merged (`/bin` → `usr/bin`) and
/// non-merged layouts.
pub fn system_layout() -> Layout {
    let mut out = Layout {
        coarse_ro: Vec::new(),
        bin_dirs: Vec::new(),
        symlinks: Vec::new(),
    };
    // Bin dirs: real → per-binary allowlist; symlink → replicate.
    for d in [
        "/usr/bin",
        "/usr/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/bin",
        "/sbin",
    ] {
        classify(Path::new(d), &mut out.bin_dirs, &mut out.symlinks);
    }
    // Library/data dirs: real → coarse ro bind; symlink → replicate.
    for d in [
        "/usr/lib",
        "/usr/lib64",
        "/usr/lib32",
        "/usr/libx32",
        "/usr/libexec",
        "/usr/share",
        "/usr/local/lib",
        "/usr/local/share",
        "/lib",
        "/lib64",
        "/lib32",
        "/libx32",
        "/etc",
        "/opt",
    ] {
        classify(Path::new(d), &mut out.coarse_ro, &mut out.symlinks);
    }
    out
}

/// Classify one host dir: a symlink is replicated (so the merged tree resolves);
/// a real dir is added to `real_bucket` (coarse ro, or per-binary); a missing
/// path is skipped.
fn classify(path: &Path, real_bucket: &mut Vec<PathBuf>, symlinks: &mut Vec<(PathBuf, PathBuf)>) {
    let Ok(meta) = path.symlink_metadata() else {
        return;
    };
    if meta.file_type().is_symlink() {
        if let Ok(target) = std::fs::read_link(path) {
            symlinks.push((path.to_path_buf(), target));
        }
    } else if meta.is_dir() {
        real_bucket.push(path.to_path_buf());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_binary_takes_prefix_before_dot() {
        assert_eq!(capability_binary("git.push"), "git");
        assert_eq!(capability_binary("docker.run"), "docker");
        assert_eq!(capability_binary("rm"), "rm");
        assert_eq!(capability_binary("a.b.c"), "a");
    }

    #[test]
    fn tool_names_unions_all_sources() {
        let caps = vec!["git.push".to_string(), "docker.run".to_string()];
        let extra = vec!["ripgrep".to_string(), "".to_string()];
        let names = tool_names(&caps, &extra);
        // Base + agent-common always present.
        assert!(names.contains("ls"));
        assert!(names.contains("bash"));
        assert!(names.contains("git")); // also agent-common, deduped by the set
        // Capability-derived.
        assert!(names.contains("docker"));
        // Extra, with the empty entry filtered.
        assert!(names.contains("ripgrep"));
        assert!(!names.contains(""));
    }

    #[test]
    fn tool_names_dedupes() {
        // `git` arrives from both AGENT_COMMON and the capability — one entry.
        let caps = vec!["git.status".to_string()];
        let names = tool_names(&caps, &[]);
        assert_eq!(names.iter().filter(|n| *n == "git").count(), 1);
    }
}
