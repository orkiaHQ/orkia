// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! End-to-end test harness for the Orkia shell.
//!
//! The harness drives the real `orkia` binary inside a PTY, owns a
//! hermetic `ORKIA_HOME` (tempdir), tails the on-disk journal, and
//! ships a scripted TUI "fake agent" binary that conforms to the same
//! contract real agents do (raw termios, OSC-133, hook bridge calls).
//!
//! It depends only on **stable observable surfaces**:
//!   * filesystem layout under `$HOME/.orkia/`
//!   * the `journal.jsonl` NDJSON stream
//!   * PTY byte I/O
//!   * the `orkia bridge --source <name>` CLI
//!
//! No internal `orkia-*` crate is imported. The harness is therefore
//! resilient to internal refactors in the shell while exercising every
//! externally-visible behaviour.
//!
//! See `tests/smoke.rs` for an end-to-end walk-through.

pub mod agent;
pub mod journal;
pub mod orkia_proc;
pub mod pty;
pub mod sandbox;
pub mod script;
pub mod seal;
pub mod wait;

pub use agent::{ScriptedAgent, ScriptedAgentBuilder};
pub use journal::{JournalEvent, JournalTail};
pub use orkia_proc::{OrkiaBinary, OrkiaProcess, resolve_or_skip};
pub use pty::PtyDriver;
pub use sandbox::OrkiaSandbox;
pub use script::{AgentScript, ScriptStep};
pub use seal::{SealEvent, SealTail};
pub use wait::{WaitError, wait_for};

/// Convenience prelude — `use orkia_test_harness::prelude::*;`.
pub mod prelude {
    pub use crate::agent::{ScriptedAgent, ScriptedAgentBuilder};
    pub use crate::journal::{JournalEvent, JournalTail};
    pub use crate::orkia_proc::{OrkiaBinary, OrkiaProcess, resolve_or_skip};
    pub use crate::pty::PtyDriver;
    pub use crate::sandbox::OrkiaSandbox;
    pub use crate::script::{AgentScript, ScriptStep};
    pub use crate::seal::{SealEvent, SealTail};
    pub use crate::wait::{WaitError, wait_for};
}
