// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::borrow::Cow;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Context, Helper};

use super::CompletionProvider;

/// Snapshot state shared between the helper and the REPL. Held behind
/// `Arc<ArcSwap<_>>` so the REPL pushes a fresh snapshot by swapping
/// the Arc (lock-free, single atomic store) and the rustyline helper
/// reads by cloning an Arc (lock-free atomic load). Replaces an earlier
/// `Arc<RwLock<_>>` which violated invariant #1 — the REPL would park
/// on the write lock whenever the completion worker held a read lock
/// while expanding a glob over a large directory.
#[derive(Default, Clone)]
pub struct HelperShared {
    pub agents: Vec<String>,
    pub history_tail: Vec<String>,
    /// used to complete `$team cd|show|rm <ident>`. The REPL pushes
    /// a fresh `HelperShared` whenever the cache refreshes.
    pub team_identifiers: Vec<String>,
    /// Project identifiers (currently project names) from the
    /// workspace snapshot, used to complete `$share project <ident>`.
    pub project_identifiers: Vec<String>,
    /// deduped: builtin-table names ∪ `CommandRegistry` names ∪ `$PATH`
    /// executable basenames. Large but slow-changing: rebuilt only when a
    /// `$PATH` dir mtime changes (F1). Shared by `Arc` so the per-prompt
    /// carry-forward is a refcount bump, not a deep `Vec` copy (F3).
    pub stable_commands: Arc<Vec<String>>,
    /// brush alias names (function names deferred). Small; re-read from
    /// brush every prompt so an alias defined in-session is recognized at
    /// the next prompt (F2).
    pub dynamic_commands: Vec<String>,
    /// cold from history. The autosuggestion fallback prefers the highest-
    /// weighted match over the alphabetically-first one (F4).
    pub command_freq: std::collections::HashMap<String, f64>,
}

impl HelperShared {
    pub fn new_arc() -> Arc<ArcSwap<Self>> {
        Arc::new(ArcSwap::from_pointee(Self::default()))
    }
}

pub struct OrkiaHelper {
    brush: Box<dyn CompletionProvider>,
    builtins: &'static [&'static str],
    shared: Arc<ArcSwap<HelperShared>>,
    files: FilenameCompleter,
    disabled: bool,
}

impl OrkiaHelper {
    pub fn new(brush: Box<dyn CompletionProvider>, shared: Arc<ArcSwap<HelperShared>>) -> Self {
        Self {
            brush,
            // the curated static list (and its phantom entries) is gone.
            builtins: crate::builtin_table::completion_names(),
            shared,
            files: FilenameCompleter::new(),
            disabled: std::env::var_os("ORKIA_NO_COMPLETE").is_some(),
        }
    }

    fn complete_agents(&self, line: &str, pos: usize) -> Option<(usize, Vec<Pair>)> {
        let (token_start, token) = current_token(line, pos);
        if !token.starts_with('@') {
            return None;
        }
        let prefix = &token[1..];
        let shared = self.shared.load();
        let mut pairs: Vec<Pair> = shared
            .agents
            .iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| Pair {
                display: format!("@{name}"),
                replacement: format!("@{name}"),
            })
            .collect();
        pairs.sort_by(|a, b| a.replacement.cmp(&b.replacement));
        Some((token_start, pairs))
    }

    fn complete_first_word(&self, line: &str, pos: usize) -> Option<(usize, Vec<Pair>)> {
        let (token_start, token) = current_token(line, pos);
        if token_start != line_word_start(line) {
            return None;
        }
        let pairs: Vec<Pair> = self
            .builtins
            .iter()
            .filter(|name| name.starts_with(token))
            .map(|name| Pair {
                display: (*name).into(),
                replacement: (*name).into(),
            })
            .collect();
        if pairs.is_empty() {
            return None;
        }
        Some((token_start, pairs))
    }

    fn complete_via_brush(&self, line: &str, pos: usize) -> Option<(usize, Vec<Pair>)> {
        let sug = self.brush.complete(line, pos);
        if sug.candidates.is_empty() {
            return None;
        }
        let pairs = sug
            .candidates
            .into_iter()
            .map(|c| Pair {
                display: c.clone(),
                replacement: c,
            })
            .collect();
        Some((sug.insertion_index, pairs))
    }

    /// Context-aware completion of team-mode subcommand arguments.
    /// Detects the first-word + slot position and pulls candidates
    /// from `HelperShared` (which the REPL keeps in sync with
    /// `TeamCache` and the workspace snapshot).
    ///
    /// Handled contexts:
    /// - `team {cd,show,rm} <ident>` → team identifiers.
    /// - `share project <ident> …`   → project identifiers.
    /// - `share issue <ident> …`     → project identifiers (issue ids
    ///   aren't surfaced in the cache today; fall back to projects
    ///   to give the user something useful).
    /// - `share unshare {project,issue} <ident> …` → as above.
    ///
    /// Member-email completion is V1.5 (workspace_members snapshot
    /// doesn't surface emails yet — flagged in DEFERRED).
    fn complete_team_subcommand(&self, line: &str, pos: usize) -> Option<(usize, Vec<Pair>)> {
        let (token_start, token) = current_token(line, pos);
        let prefix_text = &line[..token_start];
        let trimmed = prefix_text.trim_start();
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        let shared = self.shared.load();
        let candidates: &[String] = match words.as_slice() {
            // `team cd|show|rm <ident>`
            ["team", "cd"] | ["team", "show"] | ["team", "rm"] | ["team", "remove"] => {
                &shared.team_identifiers
            }
            // `share project <ident>` / `share issue <ident>` (only the
            // first <ident> slot — subsequent args are workspace UUIDs
            // and not cache-resolvable).
            ["share", "project"] | ["share", "issue"] => &shared.project_identifiers,
            ["share", "unshare", "project"] | ["share", "unshare", "issue"] => {
                &shared.project_identifiers
            }
            _ => return None,
        };
        if candidates.is_empty() {
            return None;
        }
        let pairs: Vec<Pair> = candidates
            .iter()
            .filter(|name| name.starts_with(token))
            .map(|name| Pair {
                display: name.clone(),
                replacement: name.clone(),
            })
            .collect();
        if pairs.is_empty() {
            return None;
        }
        Some((token_start, pairs))
    }

    fn complete_via_files(&self, line: &str, pos: usize) -> Option<(usize, Vec<Pair>)> {
        // Use rustyline's FilenameCompleter without recursing into the
        // helper (it's stateless w.r.t. the editor history).
        let history = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&history);
        match self.files.complete(line, pos, &ctx) {
            Ok((start, pairs)) if !pairs.is_empty() => Some((start, pairs)),
            _ => None,
        }
    }
}

fn current_token(line: &str, pos: usize) -> (usize, &str) {
    let upto = &line[..pos];
    let start = upto.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    (start, &line[start..pos])
}

fn line_word_start(line: &str) -> usize {
    line.bytes()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(0)
}

impl Completer for OrkiaHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> Result<(usize, Vec<Pair>), ReadlineError> {
        if self.disabled || pos > line.len() {
            return Ok((pos, Vec::new()));
        }

        if let Some((start, pairs)) = self.complete_agents(line, pos) {
            return Ok((start, pairs));
        }

        // Team-mode subcommand argument completion (cache-driven).
        // Tried before the brush/files fallbacks so a typo'd team
        // identifier prefix yields cache hits instead of stray
        // file-name matches.
        if let Some((start, pairs)) = self.complete_team_subcommand(line, pos) {
            return Ok((start, pairs));
        }

        // First-word case: merge orkia builtins with brush's command-name
        // suggestions, preferring brush's reply if it has anything.
        if let Some((start, mut builtin_pairs)) = self.complete_first_word(line, pos) {
            if let Some((brush_start, brush_pairs)) = self.complete_via_brush(line, pos)
                && brush_start == start
            {
                merge_pairs(&mut builtin_pairs, brush_pairs);
            }
            return Ok((start, builtin_pairs));
        }

        if let Some(out) = self.complete_via_brush(line, pos) {
            return Ok(out);
        }

        if let Some(out) = self.complete_via_files(line, pos) {
            return Ok(out);
        }

        Ok((pos, Vec::new()))
    }
}

fn merge_pairs(target: &mut Vec<Pair>, extra: Vec<Pair>) {
    use std::collections::HashSet;
    let seen: HashSet<String> = target.iter().map(|p| p.replacement.clone()).collect();
    for p in extra {
        if !seen.contains(&p.replacement) {
            target.push(p);
        }
    }
}

impl Hinter for OrkiaHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if self.disabled || line.is_empty() || pos != line.len() {
            return None;
        }
        let shared = self.shared.load();

        // 1. History (primary, fish behaviour): suffix of the most recent
        //    history line that extends the typed prefix.
        if let Some(hit) = shared
            .history_tail
            .iter()
            .rev()
            .find(|h| h.len() > line.len() && h.starts_with(line))
        {
            return Some(hit[line.len()..].to_string());
        }

        //    only while typing the first token (no whitespace yet), and only
        //    from snapshot data — never the filesystem or the async worker.
        if line.contains(char::is_whitespace) {
            return None;
        }
        if let Some(rest) = line.strip_prefix('@') {
            if rest.is_empty() {
                return None;
            }
            let best = shared
                .agents
                .iter()
                .filter(|a| a.len() > rest.len() && a.starts_with(rest))
                .min_by_key(|a| a.len())?;
            return Some(best[rest.len()..].to_string());
        }
        let best = super::syntax::best_command_hint(
            &shared.stable_commands,
            &shared.dynamic_commands,
            &shared.command_freq,
            line,
        )?;
        Some(best[line.len()..].to_string())
    }
}

impl Highlighter for OrkiaHelper {
    /// Snapshot-only, O(line length); inserts zero-width ANSI so the
    /// display width is preserved (rustyline invariant).
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if self.disabled || line.is_empty() {
            return Cow::Borrowed(line);
        }
        let shared = self.shared.load();
        super::syntax::highlight_line(
            line,
            &shared.stable_commands,
            &shared.dynamic_commands,
            &shared.agents,
        )
    }

    /// Re-highlight on edits (and forced refresh), but not on bare cursor
    /// moves — colours are position-independent, so re-painting on `MoveCursor`
    /// would be wasted work.
    fn highlight_char(&self, line: &str, _pos: usize, kind: CmdKind) -> bool {
        !self.disabled && !line.is_empty() && kind != CmdKind::MoveCursor
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        // Dim grey, fish-style.
        Cow::Owned(format!("\x1b[90m{hint}\x1b[0m"))
    }
}

impl Validator for OrkiaHelper {
    /// Detect "the user wants to continue typing" so rustyline
    /// keeps the line buffer open across a literal Enter and
    /// shows a `> ` continuation prompt on the next line.
    ///
    /// Two triggers (match bash interactive line discipline):
    ///   1. Trailing unescaped backslash — `cmd \\` + Enter.
    ///   2. Unclosed single or double quote.
    ///
    /// Pure-text inspection; no brush parser invocation, so this
    /// is cheap on every keystroke. Heredocs and unfinished
    /// control structures (`for ... do`, `if ...`) are NOT
    /// continued — those would need brush's parser, deferred.
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        let input = ctx.input();
        if input_is_incomplete(input) {
            Ok(ValidationResult::Incomplete)
        } else {
            Ok(ValidationResult::Valid(None))
        }
    }

    fn validate_while_typing(&self) -> bool {
        // Re-validate on every keystroke so Enter inside an open
        // quote or after `\` instantly opens a continuation line.
        false
    }
}

/// Returns `true` when the input ends in an incomplete bash
/// construct that should trigger a continuation line. Detects:
///   - trailing unescaped backslash (line continuation)
///   - unclosed single or double quote
///
/// Backticks and `$(` aren't handled in v1 — they're parser-
/// level, and our partial detection would be worse than nothing
/// (false positives leave the prompt stuck).
///
/// Two independent passes:
///   1. Quote state — walk the input, toggle in_single/in_double
///      on unescaped quotes. Backslash inside single quotes is
///      literal (bash semantics); elsewhere it escapes the next
///      character.
///   2. Trailing-backslash check — strip trailing whitespace,
///      count consecutive backslashes at the end. Odd = the last
///      one is unescaped = continuation.
fn input_is_incomplete(input: &str) -> bool {
    if input.trim().is_empty() {
        return false;
    }
    // Comments first: in bash everything from an unquoted word-initial `#`
    // to end of line is ignored, so a quote or trailing backslash inside a
    // comment must not open a continuation (`echo hi # don't`).
    let effective = strip_comments(input);
    if has_unclosed_quote(&effective) {
        return true;
    }
    has_trailing_unescaped_backslash(&effective)
}

/// Remove bash comments: an unescaped `#` outside quotes that begins a word
/// (start of line or preceded by whitespace) starts a comment running to the
/// end of that line. Mid-word `#` (`echo foo#bar`) is literal, as in bash.
/// Quote state is tracked with the same semantics as [`has_unclosed_quote`]
/// so a `#` inside an open quote is never treated as a comment.
fn strip_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_backslash = false;
    let mut at_word_start = true;
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if prev_backslash {
            prev_backslash = false;
            at_word_start = false;
            out.push(c);
            continue;
        }
        match c {
            '\\' if !in_single => {
                prev_backslash = true;
                at_word_start = false;
                out.push(c);
            }
            '\'' if !in_double => {
                in_single = !in_single;
                at_word_start = false;
                out.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                at_word_start = false;
                out.push(c);
            }
            '#' if !in_single && !in_double && at_word_start => {
                // Comment: drop to end of line. The newline survives — it
                // terminates the comment and still separates lines.
                for rest in chars.by_ref() {
                    if rest == '\n' {
                        out.push('\n');
                        break;
                    }
                }
                at_word_start = true;
            }
            c => {
                at_word_start = matches!(c, ' ' | '\t' | '\n');
                out.push(c);
            }
        }
    }
    out
}

/// Walk the input tracking quote state. Returns true if either
/// a `'` or `"` was opened and never matched. Bash semantics:
/// inside `'...'`, a `\` is literal (does NOT escape the closing
/// `'`); inside `"..."`, `\` does escape the next char.
fn has_unclosed_quote(input: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_backslash = false;
    for c in input.chars() {
        if prev_backslash {
            // Previous backslash escaped this char (outside
            // single quotes — we don't reach this branch when
            // in_single is true).
            prev_backslash = false;
            continue;
        }
        match c {
            '\\' if !in_single => prev_backslash = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ => {}
        }
    }
    in_single || in_double
}

/// Count consecutive `\` characters at the end of the input
/// (after stripping trailing horizontal whitespace, which bash
/// also ignores before the line-continuation marker). An odd
/// count means the last backslash is unpaired, signalling a
/// continuation.
fn has_trailing_unescaped_backslash(input: &str) -> bool {
    let trimmed = input.trim_end_matches([' ', '\t', '\n', '\r']);
    let count = trimmed.chars().rev().take_while(|c| *c == '\\').count();
    count % 2 == 1
}

impl Helper for OrkiaHelper {}

#[cfg(test)]
#[path = "helper_tests.rs"]
mod tests;
