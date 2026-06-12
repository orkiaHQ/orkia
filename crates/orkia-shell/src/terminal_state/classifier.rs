// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Best-effort textual classification of a *detected* prompt.
//!
//! Detection already fired (see `detector.rs`). This module only
//! tries to enrich the notification with WHAT kind of prompt the
//! user is looking at. Returning [`PromptType::Generic`] is a fine
//! outcome — the toast still surfaces, the user can `attach`.
//!
//! No regex; only substring / suffix checks across the current line
//! and recent lines. All patterns are heuristic and intentionally
//! non-exclusive — a real prompt may match several types; first
//! match wins by priority (password > yes/no > choices > shell >
//! continuation).

use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PromptType {
    YesNo,
    MultipleChoice,
    Password,
    ShellPrompt,
    Continuation,
    AltScreenProgram,
    Generic,
}

pub fn classify(
    current_line: &str,
    recent_lines: &VecDeque<String>,
    alt_screen: bool,
) -> PromptType {
    if alt_screen {
        return classify_alt_screen(recent_lines);
    }

    let trimmed = current_line.trim();
    let lower = trimmed.to_ascii_lowercase();

    if is_password_prompt(&lower) || recent_password_hint(recent_lines) {
        return PromptType::Password;
    }
    if has_yn_pattern(trimmed) || any_recent(recent_lines, has_yn_pattern) {
        return PromptType::YesNo;
    }
    if has_choice_indicators(trimmed, recent_lines) {
        return PromptType::MultipleChoice;
    }
    if is_continuation(&lower)
        || any_recent(recent_lines, |l| is_continuation(&l.to_ascii_lowercase()))
    {
        return PromptType::Continuation;
    }
    if is_shell_prompt(trimmed) {
        return PromptType::ShellPrompt;
    }

    PromptType::Generic
}

fn any_recent(recent: &VecDeque<String>, mut p: impl FnMut(&str) -> bool) -> bool {
    recent.iter().any(|l| p(l))
}

fn is_password_prompt(lower_trimmed: &str) -> bool {
    lower_trimmed.ends_with("password:")
        || lower_trimmed.ends_with("passphrase:")
        || lower_trimmed.ends_with("secret:")
        || lower_trimmed.ends_with("token:")
        || lower_trimmed.contains("password for ")
        || lower_trimmed.contains("[sudo] password")
}

fn recent_password_hint(recent: &VecDeque<String>) -> bool {
    recent.iter().any(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("password:") || l.contains("passphrase:") || l.contains("[sudo] password")
    })
}

fn has_yn_pattern(line: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "[Y/n]", "[y/N]", "[y/n]", "[Y/N]", "(yes/no)", "(y/n)", "(Y/N)", "(Y/n)", "(y/N)",
        "yes/no",
    ];
    PATTERNS.iter().any(|p| line.contains(p))
}

fn has_choice_indicators(current: &str, recent: &VecDeque<String>) -> bool {
    if has_choice_in_line(current) {
        return true;
    }
    recent.iter().any(|l| has_choice_in_line(l))
}

fn has_choice_in_line(l: &str) -> bool {
    if l.contains('●') || l.contains('○') || l.contains('◯') {
        return true;
    }
    if l.contains("[ ]") || l.contains("[x]") || l.contains("[X]") {
        return true;
    }
    let trim = l.trim_start();
    // `❯` is a selection marker only when it leads a non-empty
    // option: `❯ 1. Yes`. A `❯` at the end of the line is the
    // user's shell prompt character (orkia, starship, etc.) and
    // belongs to is_shell_prompt — not a choice indicator.
    if trim.starts_with("❯ ") && trim.len() > 3 {
        return true;
    }
    if trim.starts_with('>') && trim.len() > 2 && !trim.ends_with('>') {
        return true;
    }
    if trim.starts_with("→ ") || trim.starts_with("➜ ") {
        return true;
    }
    // Numbered list "  1. Yes" / "1) Yes" — common in claude / npm.
    if trim.starts_with("1.") || trim.starts_with("1)") {
        return true;
    }
    false
}

fn is_continuation(lower: &str) -> bool {
    lower.contains("press enter")
        || lower.contains("press any key")
        || lower.contains("press [enter]")
        || lower.contains("to continue")
        || lower.contains("--more--")
}

fn is_shell_prompt(line: &str) -> bool {
    let end = line.trim_end();
    match end.chars().next_back() {
        Some(last) => matches!(last, '$' | '>' | '❯' | '#' | '%'),
        None => false,
    }
}

fn classify_alt_screen(recent: &VecDeque<String>) -> PromptType {
    if let Some(last) = recent.back() {
        let lower = last.to_ascii_lowercase();
        if lower.contains("-- insert --")
            || lower.contains("-- visual --")
            || lower.contains("-- replace --")
            || lower.contains("-- normal --")
        {
            return PromptType::AltScreenProgram;
        }
        let trimmed = last.trim();
        if trimmed == ":" || trimmed == "(END)" || trimmed.starts_with("--More--") {
            return PromptType::AltScreenProgram;
        }
    }
    PromptType::AltScreenProgram
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recent_from(lines: &[&str]) -> VecDeque<String> {
        lines.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn yn_pattern_in_current_line() {
        let r = recent_from(&[]);
        assert_eq!(classify("Continue? [Y/n]", &r, false), PromptType::YesNo);
    }

    #[test]
    fn radio_buttons_in_recent() {
        let r = recent_from(&["● Yes, I trust this folder", "○ No, exit"]);
        assert_eq!(classify("", &r, false), PromptType::MultipleChoice);
    }

    #[test]
    fn password_takes_priority_over_yn() {
        let r = recent_from(&["Continue? [Y/n]"]);
        assert_eq!(
            classify("Password for root:", &r, false),
            PromptType::Password
        );
    }

    #[test]
    fn shell_prompt_endings() {
        let r = recent_from(&[]);
        assert_eq!(
            classify("user@host ~ %", &r, false),
            PromptType::ShellPrompt
        );
        assert_eq!(classify("⬡ ~ ❯", &r, false), PromptType::ShellPrompt);
        assert_eq!(classify("$", &r, false), PromptType::ShellPrompt);
    }

    #[test]
    fn unclassifiable_falls_back_to_generic() {
        let r = recent_from(&["Welcome to the agent.", "Pick something."]);
        assert_eq!(classify("anything", &r, false), PromptType::Generic);
    }

    #[test]
    fn alt_screen_overrides_text() {
        let r = recent_from(&["-- INSERT --"]);
        assert_eq!(classify("", &r, true), PromptType::AltScreenProgram);
    }

    #[test]
    fn continuation_in_recent() {
        let r = recent_from(&["Press Enter to continue"]);
        assert_eq!(classify("", &r, false), PromptType::Continuation);
    }
}
