// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! A bare line whose head collides with a system binary (`ps`, `audit`,
//! `whoami`, `route`, `log`) is routed by *shape*: if every argument
//! token matches the builtin's declared grammar the builtin runs,
//! otherwise the whole line yields to brush so the system binary gets
//! it untouched. Grammar lives in [`crate::builtin_table`]; this module
//! is the tick-level seam. Namespaced lines (`orkia <cmd>`, `/<cmd>`)
//! never consult shape — the prefix is an explicit builtin claim.

use crate::builtin_table::{ShapeRoute, route_for};

/// True when `line` is a bare collidable head whose argument shape
/// falls outside the builtin grammar — i.e. the line must be handed
/// to brush verbatim. False for namespaced lines, non-collidable
/// heads, and shapes the builtin grammar accepts.
pub fn bare_shape_yields_to_brush(line: &str) -> bool {
    let rest = line.trim();
    // `orkia <cmd>` and `/<cmd>` are explicit namespace claims: the
    // builtin always gets the line, whatever the argument shape.
    if rest == "orkia" || rest.starts_with("orkia ") || rest.starts_with('/') {
        return false;
    }
    let tokens = crate::repl::tokenize_args(rest);
    let Some(head) = tokens.first() else {
        return false;
    };
    let args: Vec<&str> = tokens[1..].iter().map(String::as_str).collect();
    matches!(route_for(head, &args), Some(ShapeRoute::Brush))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_lines_never_yield() {
        assert!(!bare_shape_yields_to_brush("orkia ps aux"));
        assert!(!bare_shape_yields_to_brush("orkia"));
        assert!(!bare_shape_yields_to_brush("/ps aux"));
    }

    #[test]
    fn bare_posix_shapes_yield() {
        assert!(bare_shape_yields_to_brush("ps aux"));
        assert!(bare_shape_yields_to_brush("ps -ef"));
        assert!(bare_shape_yields_to_brush("whoami -u"));
        assert!(bare_shape_yields_to_brush("log show --last 1h"));
        assert!(bare_shape_yields_to_brush("route -n get default"));
    }

    #[test]
    fn builtin_shapes_do_not_yield() {
        assert!(!bare_shape_yields_to_brush("ps"));
        assert!(!bare_shape_yields_to_brush("ps --json"));
        assert!(!bare_shape_yields_to_brush("whoami"));
        assert!(!bare_shape_yields_to_brush("audit --verify"));
        assert!(!bare_shape_yields_to_brush("log %1"));
        assert!(!bare_shape_yields_to_brush("route show"));
    }

    #[test]
    fn non_collidable_heads_do_not_yield() {
        assert!(!bare_shape_yields_to_brush("jobs"));
        assert!(!bare_shape_yields_to_brush("kill 12345"));
        assert!(!bare_shape_yields_to_brush("ls -la"));
        assert!(!bare_shape_yields_to_brush(""));
    }

    #[test]
    fn quoted_args_tokenize_before_shape_check() {
        // A quoted token is one arg; it is not flag-shaped, so it
        // falls outside the ps grammar and yields.
        assert!(bare_shape_yields_to_brush("ps \"aux x\""));
    }
}
