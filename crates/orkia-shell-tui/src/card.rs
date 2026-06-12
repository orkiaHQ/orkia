// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! A [`CommandCard`] groups a typed command with the blocks it produced,
//! so the main pane can render each command + its output as one Warp-style
//! card (header line + bordered-feel body). Grouping is renderer-side: the
//! REPL contract is unchanged. `read_line` opens a card when the user
//! submits a line and closes it when the next prompt appears; `publish`
//! appends blocks to the open card.

use orkia_shell_types::BlockContent;
use std::time::Instant;

pub struct CommandCard {
    /// The command line that opened this card. `None` is the session
    /// preamble — briefing / system output that belongs to no command.
    pub command: Option<String>,
    pub blocks: Vec<BlockContent>,
    /// Set when the card opens (command submitted).
    pub started: Option<Instant>,
    /// Set when the next prompt appears (output is done).
    pub finished: Option<Instant>,
    /// True once any `Error` block lands in the card, or a non-zero exit
    /// is reported via `note_exit`.
    pub failed: bool,
    /// The command's real exit code, when a shell command reported one.
    pub exit_code: Option<i32>,
    /// When true, the body is folded away — only the header shows.
    pub collapsed: bool,
}

impl CommandCard {
    /// The leading, command-less card holding briefing / system output.
    pub fn preamble() -> Self {
        Self {
            command: None,
            blocks: Vec::new(),
            started: None,
            finished: None,
            failed: false,
            exit_code: None,
            collapsed: false,
        }
    }

    pub fn command(line: String, started: Instant) -> Self {
        Self {
            command: Some(line),
            blocks: Vec::new(),
            started: Some(started),
            finished: None,
            failed: false,
            exit_code: None,
            collapsed: false,
        }
    }

    pub fn push_block(&mut self, block: BlockContent) {
        if matches!(block, BlockContent::Error(_)) {
            self.failed = true;
        }
        self.blocks.push(block);
    }

    /// Stamp the finish time once, and only for real command cards.
    pub fn close(&mut self, at: Instant) {
        if self.command.is_some() && self.finished.is_none() {
            self.finished = Some(at);
        }
    }
}
