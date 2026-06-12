// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! PTY ↔ brush binding.
//!
//! brush writes to whatever FDs are installed in `Shell::open_files` for
//! fds 0/1/2, and external commands inherit those FDs at spawn time
//! (`OpenFile::File` → `Stdio::from(file)`). To route all child output
//! through orkia's terminal-core renderer, we hand brush a PTY slave fd
//! and read the master end ourselves.

use std::os::fd::OwnedFd;

use brush_core::Shell;
use brush_core::openfiles::{OpenFile, OpenFiles};

use crate::error::ShellError;

/// Install a PTY slave fd as brush's stdin/stdout/stderr.
///
/// The caller keeps the master end and feeds it to a `TerminalEngine` (or
/// an equivalent reader). The slave fd is consumed: we clone it three
/// times — one per std stream — so the shell holds all three handles for
/// its lifetime. Closing the last handle would EOF the master and tear
/// down the PTY, so the engine must outlive any consumer of the master.
pub fn bind_pty_to_shell(shell: &mut Shell, slave: OwnedFd) -> Result<(), ShellError> {
    let stdin_file = std::fs::File::from(
        slave
            .try_clone()
            .map_err(|e| ShellError::Other(format!("clone slave for stdin: {e}")))?,
    );
    let stdout_file = std::fs::File::from(
        slave
            .try_clone()
            .map_err(|e| ShellError::Other(format!("clone slave for stdout: {e}")))?,
    );
    let stderr_file = std::fs::File::from(slave);

    let files = shell.open_files_mut();
    files.set_fd(OpenFiles::STDIN_FD, OpenFile::File(stdin_file));
    files.set_fd(OpenFiles::STDOUT_FD, OpenFile::File(stdout_file));
    files.set_fd(OpenFiles::STDERR_FD, OpenFile::File(stderr_file));
    Ok(())
}
