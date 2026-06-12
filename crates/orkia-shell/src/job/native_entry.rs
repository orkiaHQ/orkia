// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Job tracking for native (non-PTY) agent sessions.
//!
//! Same shape rationale as [`super::forge_entry::ForgeJobEntry`]: a
//! native session has no `TerminalEngine`, no child process, no PTY —
//! it is an in-process tokio actor. Rather than make `JobEntry`'s
//! engine optional, we keep a third parallel collection sharing the
//! controller's `next_id`, so `ps`/`kill`/`tell` stay one unified
//! `JobId` namespace.

use std::time::Instant;

use orkia_shell_types::job::{JobId, JobState};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::native::NativeSessionMsg;

pub struct NativeJobEntry {
    pub id: JobId,
    pub agent_name: String,
    /// Stable per-instance UUID, mirrors `JobKind::Agent { agent_id }`.
    pub agent_id: Uuid,
    /// Control channel into the session actor (`tell` → `User`,
    /// `kill` → `Kill`).
    pub inbound: mpsc::UnboundedSender<NativeSessionMsg>,
    /// Fired once by the actor when it ends; polled by `reap()`.
    pub exit_rx: oneshot::Receiver<i32>,
    pub started_at: Instant,
    pub state: JobState,
    pub label: String,
}

impl NativeJobEntry {
    /// Non-blocking exit poll, the actor analog of a `try_wait`. A
    /// dropped sender without a value means the actor died without
    /// reporting (panic/abort) — surfaced as exit 1, fail-closed.
    pub fn try_exit_code(&mut self) -> Option<i32> {
        match self.exit_rx.try_recv() {
            Ok(code) => Some(code),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => Some(1),
        }
    }
}
