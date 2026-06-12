// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Native agent runtime — the Orkia-owned LLM loop.
//!
//! An agent with `[runtime] type = "native"` has no vendor CLI and no
//! PTY: the shell itself drives the transcript, executes tools behind
//! the policy gate, and emits journal/SEAL/final-response directly.
//! The model runs via the kernel relay (`kernel.v1.llm.complete`);
//! BYO provider keys live kernel-side, never in this process.
//!
//! This is NOT vendor print-mode (non-negotiable #5 still stands for
//! vendor CLIs): there is no one-shot subprocess. The session is a
//! long-lived in-process actor — `tell`/`ps`/`kill` and the SEAL
//! chain work like any agent session; only `attach` is refused
//! (there is no terminal to splice).
//!
//! Module map:
//! - [`session`] — the per-session tokio actor (transcript owner).
//! - [`turn`] — completion → tool calls → results loop, bounded.
//! - [`tools`] — `shell` (policy-gated) + `recall_knowledge`.
//! - [`emit`] — journal/SEAL emission (`source: "native"`).

// `emit`/`tools`/`turn` are `pub` for exactly one external consumer:
// `orkia-stage-exec`'s native stage path reuses the turn machine and
// the audit emitter so a pipeline stage and a Solo session share one
// implementation. `session` (the actor) stays crate-internal.
pub mod emit;
pub(crate) mod session;
pub mod tools;
pub mod turn;

/// Control messages for a running native session, sent by the REPL
/// (`tell`, `kill`, dispatch reuse) over the entry's inbound channel.
#[derive(Debug)]
pub enum NativeSessionMsg {
    /// A user body to run as the next turn.
    User(String),
    /// Stop the session; the in-flight kernel call (if any) is
    /// dropped, nothing further executes or publishes.
    Kill,
}
