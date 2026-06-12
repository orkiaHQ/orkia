// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Generates a **deny-default** profile string passed to `sandbox-exec -p`.
//! The read stance is inverted from the reference (`macos-sandbox-utils.ts`):
//! deny-default, then allow-list the workspace + per-agent corpus — never a
//! blanket `(allow file-read*)`. Writes are allow-listed on the workspace, then
//! the write-deny set (`file-write*` subpath) is layered **last** so it wins
//! (Seatbelt: later rule overrides earlier).
//!
//! Pure string building — no syscalls — so it unit-tests on any platform.

use std::path::{Path, PathBuf};

/// Everything the profile generator needs. Built by the cage from the `Policy`
/// + the per-agent corpus + the invoking environment.
#[derive(Debug, Clone)]
pub struct ProfileSpec {
    /// Correlation tag stamped on every deny rule (`with message`). Decoded
    /// from the kernel deny-log to attribute a violation to a job/policy
    pub log_tag: String,
    /// Absolute workspace root.
    pub workspace: PathBuf,
    /// omitted, so the workspace falls under deny-default — the agent cannot see
    /// it at all. When false, `workspace_write` is moot (no write without read).
    pub workspace_read: bool,
    /// `caps.write`: when false the workspace write-allow is omitted, so the
    /// workspace is read-only (writes hit deny-default). Only meaningful when
    /// `workspace_read` is true.
    pub workspace_write: bool,
    /// Extra read-only allows (per-agent corpus read-only, resolver files).
    pub read_only: Vec<PathBuf>,
    /// Extra read+write allows (per-agent corpus state dirs).
    pub read_write: Vec<PathBuf>,
    /// Protected paths, write-denied via `(deny file-write* (subpath …))` —
    /// covers create/unlink/rename of the path and below. Mandatory set within
    /// the workspace, plus sensitive home dirs (tagged for audit; already
    /// covered by deny-default).
    pub deny_write: Vec<PathBuf>,
    /// Absolute binary paths denied `process-exec` (e.g. `docker`).
    pub deny_exec: Vec<PathBuf>,
}

/// Escape a path for an SBPL string literal — C/JSON-style: wrap in quotes and
/// space or quote silently corrupts the whole profile.
pub fn escape_sbpl(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The mandatory write-deny set inside an otherwise-writable workspace —
/// `macGetMandatoryDenyPatterns`). Hard-denied even though the workspace is rw.
pub fn mandatory_workspace_denies(workspace: &Path) -> Vec<PathBuf> {
    [".git/hooks", ".git/config", ".env", ".mcp.json"]
        .iter()
        .map(|rel| workspace.join(rel))
        .collect()
}

fn line(out: &mut Vec<String>, s: impl Into<String>) {
    out.push(s.into());
}

/// Build the full SBPL profile string. Sections (header/preamble → reads →
/// writes+denies) are emitted by helpers to stay within the file's per-fn limit.
pub fn build_profile(spec: &ProfileSpec) -> String {
    let with_msg = format!("(with message {})", escape_sbpl(&spec.log_tag));
    let mut p: Vec<String> = Vec::new();
    emit_preamble(&mut p, spec, &with_msg);
    emit_read_rules(&mut p, spec);
    emit_write_rules(&mut p, spec, &with_msg);
    p.join("\n")
}

/// Deny-default header (the stance we KEEP from srt) + the static "what a real
/// macOS program needs to not crash" preamble + device/PTY access.
fn emit_preamble(p: &mut Vec<String>, spec: &ProfileSpec, with_msg: &str) {
    line(p, "(version 1)");
    line(p, format!("(deny default {with_msg})"));
    line(p, format!("; orkia-cage SBPL — tag {}", spec.log_tag));
    for rule in [
        "(allow process-fork)",
        "(allow process-exec)", // broad; deny_exec re-denies specific binaries
        "(allow process-info* (target same-sandbox))",
        "(allow signal (target same-sandbox))",
        "(allow mach-priv-task-port (target same-sandbox))",
        "(allow sysctl-read)",
        "(allow ipc-posix-shm)",
        "(allow ipc-posix-sem)",
        "(allow user-preference-read)",
        // Network is NOT scoped in V1 (claim ceiling: "network is not
        // prevented") — the agent must reach its LLM API. deny-default would
        // otherwise silently block it, so allow it explicitly.
        "(allow network*)",
        "(allow system-socket)",
    ] {
        line(p, rule);
    }
    // Curated mach-lookup global-name allowlist (no wildcard — deliberate).
    line(p, "(allow mach-lookup");
    for name in [
        "com.apple.system.logger",
        "com.apple.system.notification_center",
        "com.apple.logd",
        "com.apple.lsd.mapdb",
        "com.apple.SecurityServer",
        "com.apple.CoreServices.coreservicesd",
        "com.apple.dnssd.service",
        "com.apple.coreservices.launchservicesd",
    ] {
        line(p, format!("  (global-name {})", escape_sbpl(name)));
    }
    line(p, "  )");
    // Char devices + the controlling PTY / inherited fds: the agent's stdio is
    // the PTY orkia allocated (`/dev/ttysN`) — it must be writable or no output
    // renders. Scoped to safe nodes — NOT raw disks.
    for dev in [
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
    ] {
        line(
            p,
            format!(
                "(allow file-read* file-write-data file-ioctl (literal {}))",
                escape_sbpl(dev)
            ),
        );
    }
    line(
        p,
        "(allow file-read* file-write-data file-ioctl (regex #\"^/dev/ttys[0-9]+$\"))",
    );
    line(
        p,
        "(allow file-read* file-write-data file-ioctl (subpath \"/dev/fd\"))",
    );
}

/// World-readable runtime/library roots an allow-listed binary needs so `dyld`
/// can load dylibs that are NOT in the shared cache (Homebrew/CLT tools). The
/// agent itself (node → only `/System` + `/usr/lib/libSystem`, all shared-cache)
/// boots without these, but `git`, `rg`, compilers, etc. link `/opt/homebrew`
/// or `/Library/Developer` libs and would `dyld: blocked by sandbox` otherwise.
/// These are package/runtime dirs only — user-data roots (`$HOME`, `~/.ssh`,
/// `/Library/Keychains`, the `/Library` root) stay deny-default, so the
/// confidentiality target is unchanged. `/var/db/xcode_select_link` lets the
/// `/usr/bin/git` xcode-select shim resolve its developer dir.
const RUNTIME_READ_ROOTS: &[&str] = &[
    "/usr/lib",
    "/usr/share",
    "/Library/Developer",
    "/opt/homebrew",
    "/usr/local",
];

/// Reads — deny-default already set; allow-list (the FLIP: no blanket read).
/// Keeps the two non-obvious fixes: re-allow `/` (dyld) + dir/symlink metadata
/// (path traversal / `realpath` of the /tmp→/private/tmp firmlink). Content
/// reads stay deny-default; metadata is dirs+symlinks only, not regular files.
fn emit_read_rules(p: &mut Vec<String>, spec: &ProfileSpec) {
    line(p, "(allow file-read* (literal \"/\"))");
    line(p, "(allow file-read-metadata (vnode-type DIRECTORY))");
    line(p, "(allow file-read-metadata (vnode-type SYMLINK))");
    for root in RUNTIME_READ_ROOTS {
        line(
            p,
            format!("(allow file-read* (subpath {}))", escape_sbpl(root)),
        );
    }
    line(
        p,
        "(allow file-read* (literal \"/var/db/xcode_select_link\"))",
    );
    // read is off the workspace is omitted here and stays under deny-default —
    // invisible to the agent. The corpus read-only allows are unconditional.
    let workspace = spec.workspace_read.then_some(&spec.workspace);
    for path in workspace.into_iter().chain(spec.read_only.iter()) {
        line(
            p,
            format!(
                "(allow file-read* (subpath {}))",
                escape_sbpl(&path.to_string_lossy())
            ),
        );
    }
}

/// Workspace + corpus writes, then the write-deny set and exec denies emitted
/// LAST so they layer on top (Seatbelt: later rule wins). A `file-write*`
/// subpath deny covers create/unlink/rename of the path and below; Seatbelt
/// matches the symlink-resolved path, so rm+recreate or ancestor-symlink swaps
/// don't bypass it. External paths (e.g. ~/.ssh) are already write-denied by
/// deny-default — listing them adds the audit tag. NB: do NOT emit
/// `(deny file-write-create (literal <dir>))` on ancestors — on macOS that
/// wrongly denies unrelated workspace creates (verified 2026-06-04).
fn emit_write_rules(p: &mut Vec<String>, spec: &ProfileSpec, with_msg: &str) {
    // write is off the allow is omitted, so writes fall under deny-default —
    // the workspace is read-only (still readable iff `caps.read`). Requires read
    // to be meaningful; emit only when both hold.
    if spec.workspace_read && spec.workspace_write {
        line(
            p,
            format!(
                "(allow file-write* (subpath {}))",
                escape_sbpl(&spec.workspace.to_string_lossy())
            ),
        );
    }
    for rw in &spec.read_write {
        let e = escape_sbpl(&rw.to_string_lossy());
        line(p, format!("(allow file-read* (subpath {e}))"));
        line(p, format!("(allow file-write* (subpath {e}))"));
    }
    for path in &spec.deny_write {
        line(
            p,
            format!(
                "(deny file-write* (subpath {}) {with_msg})",
                escape_sbpl(&path.to_string_lossy())
            ),
        );
    }
    for bin in &spec.deny_exec {
        line(
            p,
            format!(
                "(deny process-exec* (literal {}) {with_msg})",
                escape_sbpl(&bin.to_string_lossy())
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ProfileSpec {
        ProfileSpec {
            log_tag: "job-7".into(),
            workspace: PathBuf::from("/work/repo"),
            workspace_read: true,
            workspace_write: true,
            read_only: vec![PathBuf::from("/private/etc/hosts")],
            read_write: vec![PathBuf::from("/home/u/.claude")],
            deny_write: mandatory_workspace_denies(Path::new("/work/repo")),
            deny_exec: vec![PathBuf::from("/usr/bin/docker")],
        }
    }

    #[test]
    fn escapes_spaces_and_quotes() {
        assert_eq!(escape_sbpl("/a b"), "\"/a b\"");
        assert_eq!(escape_sbpl("a\"b"), "\"a\\\"b\"");
        assert_eq!(escape_sbpl("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn header_is_deny_default_not_permissive_read() {
        let prof = build_profile(&spec());
        assert!(prof.starts_with("(version 1)\n(deny default"));
        // The whole point: NO blanket allow-all-reads.
        assert!(!prof.contains("(allow file-read*)\n"));
        assert!(!prof.contains("(allow file-read* )"));
    }

    #[test]
    fn keeps_the_two_read_fixes() {
        let prof = build_profile(&spec());
        assert!(prof.contains("(allow file-read* (literal \"/\"))"));
        assert!(prof.contains("(allow file-read-metadata (vnode-type DIRECTORY))"));
    }

    #[test]
    fn runtime_read_roots_present_but_not_library_root() {
        let prof = build_profile(&spec());
        // Homebrew + CLT dylib roots are readable so allow-listed binaries load.
        assert!(prof.contains("(allow file-read* (subpath \"/opt/homebrew\"))"));
        assert!(prof.contains("(allow file-read* (subpath \"/Library/Developer\"))"));
        assert!(prof.contains("(allow file-read* (literal \"/var/db/xcode_select_link\"))"));
        // …but the /Library root (System.keychain) is NOT blanket-readable.
        assert!(!prof.contains("(allow file-read* (subpath \"/Library\"))"));
    }

    #[test]
    fn allows_workspace_read_and_write() {
        let prof = build_profile(&spec());
        assert!(prof.contains("(allow file-read* (subpath \"/work/repo\"))"));
        assert!(prof.contains("(allow file-write* (subpath \"/work/repo\"))"));
    }

    #[test]
    fn write_off_makes_workspace_read_only() {
        // caps.write=false: workspace stays readable, write-allow omitted ⇒ EROFS.
        let prof = build_profile(&ProfileSpec {
            workspace_write: false,
            ..spec()
        });
        assert!(prof.contains("(allow file-read* (subpath \"/work/repo\"))"));
        assert!(!prof.contains("(allow file-write* (subpath \"/work/repo\"))"));
    }

    #[test]
    fn read_off_omits_workspace_entirely() {
        // caps.read=false: neither read nor write allow for the workspace ⇒ it
        // falls under deny-default (invisible). write is moot.
        let prof = build_profile(&ProfileSpec {
            workspace_read: false,
            workspace_write: true,
            ..spec()
        });
        assert!(!prof.contains("(allow file-read* (subpath \"/work/repo\"))"));
        assert!(!prof.contains("(allow file-write* (subpath \"/work/repo\"))"));
    }

    #[test]
    fn mandatory_denies_are_present_and_tagged() {
        let prof = build_profile(&spec());
        assert!(prof.contains(
            "(deny file-write* (subpath \"/work/repo/.git/config\") (with message \"job-7\"))"
        ));
        assert!(
            prof.contains(
                "(deny file-write* (subpath \"/work/repo/.env\") (with message \"job-7\"))"
            )
        );
    }

    #[test]
    fn write_deny_uses_subpath_not_ancestor_literals() {
        let prof = build_profile(&spec());
        // The deny is a single subpath rule (covers create/unlink/rename + below)…
        assert!(prof.contains(
            "(deny file-write* (subpath \"/work/repo/.git/config\") (with message \"job-7\"))"
        ));
        // …and must NOT emit ancestor-literal create/unlink denies, which on
        // macOS wrongly block unrelated workspace creates (verified 2026-06-04).
        assert!(!prof.contains("file-write-create (literal"));
        assert!(!prof.contains("file-write-unlink (literal"));
    }

    #[test]
    fn deny_exec_emitted() {
        let prof = build_profile(&spec());
        assert!(prof.contains(
            "(deny process-exec* (literal \"/usr/bin/docker\") (with message \"job-7\"))"
        ));
    }
}
