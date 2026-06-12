// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Quote-aware tokenization shared by the typed-pipeline parser and the
//! argument evaluator. Mirrors the simple shell quoting used elsewhere in
//! the REPL: single and double quotes group; no escape sequences.

/// Split a line into pipeline stages on unquoted `|`.
pub fn split_pipeline(input: &str) -> Vec<String> {
    let mut stages = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in input.chars() {
        match (c, quote) {
            ('"' | '\'', None) => {
                quote = Some(c);
                cur.push(c);
            }
            (c, Some(open)) if c == open => {
                quote = None;
                cur.push(c);
            }
            ('|', None) => {
                stages.push(cur.trim().to_string());
                cur.clear();
            }
            (c, _) => cur.push(c),
        }
    }
    stages.push(cur.trim().to_string());
    stages
}

/// Tokenize a stage into whitespace-separated tokens, honoring single and
/// double quotes (the quotes group but are stripped from the token).
pub fn tokenize(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut had_token = false;
    for c in input.chars() {
        match (c, quote) {
            ('"' | '\'', None) => {
                quote = Some(c);
                had_token = true;
            }
            (c, Some(open)) if c == open => quote = None,
            (c, None) if c.is_whitespace() => {
                if had_token {
                    out.push(std::mem::take(&mut cur));
                    had_token = false;
                }
            }
            (c, _) => {
                cur.push(c);
                had_token = true;
            }
        }
    }
    if had_token {
        out.push(cur);
    }
    out
}
