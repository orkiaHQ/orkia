// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! A minimal, self-contained line editor for the TUI prompt. The TUI can't
//! reuse `rustyline` (it owns the terminal, conflicting with ratatui's
//! alt-screen + raw mode), so this provides cursor movement, the common
//! kill bindings, and in-session command history.
//!
//! The buffer is stored as `Vec<char>` so the cursor is a simple char
//! index — correct for multi-byte input without byte-boundary juggling.

#[derive(Default)]
pub struct Input {
    chars: Vec<char>,
    /// Cursor position in `0..=chars.len()`.
    cursor: usize,
    /// Submitted commands, oldest first.
    history: Vec<String>,
    /// `Some(i)` while browsing `history[i]`; `None` while editing live.
    hist_pos: Option<usize>,
    /// Live buffer stashed when history browsing begins, restored on exit.
    stash: String,
}

impl Input {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Reset the editable buffer (keeps history). Called between prompts.
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.hist_pos = None;
        self.stash.clear();
    }

    // ── editing ────────────────────────────────────────────────────────
    pub fn insert(&mut self, c: char) {
        self.detach_history();
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        self.detach_history();
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        self.detach_history();
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    /// Ctrl-W: delete the word (and trailing run of spaces) before the cursor.
    pub fn kill_word_back(&mut self) {
        self.detach_history();
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.chars.drain(i..self.cursor);
        self.cursor = i;
    }

    /// Ctrl-U: delete everything before the cursor.
    pub fn kill_to_start(&mut self) {
        self.detach_history();
        self.chars.drain(0..self.cursor);
        self.cursor = 0;
    }

    // ── cursor ─────────────────────────────────────────────────────────
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    // ── history ────────────────────────────────────────────────────────
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.hist_pos {
            None => {
                self.stash = self.text();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.hist_pos = Some(idx);
        self.set_text(&self.history[idx].clone());
    }

    pub fn history_next(&mut self) {
        match self.hist_pos {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.hist_pos = Some(i + 1);
                self.set_text(&self.history[i + 1].clone());
            }
            Some(_) => {
                // Past the newest entry — back to the live buffer.
                self.hist_pos = None;
                let s = std::mem::take(&mut self.stash);
                self.set_text(&s);
            }
        }
    }

    /// Submit the line: record it in history (no consecutive duplicates)
    /// and reset the buffer. Returns the submitted text.
    pub fn submit(&mut self) -> String {
        let line = self.text();
        if !line.trim().is_empty() && self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        self.clear();
        line
    }

    // ── internals ──────────────────────────────────────────────────────
    fn set_text(&mut self, s: &str) {
        self.chars = s.chars().collect();
        self.cursor = self.chars.len();
    }

    /// Any edit while browsing history copies that entry into the live
    /// buffer (matches shell behaviour).
    fn detach_history(&mut self) {
        self.hist_pos = None;
        self.stash.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Input {
        let mut i = Input::default();
        for c in s.chars() {
            i.insert(c);
        }
        i
    }

    #[test]
    fn insert_and_cursor_track() {
        let i = typed("abc");
        assert_eq!(i.text(), "abc");
        assert_eq!(i.cursor(), 3);
    }

    #[test]
    fn insert_mid_buffer() {
        let mut i = typed("ac");
        i.left();
        i.insert('b');
        assert_eq!(i.text(), "abc");
        assert_eq!(i.cursor(), 2);
    }

    #[test]
    fn backspace_and_delete_at_cursor() {
        let mut i = typed("abc");
        i.home();
        i.delete();
        assert_eq!(i.text(), "bc");
        i.end();
        i.backspace();
        assert_eq!(i.text(), "b");
    }

    #[test]
    fn kill_word_back_eats_word_and_spaces() {
        let mut i = typed("foo bar ");
        i.kill_word_back();
        assert_eq!(i.text(), "foo ");
        i.kill_word_back();
        assert_eq!(i.text(), "");
    }

    #[test]
    fn kill_to_start() {
        let mut i = typed("hello world");
        i.left();
        i.left();
        i.kill_to_start();
        assert_eq!(i.text(), "ld");
    }

    #[test]
    fn home_end_clamp() {
        let mut i = typed("ab");
        i.home();
        i.left();
        assert_eq!(i.cursor(), 0);
        i.end();
        i.right();
        assert_eq!(i.cursor(), 2);
    }

    #[test]
    fn submit_records_history_no_consecutive_dupes() {
        let mut i = Input::default();
        for c in "ls".chars() {
            i.insert(c);
        }
        assert_eq!(i.submit(), "ls");
        for c in "ls".chars() {
            i.insert(c);
        }
        let _ = i.submit();
        assert_eq!(i.history.len(), 1);
    }

    #[test]
    fn history_prev_next_round_trip() {
        let mut i = Input::default();
        for cmd in ["one", "two"] {
            for c in cmd.chars() {
                i.insert(c);
            }
            let _ = i.submit();
        }
        for c in "edit".chars() {
            i.insert(c);
        }
        i.history_prev();
        assert_eq!(i.text(), "two");
        i.history_prev();
        assert_eq!(i.text(), "one");
        i.history_next();
        assert_eq!(i.text(), "two");
        i.history_next(); // past newest → live buffer restored
        assert_eq!(i.text(), "edit");
    }

    #[test]
    fn editing_recalled_entry_detaches() {
        let mut i = Input::default();
        for c in "alpha".chars() {
            i.insert(c);
        }
        let _ = i.submit();
        i.history_prev();
        assert_eq!(i.text(), "alpha");
        i.backspace();
        i.history_next(); // no live stash to restore — stays put
        assert_eq!(i.text(), "alph");
    }
}
