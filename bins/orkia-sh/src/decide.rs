// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! The shim is invoked as `orkia-sh -c "<string>"`. What that string *is*
//! depends on the agent: Claude Code does not pass the bare command — it wraps
//! it in a snapshot/`eval` envelope, so a raw match never fires. This
//! module recovers the real command to evaluate. It does **no** glob/var
//! envelope*. Everything here is platform-agnostic and unit-tested.
//!
//! Provider coverage: Codex (`bash -lc "<cmd>"`) and Gemini's
//! `run_shell_command` (`bash -c "<cmd>"`) pass the command **bare** — they do
//! not emit Claude's `source … && eval '…'` snapshot scaffolding — so the
//! bare-passthrough branch already extracts them correctly; only the clustered
//! `-lc` flag needed handling, in `main::dash_c_command`. The `eval '…'`
//! unwrap therefore stays Claude-shaped. If a future Codex/Gemini build *does*
//! wrap (real-capture owed — `qa/linux/agent-shells.md` Codex/Gemini rows), an
//! unrecognized wrapper passed through bare simply fails to match an allow rule
//! and falls to the policy default (ask→deny) — fail-closed, never a silent
//! allow.

/// The command to evaluate against the policy, or a signal to fail closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extracted {
    /// The (normalized) command string to feed to `Policy::evaluate_match`.
    Command(String),
    /// An `eval '…'` envelope was present but its payload could not be
    /// recovered. Treat as untrusted (CLAUDE.md #7) → fail closed.
    Unparseable,
}

const EVAL_MARKER: &str = "eval '";

/// Recover the command to evaluate from the raw `-c` payload.
///
/// - If the payload contains Claude's `eval '…'` envelope, return the unwrapped
///   inner command.
/// - If the envelope is present but unterminated, return [`Extracted::Unparseable`]
///   so the caller fails closed.
/// - Otherwise (bare command from a non-wrapping agent, or a direct `sh -c`),
///   return the trimmed raw string as-is.
pub fn extract_command(raw: &str) -> Extracted {
    if raw.contains(EVAL_MARKER) {
        match unwrap_eval(raw) {
            Some(inner) => Extracted::Command(inner),
            None => Extracted::Unparseable,
        }
    } else {
        Extracted::Command(raw.trim().to_string())
    }
}

/// Extract the body of the first `eval '…'` in `raw`, decoding bash
/// single-quote escaping (`'\''` → `'`). Returns `None` if the closing quote
/// is missing. Treats only the first envelope (V1 limit, documented).
fn unwrap_eval(raw: &str) -> Option<String> {
    let start = raw.find(EVAL_MARKER)? + EVAL_MARKER.len();
    let rest = &raw[start..];
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((idx, c)) = chars.next() {
        if c == '\'' {
            // Bash escapes a literal single quote inside a single-quoted string
            // as the 4-char sequence `'\''` (close, escaped-quote, reopen).
            if rest[idx..].starts_with("'\\''") {
                out.push('\'');
                chars.next(); // '\\'
                chars.next(); // '\''
                chars.next(); // '\''
                continue;
            }
            return Some(out); // the terminating quote
        }
        out.push(c);
    }
    None // ran off the end with no closing quote → unparseable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_command_passes_through_trimmed() {
        assert_eq!(
            extract_command("  git push origin  "),
            Extracted::Command("git push origin".to_string())
        );
    }

    #[test]
    fn unwraps_claude_eval_envelope() {
        // The shape from qa/linux/agent-shells.md.
        let raw = "source /tmp/snap.sh 2>/dev/null || true \
            && shopt -u extglob 2>/dev/null || true \
            && eval 'git push origin main' < /dev/null \
            && pwd -P >| /tmp/cwd";
        assert_eq!(
            extract_command(raw),
            Extracted::Command("git push origin main".to_string())
        );
    }

    #[test]
    fn decodes_escaped_single_quote_in_payload() {
        // `echo it's fine` → wrapper escapes the apostrophe as '\''
        let raw = "source x && eval 'echo it'\\''s fine' < /dev/null";
        assert_eq!(
            extract_command(raw),
            Extracted::Command("echo it's fine".to_string())
        );
    }

    #[test]
    fn unterminated_envelope_is_unparseable_fail_closed() {
        let raw = "source x && eval 'git push --no-close";
        assert_eq!(extract_command(raw), Extracted::Unparseable);
    }

    #[test]
    fn first_envelope_wins() {
        let raw = "eval 'git status' && eval 'git push'";
        assert_eq!(
            extract_command(raw),
            Extracted::Command("git status".to_string())
        );
    }

    #[test]
    fn codex_gemini_bare_command_passes_through_unwrapped() {
        // Codex (`bash -lc "git push"`) and Gemini (`bash -c "git push"`) pass
        // the command bare — no `eval` envelope — so it reaches the policy
        // matcher directly and an allow/deny rule for `git push*` fires.
        assert_eq!(
            extract_command("git push origin main"),
            Extracted::Command("git push origin main".to_string())
        );
    }
}
