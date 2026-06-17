// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The acceptance oracle (SPEC-CONVERGENCE-LOOP-V1, Phase 3).
//!
//! Runs a task's `accept` command in its workspace and reports the exit code +
//! a bounded tail of the combined output. The command is **author-declared
//! trusted input** (the RFC), so V1 runs it directly (`/bin/sh -lc`) rather than
//! through the cage — the cage bounds the untrusted *agent*, not this verdict.
//! Run OFF the actor thread (the caller spawns a thread and posts the result as
//! [`crate::run::ProxyMsg::Verdict`]): `cargo test` can take minutes and the
//! actor must stay responsive for other in-flight tasks (`max_inflight > 1`).

use std::path::Path;
use std::process::Command;

/// Last bytes of the acceptance output kept for the self-repair prompt. Bounded
/// so a noisy build log can't blow up the retry context.
const TAIL_CAP: usize = 4096;

/// The verdict of one acceptance run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptanceResult {
    /// Exit code of the `accept` command (`0` ⇒ passed). `-1` if it could not
    /// be spawned or was killed by a signal.
    pub exit_code: i32,
    /// Last [`TAIL_CAP`] bytes of stdout+stderr, for the self-repair prompt.
    pub output_tail: String,
}

impl AcceptanceResult {
    pub fn passed(&self) -> bool {
        self.exit_code == 0
    }
}

/// Run `command` via `/bin/sh -lc` in `working_dir`, capturing combined
/// stdout+stderr. Never panics: a spawn failure is reported as a failing
/// verdict (`exit_code = -1`) so the loop treats "couldn't verify" as "not
/// converged" (fail-closed).
pub(crate) fn run_acceptance(command: &str, working_dir: Option<&str>) -> AcceptanceResult {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-lc").arg(command);
    if let Some(dir) = working_dir {
        cmd.current_dir(Path::new(dir));
    }
    match cmd.output() {
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
            AcceptanceResult {
                exit_code: out.status.code().unwrap_or(-1),
                output_tail: tail_of(&combined, TAIL_CAP),
            }
        }
        Err(e) => AcceptanceResult {
            exit_code: -1,
            output_tail: format!("acceptance command failed to spawn: {e}"),
        },
    }
}

/// Keep the last `cap` bytes of `s`, snapped to a char boundary.
fn tail_of(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut start = s.len() - cap;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_zero_passes() {
        let r = run_acceptance("exit 0", None);
        assert_eq!(r.exit_code, 0);
        assert!(r.passed());
    }

    #[test]
    fn nonzero_fails_and_captures_tail() {
        let r = run_acceptance("echo boom 1>&2; exit 7", None);
        assert_eq!(r.exit_code, 7);
        assert!(!r.passed());
        assert!(r.output_tail.contains("boom"), "tail: {:?}", r.output_tail);
    }

    #[test]
    fn runs_in_working_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "x").unwrap();
        let r = run_acceptance("test -f marker", dir.path().to_str());
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn tail_is_bounded_and_char_safe() {
        let big = "é".repeat(5000); // > TAIL_CAP bytes, multibyte
        let t = tail_of(&big, TAIL_CAP);
        assert!(t.len() <= TAIL_CAP);
        assert!(t.starts_with('é')); // snapped to a boundary, not mid-codepoint
    }
}
