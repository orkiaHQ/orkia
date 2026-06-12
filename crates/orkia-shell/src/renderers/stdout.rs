// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Non-interactive / piped renderer.
//!
//! Used when stdin/stdout aren't a TTY (`echo "ls" | orkia`) or when the
//! caller explicitly passes `--no-tui`. No prompt is printed; all output
//! goes to stdout/stderr as ANSI-coloured text.
//!
//! Distinct from [`super::shell_mode::ShellModeRenderer`] only in that
//! it never paints a prompt — useful for CI and scripts where the prompt
//! noise would pollute captured output.

use std::io::{self, BufRead, IsTerminal, Write};

use orkia_shell_types::decision::BlockContent;
use orkia_shell_types::renderer::{PromptContext, RenderEvent, ShellRenderer};

use super::{no_color_env, plain_output, write_block};

pub struct StdoutRenderer {
    line_buf: String,
    /// When set, [`publish`](StdoutRenderer::publish) drops every event.
    /// Armed by [`mute`](ShellRenderer::mute) once a detached runtime's
    muted: bool,
    /// output when stdout is not a terminal or `NO_COLOR` is set.
    plain: bool,
}

impl Default for StdoutRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl StdoutRenderer {
    pub fn new() -> Self {
        Self {
            line_buf: String::new(),
            muted: false,
            plain: plain_output(io::stdout().is_terminal(), no_color_env()),
        }
    }
}

impl ShellRenderer for StdoutRenderer {
    fn mute(&mut self) {
        self.muted = true;
    }

    fn publish(&mut self, event: RenderEvent) {
        if self.muted {
            return;
        }
        match event {
            RenderEvent::Block(block) => {
                let mut sink: Box<dyn Write> = match &block {
                    BlockContent::Error(_) | BlockContent::SystemInfo(_) => {
                        Box::new(io::stderr().lock())
                    }
                    _ => Box::new(io::stdout().lock()),
                };
                let _ = write_block(&mut sink, &block, self.plain);
            }
            RenderEvent::RoutingInfo {
                agent,
                confidence,
                reason,
            } => {
                let mut err = io::stderr().lock();
                let _ = writeln!(
                    err,
                    "  \x1b[35m▸\x1b[0m routed to \x1b[1m{agent}\x1b[0m \x1b[90m({reason}, {pct:.0}%)\x1b[0m",
                    pct = confidence * 100.0
                );
            }
            RenderEvent::Welcome(info) => {
                let mut err = io::stderr().lock();
                let _ = writeln!(err, "\n  \x1b[35m⬡ orkia\x1b[0m v{}\n", info.version);
            }
            RenderEvent::JobUpdate(event) => {
                // One owner for job-notification formatting (and the id=0
                // SIGCHLD sentinel suppression): the shell-mode renderer.
                super::shell_mode::publish_job_update(&event);
            }
            RenderEvent::Prompt(_)
            | RenderEvent::JobsSnapshot(_)
            | RenderEvent::WorkspaceSnapshot(_)
            | RenderEvent::TeamSnapshot(_)
            | RenderEvent::CurrentTeamChanged { .. } => {}
        }
    }

    fn read_line(&mut self, _ctx: &PromptContext) -> Option<String> {
        self.line_buf.clear();
        let stdin = io::stdin();
        let mut handle = stdin.lock();
        match handle.read_line(&mut self.line_buf) {
            Ok(0) => None,
            Ok(_) => Some(self.line_buf.clone()),
            Err(_) => None,
        }
    }
}
