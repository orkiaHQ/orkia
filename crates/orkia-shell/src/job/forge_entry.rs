// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Process tracking for Forge viewer instances.
//!
//! The PTY-backed [`super::entry::JobEntry`] is the wrong shape for the
//! Forge viewer: it's a GUI process with no PTY, no terminal grid, no
//! attach pump. Rather than turn `JobEntry`'s `TerminalEngine` field
//! into an `Option` (which would scatter `Option`-handling across every
//! attach/spawn/hook path), we keep a parallel collection of bare
//! children. The two collections share the same `next_id` sequence
//! through [`super::JobController`] so `kill`/`stop`/`fg`/`ps` keep
//! working uniformly on `JobId` regardless of the kind underneath.

use std::process::Child;
use std::time::Instant;

use orkia_shell_types::job::{JobId, JobState};

use crate::error::ShellError;

pub struct ForgeJobEntry {
    pub id: JobId,
    pub app_name: String,
    pub child: Child,
    pub started_at: Instant,
    pub state: JobState,
    /// Display label for `ps` / `jobs` output. We seed it with the app
    /// name (matches what `JobKind::ForgeApp { app_name }` already
    /// renders) so the user sees the same string in every surface.
    pub label: String,
}

impl ForgeJobEntry {
    pub fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }

    pub fn try_exit_code(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(0)),
            Ok(None) => None,
            // A reap error (ECHILD etc.) means the process is gone but
            // we missed it. Treat as exit 0 so the entry can be cleaned
            // up rather than getting stuck in Running forever.
            Err(_) => Some(0),
        }
    }

    #[cfg(unix)]
    pub fn signal(&self, sig: i32) -> Result<(), ShellError> {
        let pid = self.child.id() as i32;
        // SAFETY: kill(2) with a positive pid sends `sig` to that pid.
        // The pid here came from our own spawn so we know we own it
        // (or the OS will return ESRCH which we surface as an error).
        let rc = unsafe { libc::kill(pid, sig) };
        if rc == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            Err(ShellError::Other(format!("kill({pid}, {sig}): {err}")))
        }
    }

    #[cfg(not(unix))]
    pub fn signal(&self, _sig: i32) -> Result<(), ShellError> {
        Err(ShellError::Other(
            "signalling forge-app jobs is unix-only in V0".into(),
        ))
    }
}
