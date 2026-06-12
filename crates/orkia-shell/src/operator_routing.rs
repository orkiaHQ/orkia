// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! The classifier path is operator-blind: `|`, `&&`, `||`, `;` tokenize
//! as ordinary arguments, so `ps | grep x` fed the pipe to `PsFlags`
//! and `login | grep x` ran login and silently dropped the pipe. This
//! module is the grammar-level fix, consulted from `Repl::tick` once a
//! line has resolved to `Mode::Builtin`, before `parse_builtin`:
//!
//!   is plain POSIX; brush runs it byte-for-byte.
//! - Orkia-native head, or an explicit `orkia `/`/` namespace claim →
//!   loud refusal, nothing executes. (Brush would exec a nonexistent
//!   binary — a confusing 127 — or spawn a child orkia CLI.)
//!
//! Typed-path lines (`orkia jobs | grep x`) never reach this check —
//! `try_parse_exec` captures them earlier with `external_suffix`.

/// First unquoted shell operator in the line: `|`, `||`, `&&`, or `;`.
/// Quote-aware in the same way as `shell_agent_pipe::has_unquoted_pipe`:
/// single/double quotes, backslash escapes, `$(...)` and backticks all
/// pause the scan. A `>|` (noclobber override redirect) is not a pipe.
pub fn find_unquoted_operator(line: &str) -> Option<&'static str> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut paren_depth: u32 = 0;
    let mut backtick = false;

    while i < len {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            match c {
                b'"' => in_double = false,
                // POSIX: backslash in double quotes only escapes `$` ` " \
                // and newline; otherwise it is literal (BUG-N09).
                b'\\'
                    if i + 1 < len
                        && matches!(bytes[i + 1], b'"' | b'$' | b'`' | b'\\' | b'\n') =>
                {
                    i += 2;
                    continue;
                }
                _ => {}
            }
            i += 1;
            continue;
        }
        if paren_depth > 0 {
            match c {
                b'(' => paren_depth += 1,
                b')' => paren_depth -= 1,
                _ => {}
            }
            i += 1;
            continue;
        }
        if backtick {
            if c == b'`' {
                backtick = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'`' => backtick = true,
            b'\\' if i + 1 < len => {
                i += 2;
                continue;
            }
            b'$' if i + 1 < len && bytes[i + 1] == b'(' => {
                paren_depth += 1;
                i += 2;
                continue;
            }
            b'|' if i == 0 || bytes[i - 1] != b'>' => {
                return Some(if i + 1 < len && bytes[i + 1] == b'|' {
                    "||"
                } else {
                    "|"
                });
            }
            b'&' if i + 1 < len && bytes[i + 1] == b'&' => return Some("&&"),
            b';' => return Some(";"),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Routing verdict for a `Mode::Builtin` line carrying a shell operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorRoute {
    /// Bare collidable head: the whole line is plain POSIX — brush runs it.
    Brush,
    /// Orkia-native head (or an explicit namespace claim): refuse loudly,
    /// execute nothing.
    Refuse { head: String },
}

/// when the line has no unquoted operator (dispatch normally) or its
/// head is in no table row (only reachable namespaced — the
/// unknown-builtin error path handles it in-process).
pub fn route_builtin_operator(line: &str) -> Option<OperatorRoute> {
    find_unquoted_operator(line)?;
    let mut rest = line.trim();
    let mut namespaced = false;
    // Strip optional `orkia` / `/` prefixes recursively (matches
    // `parse_builtin`). The prefix is an explicit builtin claim: a
    // collidable head loses its brush fallback once namespaced —
    // rerouting `orkia login | grep x` to brush would exec a child
    // orkia CLI, the exact failure Invariant 5 kills.
    loop {
        if let Some(r) = rest.strip_prefix("orkia ") {
            rest = r.trim();
            namespaced = true;
            continue;
        }
        if let Some(r) = rest.strip_prefix('/') {
            rest = r.trim();
            namespaced = true;
            continue;
        }
        break;
    }
    let head = rest.split_whitespace().next().unwrap_or("");
    let spec = crate::builtin_table::spec_for(head)?;
    if spec.collision.is_some() && !namespaced {
        Some(OperatorRoute::Brush)
    } else {
        Some(OperatorRoute::Refuse {
            head: head.to_string(),
        })
    }
}

/// hint applies to typed builtins via `external_suffix`; others need
/// separate lines.)
pub fn refusal_message(head: &str) -> String {
    format!(
        "{head}: builtin does not support shell operators; use 'orkia {head} | …' for pipes or separate lines"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── find_unquoted_operator ──────────────────────────────────

    #[test]
    fn detects_each_operator() {
        assert_eq!(find_unquoted_operator("ps | grep x"), Some("|"));
        assert_eq!(find_unquoted_operator("setup && echo done"), Some("&&"));
        assert_eq!(find_unquoted_operator("false || echo no"), Some("||"));
        assert_eq!(find_unquoted_operator("kill 1; echo done"), Some(";"));
    }

    #[test]
    fn no_operator_in_plain_lines() {
        assert_eq!(find_unquoted_operator("ps"), None);
        assert_eq!(find_unquoted_operator("tell @a fix the build"), None);
        assert_eq!(find_unquoted_operator(""), None);
    }

    #[test]
    fn quoted_operators_are_not_operators() {
        assert_eq!(find_unquoted_operator(r#"tell @a "x | y""#), None);
        assert_eq!(find_unquoted_operator("tell @a 'a && b; c'"), None);
        assert_eq!(find_unquoted_operator("echo $(cat a | grep b)"), None);
        assert_eq!(find_unquoted_operator("echo `true && false`"), None);
    }

    #[test]
    fn single_ampersand_and_redirects_are_not_operators() {
        // `&` alone is background (stripped by parse_background upstream),
        // `2>&1` is a redirect, `>|` is the noclobber override.
        assert_eq!(find_unquoted_operator("audit 2>&1"), None);
        assert_eq!(find_unquoted_operator("audit >| out.txt"), None);
    }

    // ── route_builtin_operator ──────────────────────────────────

    #[test]
    fn bare_collidable_head_routes_to_brush() {
        assert_eq!(
            route_builtin_operator("ps | grep x"),
            Some(OperatorRoute::Brush)
        );
        assert_eq!(
            route_builtin_operator("login | grep x"),
            Some(OperatorRoute::Brush)
        );
        assert_eq!(
            route_builtin_operator("kill 123; echo done"),
            Some(OperatorRoute::Brush)
        );
    }

    #[test]
    fn native_head_is_refused() {
        assert_eq!(
            route_builtin_operator("tell @a hi | grep x"),
            Some(OperatorRoute::Refuse {
                head: "tell".into()
            })
        );
        assert_eq!(
            route_builtin_operator("setup && echo done"),
            Some(OperatorRoute::Refuse {
                head: "setup".into()
            })
        );
    }

    #[test]
    fn namespaced_collidable_head_is_refused_not_rerouted() {
        // `orkia ` claims the builtin explicitly — brush would exec a
        // child orkia CLI, so the refusal applies even to collidables.
        assert_eq!(
            route_builtin_operator("orkia login | grep x"),
            Some(OperatorRoute::Refuse {
                head: "login".into()
            })
        );
    }

    #[test]
    fn no_operator_means_no_route() {
        assert_eq!(route_builtin_operator("ps"), None);
        assert_eq!(route_builtin_operator(r#"tell @a "x | y""#), None);
    }

    #[test]
    fn unknown_head_falls_through_to_unknown_builtin_path() {
        // Only reachable namespaced (`orkia nosuchcmd | grep x`) — the
        // dispatch catch-all owns the error, not the operator refusal.
        assert_eq!(route_builtin_operator("orkia nosuchcmd | grep x"), None);
    }
}
