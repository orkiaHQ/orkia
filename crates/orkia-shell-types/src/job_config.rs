// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Leaf configuration enums shared between the `orkia-shell` job
//! model and any future crate that needs to describe a job spawn
//! without pulling the full `JobConfig` / `Attachment` types (which
//! live in `orkia-shell` because they reference shell-internal types
//! like `AgentContext` and `Provider`).
//!

/// Where the child process's stdin comes from.
#[derive(Debug, Clone, Default)]
pub enum StdinSource {
    /// Standard agent / interactive job: stdin is the PTY slave.
    #[default]
    Pty,
    /// Inherit the parent's stdin (rare — used for foreground
    /// processes that should read the user's keyboard directly).
    Inherit,
    /// Discard the child's stdin (`/dev/null`).
    Null,
    /// Use the PTY slave, but write these bytes into the master
    /// immediately after spawn (e.g. the agent's intent body).
    InitialBytes(Vec<u8>),
}

/// How the child should relate to the parent's process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessGroupMode {
    /// `setsid` — start a brand new session. Used for everything
    /// today: agents, background shell jobs, RFC delegates.
    #[default]
    NewSession,
    /// Inherit the parent's session. Reserved for future
    /// foreground job-control work (`fg` / `bg` requires the child
    /// to share the controlling tty with the shell).
    Inherit,
}
