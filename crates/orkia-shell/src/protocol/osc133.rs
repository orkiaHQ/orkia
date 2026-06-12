// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! OSC 133 parameter parsing.
//!
//! The actual byte-stream stripping happens in
//! `orkia-terminal-core/src/blocks.rs::BlockParser`. This module
//! exposes the parameter-set → marker mapping in pure form so it
//! can be unit-tested and called from both the BlockParser callback
//! site and any future re-implementation.

use orkia_terminal_core::Osc133Marker;

/// Parse the OSC 133 parameter list (`133` already stripped). The
/// first parameter is the marker letter (`A` / `B` / `C` / `D`).
/// `D` may carry an exit-code parameter following a `;`. Anything
/// else is returned as `None`.
pub fn parse_osc133(params: &[&[u8]]) -> Option<Osc133Marker> {
    let marker = params.first()?;
    if marker.is_empty() {
        return None;
    }
    match marker[0] {
        b'A' => Some(Osc133Marker::PromptStart),
        b'B' => Some(Osc133Marker::PromptReady),
        b'C' => Some(Osc133Marker::OutputStart),
        b'D' => {
            let exit_code = params
                .get(1)
                .and_then(|p| std::str::from_utf8(p).ok())
                .and_then(|s| s.parse::<i32>().ok());
            Some(Osc133Marker::OutputFinished { exit_code })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_a_parses() {
        let p: &[&[u8]] = &[b"A"];
        assert!(matches!(parse_osc133(p), Some(Osc133Marker::PromptStart)));
    }

    #[test]
    fn marker_b_parses() {
        let p: &[&[u8]] = &[b"B"];
        assert!(matches!(parse_osc133(p), Some(Osc133Marker::PromptReady)));
    }

    #[test]
    fn marker_c_parses() {
        let p: &[&[u8]] = &[b"C"];
        assert!(matches!(parse_osc133(p), Some(Osc133Marker::OutputStart)));
    }

    #[test]
    fn marker_d_carries_exit_code() {
        let p: &[&[u8]] = &[b"D", b"0"];
        assert!(matches!(
            parse_osc133(p),
            Some(Osc133Marker::OutputFinished { exit_code: Some(0) })
        ));

        let p: &[&[u8]] = &[b"D", b"127"];
        assert!(matches!(
            parse_osc133(p),
            Some(Osc133Marker::OutputFinished {
                exit_code: Some(127)
            })
        ));
    }

    #[test]
    fn marker_d_without_exit_code_is_none() {
        let p: &[&[u8]] = &[b"D"];
        assert!(matches!(
            parse_osc133(p),
            Some(Osc133Marker::OutputFinished { exit_code: None })
        ));
    }

    #[test]
    fn unknown_marker_returns_none() {
        let p: &[&[u8]] = &[b"Z"];
        assert!(parse_osc133(p).is_none());

        let p: &[&[u8]] = &[];
        assert!(parse_osc133(p).is_none());

        let p: &[&[u8]] = &[b""];
        assert!(parse_osc133(p).is_none());
    }

    #[test]
    fn malformed_exit_code_yields_none_code() {
        let p: &[&[u8]] = &[b"D", b"not-a-number"];
        assert!(matches!(
            parse_osc133(p),
            Some(Osc133Marker::OutputFinished { exit_code: None })
        ));
    }
}
