// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Cross-crate payload describing an attached PTY-backed job. The REPL
//! constructs one of these from a `JobEntry` and hands it to the renderer's
//! `attach_job`. The renderer keeps the handle for the duration of the
//! attached session, using it to read the PTY grid, send keystrokes, and
//! query liveness.

use std::sync::Arc;
use std::time::Instant;

use orkia_terminal_core::{ScreenTerm, SharedDims, SharedMaster, SharedWriter, Wake, WakeRx};

use crate::job::JobId;

/// Non-blocking liveness probe — returns `false` once the child process has
/// exited. Boxed so the trait crate doesn't need to know about `TerminalEngine`.
pub type LivenessProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// Everything the renderer needs to drive an attached PTY session.
///
/// All fields except `wake_rx` are cheap `Arc` clones from the engine; the
/// engine itself stays in the job registry. `wake_rx` is the one consumer of
/// engine repaint events and is moved out of the engine at attach time.
pub struct AttachedHandle {
    pub job_id: JobId,
    pub label: String,
    pub pid: Option<u32>,
    pub started_at: Instant,
    pub seal_active: bool,

    pub writer: SharedWriter,
    pub screen: ScreenTerm,
    /// `None` for engines created via `TerminalEngine::adopt_master`
    /// (resize is handled internally via raw-fd ioctl). Agent jobs
    /// spawned via `TerminalEngine::start` always carry `Some`.
    pub master: Option<SharedMaster>,
    pub dims: SharedDims,
    pub wake: Wake,
    pub wake_rx: Option<WakeRx>,
    pub is_alive: LivenessProbe,
}

/// Outcome of a `drive_attached` call — tells the REPL why the renderer
/// returned control. `Unsupported` is the default for renderers that don't
/// implement widget-mode attach (e.g. the stdout renderer).
#[derive(Debug, Clone, Copy)]
pub enum AttachedOutcome {
    /// User pressed Ctrl-Z.
    Detached,
    /// Child process exited on its own; renderer auto-detached.
    ChildExited,
    /// Renderer does not support widget-mode attach.
    Unsupported,
}
