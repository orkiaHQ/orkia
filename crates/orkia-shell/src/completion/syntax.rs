// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Hot-path syntax highlighting + autosuggestion helpers —
//!
//! Pure, snapshot-only, O(line length): a light tokenizer that inserts
//! zero-width ANSI colour codes around shell tokens. It never touches the
//! filesystem, the network, or the async brush worker — the caller passes the
//! already-loaded snapshot slices (`commands`, `agents`). The highlighted
//! string has the **same display width** as the input (rustyline invariant):
//! only escape sequences are inserted, never visible characters.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;

const RESET: &str = "\x1b[0m";
const CMD_VALID: &str = "\x1b[32m"; // green — recognized command
const CMD_UNKNOWN: &str = "\x1b[31m"; // red — not in the snapshot
const AGENT: &str = "\x1b[35m"; // magenta — known @agent
const AGENT_UNKNOWN: &str = "\x1b[31m"; // red — unknown @agent
const NAMESPACE: &str = "\x1b[34m"; // blue — `ork`/`orkia` namespace
const OPERATOR: &str = "\x1b[33m"; // yellow — | > >> && ||
const STRING_OK: &str = "\x1b[36m"; // cyan — closed quote
const STRING_BAD: &str = "\x1b[31m"; // red — unclosed quote
const FLAG: &str = "\x1b[36m"; // cyan — -x / --long

#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Whitespace,
    Operator,
    Str { closed: bool },
    Word,
}

/// `true` for characters that terminate a bare word (operators / quotes).
fn is_word_break(c: char) -> bool {
    c.is_whitespace() || matches!(c, '|' | '>' | '<' | '&' | ';' | '"' | '\'')
}

/// Match a shell operator at the start of `s`, returning its byte length.
fn operator_len(s: &str) -> Option<usize> {
    for op in ["&&", "||", ">>"] {
        if s.starts_with(op) {
            return Some(2);
        }
    }
    let c = s.chars().next()?;
    if matches!(c, '|' | '>' | '<' | '&' | ';') {
        Some(1)
    } else {
        None
    }
}

/// Does the operator start a fresh command stage (so the next word is a
/// command), as opposed to a redirection / background marker?
fn operator_resets_command(op: &str) -> bool {
    matches!(op, "|" | "&&" | "||" | ";")
}

/// Split `line` into `(start, end, kind)` spans covering it exactly.
fn tokenize(line: &str) -> Vec<(usize, usize, Tok)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < line.len() {
        let rest = &line[i..];
        let Some(c) = rest.chars().next() else { break };

        if c.is_whitespace() {
            let end = i + rest
                .find(|c: char| !c.is_whitespace())
                .unwrap_or(rest.len());
            spans.push((i, end, Tok::Whitespace));
            i = end;
        } else if let Some(op_len) = operator_len(rest) {
            spans.push((i, i + op_len, Tok::Operator));
            i += op_len;
        } else if c == '"' || c == '\'' {
            let after = &rest[c.len_utf8()..];
            match after.find(c) {
                Some(rel) => {
                    let end = i + c.len_utf8() + rel + c.len_utf8();
                    spans.push((i, end, Tok::Str { closed: true }));
                    i = end;
                }
                None => {
                    spans.push((i, line.len(), Tok::Str { closed: false }));
                    i = line.len();
                }
            }
        } else {
            let end = i + rest.find(is_word_break).unwrap_or(rest.len());
            spans.push((i, end, Tok::Word));
            i = end;
        }
    }
    spans
}

fn is_namespace(word: &str) -> bool {
    word == "ork" || word == "orkia"
}

/// Highlight a command line. Returns `Borrowed` when there is nothing to
/// colour (empty line), `Owned` with ANSI codes otherwise. `stable` and
/// `dynamic` are the two halves of the validity set.
pub fn highlight_line<'l>(
    line: &'l str,
    stable: &[String],
    dynamic: &[String],
    agents: &[String],
) -> Cow<'l, str> {
    if line.is_empty() {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len() + 32);
    let mut command_pos = true;

    for (start, end, tok) in tokenize(line) {
        let text = &line[start..end];
        match tok {
            Tok::Whitespace => out.push_str(text),
            Tok::Operator => {
                paint(&mut out, OPERATOR, text);
                command_pos = operator_resets_command(text);
            }
            Tok::Str { closed } => {
                paint(&mut out, if closed { STRING_OK } else { STRING_BAD }, text);
                command_pos = false;
            }
            Tok::Word => {
                command_pos = paint_word(&mut out, text, command_pos, stable, dynamic, agents);
            }
        }
    }
    Cow::Owned(out)
}

/// Colour one word; returns the next `command_pos` state.
fn paint_word(
    out: &mut String,
    word: &str,
    command_pos: bool,
    stable: &[String],
    dynamic: &[String],
    agents: &[String],
) -> bool {
    if let Some(name) = word.strip_prefix('@') {
        let colour = if !name.is_empty() && agents.iter().any(|a| a == name) {
            AGENT
        } else {
            AGENT_UNKNOWN
        };
        paint(out, colour, word);
        return false;
    }
    if command_pos {
        if is_namespace(word) {
            paint(out, NAMESPACE, word);
            return true; // the following word is the real command
        }
        let colour = if is_known_command(stable, dynamic, word) {
            CMD_VALID
        } else {
            CMD_UNKNOWN
        };
        paint(out, colour, word);
        return false;
    }
    // Argument position.
    if word.starts_with('-') {
        paint(out, FLAG, word);
    } else {
        out.push_str(word);
    }
    false
}

fn paint(out: &mut String, colour: &str, text: &str) {
    out.push_str(colour);
    out.push_str(text);
    out.push_str(RESET);
}

/// Exact-membership test: the token is a known command if it is in the
/// stable set (PATH/builtins/registry) **or** the dynamic set (aliases).
pub fn is_known_command(stable: &[String], dynamic: &[String], token: &str) -> bool {
    stable.binary_search_by(|c| c.as_str().cmp(token)).is_ok()
        || dynamic.binary_search_by(|c| c.as_str().cmp(token)).is_ok()
}

/// The alphabetically-first command that is a *proper* extension of `prefix`
/// (starts with it and is strictly longer). Requires `sorted` to be sorted.
/// O(log n). Used as the no-frequency-data fallback.
pub fn best_prefix_match<'a>(sorted: &'a [String], prefix: &str) -> Option<&'a str> {
    let idx = sorted.partition_point(|c| c.as_str() <= prefix);
    match sorted.get(idx) {
        Some(c) if c.starts_with(prefix) => Some(c.as_str()),
        _ => None,
    }
}

/// Prefers the highest-frequency previously-run command that extends the
/// prefix (so `g` → `git`, not `gcc`); ties break alphabetically. Falls back
/// to the alphabetically-first match in the stable then dynamic sets when no
/// frequency data applies. Frequency scan is O(freq map size), which is small
/// (only commands actually executed) — cheap enough for the keystroke path.
pub fn best_command_hint(
    stable: &[String],
    dynamic: &[String],
    freq: &HashMap<String, f64>,
    prefix: &str,
) -> Option<String> {
    let mut best: Option<(&str, f64)> = None;
    for (cmd, &weight) in freq {
        if cmd.len() <= prefix.len() || !cmd.starts_with(prefix) {
            continue;
        }
        let better = match best {
            None => true,
            Some((bc, bw)) => match weight.total_cmp(&bw) {
                Ordering::Greater => true,
                Ordering::Equal => cmd.as_str() < bc,
                Ordering::Less => false,
            },
        };
        if better {
            best = Some((cmd.as_str(), weight));
        }
    }
    if let Some((cmd, _)) = best {
        return Some(cmd.to_string());
    }
    best_prefix_match(stable, prefix)
        .or_else(|| best_prefix_match(dynamic, prefix))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip ANSI SGR sequences (`\x1b[…m`) so we can assert width/text.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for n in chars.by_ref() {
                    if n == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn cmds(list: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    }

    const NO_DYN: &[String] = &[];

    #[test]
    fn width_preserved_text_unchanged() {
        let commands = cmds(&["ls", "grep"]);
        let agents = vec!["faye".to_string()];
        for line in [
            "ls -la",
            "lzz foo",
            "ls | grep x",
            "@faye fix it",
            "echo \"open",
            "ork ls",
            // multibyte: tokenizer must slice on char boundaries, never panic.
            "ls café/ 日本",
            "echo 'naïve façade'",
            "grep → x",
        ] {
            let h = highlight_line(line, &commands, NO_DYN, &agents);
            assert_eq!(strip_ansi(&h), line, "text must be unchanged for {line:?}");
        }
    }

    #[test]
    fn valid_vs_unknown_command() {
        let commands = cmds(&["ls"]);
        let agents: Vec<String> = vec![];
        assert!(highlight_line("ls -la", &commands, NO_DYN, &agents).contains(CMD_VALID));
        assert!(highlight_line("lzz foo", &commands, NO_DYN, &agents).contains(CMD_UNKNOWN));
    }

    #[test]
    fn alias_in_dynamic_set_is_valid() {
        // `g` is not in the stable set but is a brush alias (dynamic) ⇒ valid.
        let stable = cmds(&["git"]);
        let dynamic = cmds(&["g"]);
        let agents: Vec<String> = vec![];
        assert!(highlight_line("g status", &stable, &dynamic, &agents).contains(CMD_VALID));
        assert!(is_known_command(&stable, &dynamic, "g"));
        assert!(!is_known_command(&stable, &dynamic, "nope"));
    }

    #[test]
    fn flags_and_operators_and_strings() {
        let commands = cmds(&["ls", "grep"]);
        let agents: Vec<String> = vec![];
        let h = highlight_line("ls -la | grep x", &commands, NO_DYN, &agents);
        assert!(h.contains(FLAG), "flag coloured");
        assert!(h.contains(OPERATOR), "pipe coloured");

        let unclosed = highlight_line("echo \"open", &commands, NO_DYN, &agents);
        assert!(unclosed.contains(STRING_BAD), "unclosed quote flagged");
        let closed = highlight_line("echo \"shut\"", &commands, NO_DYN, &agents);
        assert!(closed.contains(STRING_OK), "closed quote coloured");
    }

    #[test]
    fn agent_known_and_unknown() {
        let commands: Vec<String> = vec![];
        let agents = vec!["faye".to_string()];
        assert!(highlight_line("@faye go", &commands, NO_DYN, &agents).contains(AGENT));
        assert!(highlight_line("@nope go", &commands, NO_DYN, &agents).contains(AGENT_UNKNOWN));
    }

    #[test]
    fn namespace_then_command() {
        let commands = cmds(&["ls"]);
        let agents: Vec<String> = vec![];
        // `ork` is namespace (blue); the following `ls` is the command (green).
        let h = highlight_line("ork ls", &commands, NO_DYN, &agents);
        assert!(h.contains(NAMESPACE));
        assert!(h.contains(CMD_VALID));
    }

    #[test]
    fn command_after_pipe_is_highlighted() {
        let commands = cmds(&["ls", "grep"]);
        let agents: Vec<String> = vec![];
        // both `ls` and `grep` are at command positions ⇒ both valid-coloured.
        let h = highlight_line("ls | grep x", &commands, NO_DYN, &agents);
        let greens = h.matches(CMD_VALID).count();
        assert_eq!(greens, 2, "command after | is also validated");
    }

    #[test]
    fn best_prefix_match_finds_completion() {
        let commands = cmds(&["ls", "lsattr", "lsof", "where", "whoami"]);
        assert_eq!(best_prefix_match(&commands, "wher"), Some("where"));
        // exact match present but a longer completion exists.
        assert_eq!(best_prefix_match(&commands, "ls"), Some("lsattr"));
        // no completion.
        assert_eq!(best_prefix_match(&commands, "zzz"), None);
    }

    #[test]
    fn best_command_hint_prefers_frequency() {
        // F4: git run 100×, gcc 1× — "g" must suggest git, not the
        // alphabetically-first gcc.
        let stable = cmds(&["gcc", "git", "grep"]);
        let mut freq = HashMap::new();
        freq.insert("git".to_string(), 100.0);
        freq.insert("gcc".to_string(), 1.0);
        assert_eq!(
            best_command_hint(&stable, NO_DYN, &freq, "g").as_deref(),
            Some("git")
        );
    }

    #[test]
    fn best_command_hint_falls_back_to_alphabetical() {
        // No frequency data → alphabetically-first proper completion.
        let stable = cmds(&["gcc", "git", "grep"]);
        let freq = HashMap::new();
        assert_eq!(
            best_command_hint(&stable, NO_DYN, &freq, "g").as_deref(),
            Some("gcc")
        );
    }
}
