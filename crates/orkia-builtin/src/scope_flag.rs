// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Shared `--scope` flag parser for builtins that touch scoped
//! artifacts (project, RFC, issue).
//!
//! Extracted into its own module so the four creation/update sites
//! agree on the surface: `--scope=<value>` and `--scope <value>` both
//! work, an unknown value errors out the same way everywhere, and the
//! flag is stripped from the residual args before the caller's
//! existing parser sees them.

use orkia_shell_types::scope::{Scope, ScopeParseError};

/// Extract a `--scope=<value>` (or `--scope <value>`) flag from a
/// builtin's argument list. Returns `(parsed_scope, remaining_args)`
/// where `remaining_args` does NOT contain the scope tokens.
pub fn parse_scope_flag(args: &[String]) -> Result<(Option<Scope>, Vec<String>), ScopeParseError> {
    let mut scope: Option<Scope> = None;
    let mut remaining: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(value) = arg.strip_prefix("--scope=") {
            if scope.is_some() {
                tracing::warn!("--scope specified multiple times; last value wins");
            }
            scope = Some(Scope::parse(value)?);
            i += 1;
            continue;
        }
        if arg == "--scope" {
            let value = args
                .get(i + 1)
                .ok_or_else(|| ScopeParseError::Unknown("--scope without value".into()))?;
            if scope.is_some() {
                tracing::warn!("--scope specified multiple times; last value wins");
            }
            scope = Some(Scope::parse(value)?);
            i += 2;
            continue;
        }
        remaining.push(arg.clone());
        i += 1;
    }
    Ok((scope, remaining))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_scope_flag() {
        let (sc, rest) = parse_scope_flag(&s(&["foo", "bar"])).unwrap();
        assert!(sc.is_none());
        assert_eq!(rest, s(&["foo", "bar"]));
    }

    #[test]
    fn equals_form() {
        let (sc, rest) = parse_scope_flag(&s(&["--scope=public", "foo"])).unwrap();
        assert_eq!(sc, Some(Scope::Public));
        assert_eq!(rest, s(&["foo"]));
    }

    #[test]
    fn two_token_form() {
        let (sc, rest) = parse_scope_flag(&s(&["foo", "--scope", "team"])).unwrap();
        assert_eq!(sc, Some(Scope::Team));
        assert_eq!(rest, s(&["foo"]));
    }

    #[test]
    fn unknown_value_errors() {
        let result = parse_scope_flag(&s(&["--scope=internal"]));
        assert!(matches!(result, Err(ScopeParseError::Unknown(_))));
    }

    #[test]
    fn missing_value_errors() {
        let result = parse_scope_flag(&s(&["--scope"]));
        assert!(result.is_err());
    }

    #[test]
    fn preserves_argument_order() {
        let (sc, rest) = parse_scope_flag(&s(&["a", "--scope=public", "b", "c"])).unwrap();
        assert_eq!(sc, Some(Scope::Public));
        assert_eq!(rest, s(&["a", "b", "c"]));
    }

    #[test]
    fn last_value_wins_on_duplicate() {
        let (sc, _) = parse_scope_flag(&s(&["--scope=public", "--scope=team"])).unwrap();
        assert_eq!(sc, Some(Scope::Team));
    }
}
