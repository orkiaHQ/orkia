// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;
use crate::completion::{NullProvider, Suggestion};

struct FixedProvider {
    sug: Suggestion,
}

impl CompletionProvider for FixedProvider {
    fn complete(&self, _line: &str, _pos: usize) -> Suggestion {
        self.sug.clone()
    }
}

fn helper(provider: Box<dyn CompletionProvider>) -> (OrkiaHelper, Arc<ArcSwap<HelperShared>>) {
    let shared = HelperShared::new_arc();
    (OrkiaHelper::new(provider, shared.clone()), shared)
}

fn replace_shared(shared: &Arc<ArcSwap<HelperShared>>, f: impl FnOnce(&mut HelperShared)) {
    let mut next = (**shared.load()).clone();
    f(&mut next);
    shared.store(Arc::new(next));
}

#[test]
fn agent_prefix_completes_from_shared() {
    let (h, shared) = helper(Box::new(NullProvider));
    replace_shared(&shared, |s| {
        s.agents = vec!["alpha".into(), "anvil".into(), "beta".into()];
    });

    let history = rustyline::history::DefaultHistory::new();
    let ctx = Context::new(&history);
    let (start, pairs) = h.complete("@a", 2, &ctx).unwrap();
    assert_eq!(start, 0);
    let names: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
    assert_eq!(names, vec!["@alpha", "@anvil"]);
}

#[test]
fn first_word_merges_builtins_and_brush() {
    // brush proposes "agent-2" (a hypothetical PATH binary); helper
    // already has "agent" as a builtin.
    let provider = FixedProvider {
        sug: Suggestion {
            insertion_index: 0,
            replace_len: 0,
            candidates: vec!["agent-2".into()],
        },
    };
    let (h, _) = helper(Box::new(provider));
    let history = rustyline::history::DefaultHistory::new();
    let ctx = Context::new(&history);

    let (start, pairs) = h.complete("age", 3, &ctx).unwrap();
    assert_eq!(start, 0);
    let names: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
    assert!(names.contains(&"agent"));
    assert!(names.contains(&"agent-2"));
}

#[test]
fn falls_back_to_files_when_brush_empty() {
    // Brush returns nothing, line isn't a first-word case.
    let provider = FixedProvider {
        sug: Suggestion::empty(),
    };
    let (h, _) = helper(Box::new(provider));
    let history = rustyline::history::DefaultHistory::new();
    let ctx = Context::new(&history);

    // Ask to complete a path that almost certainly exists.
    let line = "ls /";
    let (_, pairs) = h.complete(line, line.len(), &ctx).unwrap();
    assert!(
        !pairs.is_empty(),
        "FilenameCompleter should propose entries under /"
    );
}

#[test]
fn hint_returns_suffix_from_history() {
    let (h, shared) = helper(Box::new(NullProvider));
    replace_shared(&shared, |s| {
        s.history_tail = vec!["cargo test".into(), "cargo build --release".into()];
    });

    let history = rustyline::history::DefaultHistory::new();
    let ctx = Context::new(&history);
    assert_eq!(
        h.hint("cargo b", 7, &ctx).as_deref(),
        Some("uild --release")
    );
    // Cursor not at end -> no hint.
    assert_eq!(h.hint("cargo b extra", 7, &ctx), None);
    // No match -> no hint.
    assert_eq!(h.hint("xyz", 3, &ctx), None);
}

#[test]
fn hint_falls_back_to_command_snapshot() {
    // No history match → suggest the best command completion from the
    let (h, shared) = helper(Box::new(NullProvider));
    replace_shared(&shared, |s| {
        s.stable_commands = Arc::new(vec!["where".into(), "whoami".into()]); // sorted
    });
    let history = rustyline::history::DefaultHistory::new();
    let ctx = Context::new(&history);
    assert_eq!(h.hint("wher", 4, &ctx).as_deref(), Some("e"));
    // Mid-line (whitespace present) → no command fallback.
    assert_eq!(h.hint("wher x", 6, &ctx), None);
}

#[test]
fn hint_history_beats_command_fallback() {
    let (h, shared) = helper(Box::new(NullProvider));
    replace_shared(&shared, |s| {
        s.history_tail = vec!["where size > 1mb".into()];
        s.stable_commands = Arc::new(vec!["where".into()]);
    });
    let history = rustyline::history::DefaultHistory::new();
    let ctx = Context::new(&history);
    // History wins: the full recalled suffix, not just "e".
    assert_eq!(h.hint("wher", 4, &ctx).as_deref(), Some("e size > 1mb"));
}

#[test]
fn hint_fallback_prefers_frequency() {
    // F4: git 100× vs gcc 1× → "g" suggests git, not alphabetical gcc.
    let (h, shared) = helper(Box::new(NullProvider));
    replace_shared(&shared, |s| {
        s.stable_commands = Arc::new(vec!["gcc".into(), "git".into()]);
        let mut freq = std::collections::HashMap::new();
        freq.insert("git".to_string(), 100.0);
        freq.insert("gcc".to_string(), 1.0);
        s.command_freq = freq;
    });
    let history = rustyline::history::DefaultHistory::new();
    let ctx = Context::new(&history);
    assert_eq!(h.hint("g", 1, &ctx).as_deref(), Some("it"));
}

#[test]
fn hint_falls_back_to_agent_snapshot() {
    let (h, shared) = helper(Box::new(NullProvider));
    replace_shared(&shared, |s| {
        s.agents = vec!["alpha".into()];
    });
    let history = rustyline::history::DefaultHistory::new();
    let ctx = Context::new(&history);
    assert_eq!(h.hint("@al", 3, &ctx).as_deref(), Some("pha"));
}

#[test]
fn highlight_colours_valid_and_unknown() {
    let (h, shared) = helper(Box::new(NullProvider));
    replace_shared(&shared, |s| {
        s.stable_commands = Arc::new(vec!["ls".into()]);
    });
    let valid = h.highlight("ls -la", 6);
    let unknown = h.highlight("zzz", 3);
    assert!(valid.contains("\x1b[32m"), "ls coloured valid");
    assert!(unknown.contains("\x1b[31m"), "zzz coloured unknown");
}

#[test]
fn disabled_via_env_returns_empty() {
    // Manually flip the flag without env munging (avoids global state).
    let mut h = OrkiaHelper::new(Box::new(NullProvider), HelperShared::new_arc());
    h.disabled = true;
    let history = rustyline::history::DefaultHistory::new();
    let ctx = Context::new(&history);
    let (_, pairs) = h.complete("agent", 5, &ctx).unwrap();
    assert!(pairs.is_empty());
    assert_eq!(h.hint("foo", 3, &ctx), None);
}

#[test]
fn trailing_backslash_signals_continuation() {
    assert!(input_is_incomplete("echo foo \\"));
    assert!(input_is_incomplete("ls \\"));
}

#[test]
fn escaped_backslash_does_not_continue() {
    // `\\` is an escaped backslash — the line ENDS with a
    // literal backslash, not a continuation marker. Equivalent
    // to bash: `echo \\` + Enter prints a single backslash.
    assert!(!input_is_incomplete("echo \\\\"));
}

#[test]
fn unclosed_single_quote_continues() {
    assert!(input_is_incomplete("echo 'hello"));
    assert!(!input_is_incomplete("echo 'hello'"));
}

#[test]
fn unclosed_double_quote_continues() {
    assert!(input_is_incomplete("echo \"hello"));
    assert!(!input_is_incomplete("echo \"hello\""));
}

#[test]
fn quote_inside_other_quote_does_not_toggle() {
    // `"it's fine"` — the apostrophe inside double quotes
    // is literal, not a quote toggle.
    assert!(!input_is_incomplete("echo \"it's fine\""));
    // Symmetric case.
    assert!(!input_is_incomplete("echo 'he said \"hi\"'"));
}

#[test]
fn backslash_inside_single_quote_is_literal() {
    // In bash, backslash inside single quotes is just a
    // backslash — no escape semantics. `'foo\'` is "foo\"
    // (still inside the quote, so the line is incomplete).
    assert!(input_is_incomplete("echo 'foo\\"));
}

#[test]
fn empty_input_is_complete() {
    assert!(!input_is_incomplete(""));
    assert!(!input_is_incomplete("   "));
    assert!(!input_is_incomplete("\n"));
}

#[test]
fn plain_line_is_complete() {
    assert!(!input_is_incomplete("echo hello world"));
    assert!(!input_is_incomplete("cd /tmp && ls"));
}

#[test]
fn continuation_then_complete() {
    // After the continuation, rustyline calls validate with
    // the JOINED buffer (including the newline). A complete
    // input after continuation must validate.
    assert!(!input_is_incomplete("echo foo \\\nbar"));
}

#[test]
fn quote_inside_comment_is_complete() {
    // Bash ignores everything after an unquoted word-initial `#`,
    // quotes included — no continuation.
    assert!(!input_is_incomplete("echo hi # don't"));
    assert!(!input_is_incomplete("# it's a 'comment"));
    assert!(!input_is_incomplete("ls \"x\" # unmatched \" here"));
}

#[test]
fn trailing_backslash_inside_comment_is_complete() {
    assert!(!input_is_incomplete("echo hi # trailing \\"));
}

#[test]
fn hash_mid_word_or_quoted_is_not_a_comment() {
    // Mid-word `#` is literal in bash — the quote after it counts.
    assert!(input_is_incomplete("echo foo#bar'"));
    // `#` inside quotes never starts a comment.
    assert!(!input_is_incomplete("echo 'a # b'"));
    assert!(input_is_incomplete("echo 'a # b"));
    assert!(input_is_incomplete("echo \"a # b"));
}

#[test]
fn comment_only_ends_at_newline() {
    // The comment swallows to end of line, not end of input: an
    // unclosed quote on the NEXT line still continues.
    assert!(input_is_incomplete("# note\necho 'open"));
    assert!(!input_is_incomplete("# note\necho 'closed'"));
}
