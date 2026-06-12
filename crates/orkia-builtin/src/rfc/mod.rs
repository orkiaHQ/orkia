// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia rfc ...` — list / show / create / edit / update / delegate / remove RFCs.

mod actions;
mod model;
mod parse;

pub use actions::{create, list, locate, rfc, show, update};
pub use model::RfcAction;
pub use parse::parse;

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn promote_without_yes_records_no_confirm() {
        let action = parse(&args(&["promote", "auth-pkce"])).expect("parse");
        match action {
            RfcAction::Promote { confirm, .. } => assert!(!confirm),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn promote_with_yes_records_confirm() {
        let action = parse(&args(&["promote", "auth-pkce", "--yes"])).expect("parse");
        match action {
            RfcAction::Promote { confirm, .. } => assert!(confirm),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn complete_abandon_reopen_recognize_yes() {
        match parse(&args(&["complete", "x", "--yes"])).expect("parse") {
            RfcAction::Complete { confirm, .. } => assert!(confirm),
            _ => panic!("complete"),
        }
        match parse(&args(&["abandon", "x", "--reason", "scope", "--yes"])).expect("parse") {
            RfcAction::Abandon {
                confirm, reason, ..
            } => {
                assert!(confirm);
                assert_eq!(reason, "scope");
            }
            _ => panic!("abandon"),
        }
        match parse(&args(&["reopen", "x", "--yes"])).expect("parse") {
            RfcAction::Reopen { confirm, .. } => assert!(confirm),
            _ => panic!("reopen"),
        }
    }

    #[test]
    fn ask_requires_q_and_rationale() {
        let r = parse(&args(&["ask", "x", "--q", "iOS?"]));
        assert!(r.is_err(), "missing rationale should error");
        let action = parse(&args(&[
            "ask",
            "x",
            "--q",
            "iOS?",
            "--rationale",
            "need scope",
        ]))
        .expect("parse");
        match action {
            RfcAction::Ask {
                question,
                rationale,
                ..
            } => {
                assert_eq!(question, "iOS?");
                assert_eq!(rationale, "need scope");
            }
            _ => panic!("ask"),
        }
    }

    #[test]
    fn resolve_parses_decision_id_and_answer() {
        let action = parse(&args(&["resolve", "d-001", "--answer", "both"])).expect("parse");
        match action {
            RfcAction::Resolve {
                decision_id,
                answer,
                ..
            } => {
                assert_eq!(decision_id, "d-001");
                assert_eq!(answer, "both");
            }
            _ => panic!("resolve"),
        }
    }
}
