// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! The actual `leave_workspace` call is gated server-side (owner
//! cannot leave). This parser is intentionally tiny; the only
//! decision it surfaces is whether the user pre-confirmed.

#[derive(Debug, Clone, PartialEq)]
pub struct LeaveAction {
    pub confirmed: bool,
}

pub fn parse(args: &[String]) -> Result<LeaveAction, String> {
    let confirmed = args.iter().any(|a| a == "--yes" || a == "-y");
    let extra = args.iter().any(|a| a != "--yes" && a != "-y");
    if extra {
        return Err("usage: leave [--yes]".into());
    }
    Ok(LeaveAction { confirmed })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_requires_confirmation() {
        assert_eq!(parse(&[]).unwrap(), LeaveAction { confirmed: false });
    }

    #[test]
    fn yes_flag_pre_confirms() {
        let yes = vec!["--yes".to_string()];
        assert_eq!(parse(&yes).unwrap(), LeaveAction { confirmed: true });
    }

    #[test]
    fn unknown_arg_errors() {
        let bad = vec!["nope".to_string()];
        assert!(parse(&bad).is_err());
    }
}
