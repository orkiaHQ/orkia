// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Process-agnostic journal hub.
//!
//! Owns the `orkia.sock` listener, the broadcast bus, and the
//! disk-backed subscribers (FinalResponseService stop-hook + disk tee).
//! Extracted from `orkia-shell` so the pty-daemon can host the same hub
//! that the REPL does — the prerequisite for letting agent hooks / SEAL
//!
//! The envelope types live in `orkia-shell-types`; the concrete event
//! router and MCP dispatcher are injected via the [`HookRouter`] and
//! [`McpDispatcher`] traits so this crate carries no dependency edge back
//! into `orkia-shell`.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod error;
mod hub;
mod listener;
mod normalize;
mod notifications;
mod router;

pub use error::JournalHubError;
pub use hub::{JournalHub, JournalHubConfig};
pub use listener::{JournalListener, LiveJournalHandlers, McpDispatcher, McpReply};
pub use normalize::{normalize_event_name, normalize_hook_value, try_recover_hook_line};
pub use notifications::{event_summary, event_type_label, notification_for, query_row};
pub use router::HookRouter;
