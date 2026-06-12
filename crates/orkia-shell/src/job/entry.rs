// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use orkia_shell_types::AttachedHandle;
use orkia_shell_types::job::{JobId, JobKind, JobState};
use orkia_terminal_core::TerminalEngine;

use crate::error::ShellError;
use crate::job::lifecycle::JobLifecycleHook;

/// Where a bound agent session's per-turn response text is delivered
#[derive(Debug, Clone)]
pub enum SinkTarget {
    /// `@agent … | <cmd>` — pipe the text into `sink_cmd` (run via `sh -c` once
    /// per completed turn, fed the text on stdin). `cwd`/`env` are snapshot from
    /// the shell engine at bind time so the command runs where the user expects
    /// and with their `export`s.
    Command {
        sink_cmd: String,
        cwd: std::path::PathBuf,
        env: Vec<(String, String)>,
    },
    /// Standalone `@agent … --once` (no `|`) — write the clean per-turn text as
    /// raw bytes to the REPL's stdout (the originating terminal). The sink is the
    /// terminal.
    Terminal,
}

/// response text is delivered to `target`. `once` kills the session after a
/// single turn (the only lifetime for a `Terminal` target — standalone `--once`).
#[derive(Debug, Clone)]
pub struct SinkRecipe {
    pub target: SinkTarget,
    pub once: bool,
}

pub struct JobEntry {
    pub id: JobId,
    pub kind: JobKind,
    pub state: JobState,
    pub engine: TerminalEngine,
    pub started_at: Instant,
    pub label: String,
    /// Attachment-driven lifecycle hooks. `on_spawn` fired once
    /// by [`crate::job::spawn::spawn`] right after the entry was
    /// pushed; `on_complete` fired by
    /// [`crate::job::JobController::dispatch_on_complete`] when
    /// the `Completed` event drains. Cleared on entry drop so any
    /// per-hook resources release deterministically.
    pub lifecycle_hooks: Vec<Arc<dyn JobLifecycleHook>>,
    /// per-turn responses are piped into a downstream command. Set/cleared on
    /// the REPL thread (single owner); the actual write runs on a detached
    /// task off the journal drain.
    pub sink_recipe: Option<SinkRecipe>,
}

impl JobEntry {
    pub fn pid(&self) -> Option<u32> {
        self.engine.child_id()
    }

    pub fn is_alive(&self) -> bool {
        self.engine.try_wait().ok().flatten().is_none()
    }

    pub fn try_exit_code(&self) -> Option<i32> {
        self.engine.try_wait().ok().flatten()
    }

    #[cfg(unix)]
    pub fn signal(&self, sig: i32) -> Result<(), ShellError> {
        self.engine
            .signal(sig)
            .map_err(|e| ShellError::Other(e.to_string()))
    }

    pub fn write_stdin(&self, data: &[u8]) -> Result<(), ShellError> {
        let writer = self.engine.writer();
        let mut w = writer.lock();
        w.write_all(data).map_err(ShellError::Io)?;
        w.flush().map_err(ShellError::Io)
    }

    /// Build the cross-crate payload the TUI renderer needs to drive an
    /// attached widget-mode session against this job's PTY.
    pub fn build_attached_handle(&mut self, seal_active: bool) -> AttachedHandle {
        AttachedHandle {
            job_id: self.id,
            label: self.label.clone(),
            pid: self.engine.child_id(),
            started_at: self.started_at,
            seal_active,
            writer: self.engine.writer(),
            screen: self.engine.screen(),
            master: self.engine.master(),
            dims: self.engine.dims(),
            wake: self.engine.wake(),
            wake_rx: self.engine.take_wake_rx(),
            is_alive: self.engine.liveness_probe(),
        }
    }
}
