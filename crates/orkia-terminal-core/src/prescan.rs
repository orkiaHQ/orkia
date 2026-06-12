// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Byte prescan for CSI private-mode signals used by the state machine.
//!
//! [`PreScanner`] is stateful: an incomplete trailing `ESC [ ? <num> (h|l)`
//! sequence split across two reads is carried to the next scan instead of
//! being silently dropped (BUG-104). The carried prefix is bounded so a
//! malformed stream can never grow it without limit (treat every byte as
//! untrusted).

use crate::state::Signal;

/// Maximum bytes of an incomplete trailing sequence we carry between scans.
/// A real `ESC [ ? <digits> (h|l)` private-mode sequence is short; anything
/// longer without a terminator is malformed and is not carried.
const MAX_PENDING: usize = 16;

/// Stateful prescanner that survives sequences split across reads.
#[derive(Debug, Default)]
pub struct PreScanner {
    /// Bytes of an incomplete trailing CSI private-mode sequence, prepended to
    /// the next scan.
    pending: Vec<u8>,
}

impl PreScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan `bytes` (joined with any carried prefix) for private-mode signals,
    /// retaining an incomplete trailing sequence for the next call.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<Signal> {
        if self.pending.is_empty() {
            let (signals, leftover) = scan_with_leftover(bytes);
            self.pending = leftover;
            signals
        } else {
            let mut combined = std::mem::take(&mut self.pending);
            combined.extend_from_slice(bytes);
            let (signals, leftover) = scan_with_leftover(&combined);
            self.pending = leftover;
            signals
        }
    }
}

/// Outcome of attempting to parse a private-mode sequence at a position.
enum ParseResult {
    /// A full `ESC [ ? <digits> (h|l)` was consumed; `signal` may be `None`
    /// for a recognised-but-unmapped mode number.
    Complete {
        signal: Option<Signal>,
        consumed: usize,
    },
    /// The bytes are a prefix of a valid sequence but run off the end.
    Incomplete,
    /// Not a private-mode sequence at this position.
    NoMatch,
}

/// Scan a buffer, returning the emitted signals and any trailing incomplete
/// private-mode sequence (to be carried to the next scan).
fn scan_with_leftover(bytes: &[u8]) -> (Vec<Signal>, Vec<u8>) {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            match parse_private_mode(&bytes[i..]) {
                ParseResult::Complete { signal, consumed } => {
                    if let Some(s) = signal {
                        out.push(s);
                    }
                    i += consumed;
                    continue;
                }
                ParseResult::Incomplete => {
                    let leftover = &bytes[i..];
                    if leftover.len() <= MAX_PENDING {
                        // Carry the partial sequence to the next read.
                        return (out, leftover.to_vec());
                    }
                    // Too long to be a real private-mode sequence — drop the
                    // ESC and keep scanning rather than carrying unbounded data.
                    i += 1;
                    continue;
                }
                ParseResult::NoMatch => {
                    i += 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    (out, Vec::new())
}

/// Try to parse `ESC [ ? <digits> (h|l)` starting at `b[0] == ESC`.
fn parse_private_mode(b: &[u8]) -> ParseResult {
    // Caller guarantees b[0] == ESC.
    if b.len() < 2 {
        return ParseResult::Incomplete;
    }
    if b[1] != b'[' {
        return ParseResult::NoMatch;
    }
    if b.len() < 3 {
        return ParseResult::Incomplete;
    }
    if b[2] != b'?' {
        return ParseResult::NoMatch;
    }
    let mut j = 3;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    if j == b.len() {
        // Digits (or nothing) run to the end with no terminator yet.
        return ParseResult::Incomplete;
    }
    if j == 3 {
        // No digits before the terminator → not a valid private-mode set.
        return ParseResult::NoMatch;
    }
    let set = match b[j] {
        b'h' => true,
        b'l' => false,
        _ => return ParseResult::NoMatch,
    };
    let consumed = j + 1;
    let Some(n) = std::str::from_utf8(&b[3..j])
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return ParseResult::Complete {
            signal: None,
            consumed,
        };
    };
    let signal = match (n, set) {
        (1049, true) | (1047, true) | (47, true) => Some(Signal::AltEnter),
        (1049, false) | (1047, false) | (47, false) => Some(Signal::AltExit),
        (2004, true) => Some(Signal::BracketedEnter),
        (2004, false) => Some(Signal::BracketedExit),
        (25, false) => Some(Signal::CursorHide),
        (25, true) => Some(Signal::CursorShow),
        (1, true) => Some(Signal::AppCursorEnter),
        (1, false) => Some(Signal::AppCursorExit),
        _ => None,
    };
    ParseResult::Complete { signal, consumed }
}

/// Stateless one-shot scan (drops any trailing incomplete sequence). Retained
/// for callers/tests that scan a single complete buffer.
pub fn prescan(bytes: &[u8]) -> Vec<Signal> {
    scan_with_leftover(bytes).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_complete_sequences() {
        let mut s = PreScanner::new();
        assert_eq!(s.scan(b"\x1b[?1049h"), vec![Signal::AltEnter]);
        assert_eq!(s.scan(b"\x1b[?1049l"), vec![Signal::AltExit]);
    }

    #[test]
    fn carries_split_sequence_across_reads() {
        let mut s = PreScanner::new();
        // Sequence ESC [ ? 1 0 4 9 h split right before the terminator.
        assert_eq!(s.scan(b"\x1b[?1049"), Vec::<Signal>::new());
        assert_eq!(s.scan(b"h"), vec![Signal::AltEnter]);
    }

    #[test]
    fn carries_split_at_prefix() {
        let mut s = PreScanner::new();
        assert_eq!(s.scan(b"text\x1b["), Vec::<Signal>::new());
        assert_eq!(s.scan(b"?2004h"), vec![Signal::BracketedEnter]);
    }

    #[test]
    fn drops_overlong_incomplete_prefix() {
        let mut s = PreScanner::new();
        // ESC [ ? followed by too many digits and no terminator → not carried.
        let long = b"\x1b[?12345678901234567890";
        assert_eq!(s.scan(long), Vec::<Signal>::new());
        // A following real sequence is still detected (prefix wasn't stuck).
        assert_eq!(s.scan(b"\x1b[?25l"), vec![Signal::CursorHide]);
    }

    #[test]
    fn stateless_prescan_still_works() {
        assert_eq!(prescan(b"\x1b[?47h"), vec![Signal::AltEnter]);
    }
}
