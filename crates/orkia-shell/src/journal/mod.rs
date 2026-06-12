// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Unified event journal.
//!
//! Every event in orkia — agent hooks (Claude / Codex / Gemini),
//! approvals, job lifecycle, shell SEAL, tells — flows into the same
//! envelope ([`JournalEnvelope`]) and lands in the same store
//! ([`JournalStore`]). External writers connect to a Unix socket
//! served by [`JournalListener`] and send NDJSON; in-process emitters
//! call [`emit`] directly.
//!

pub mod query;
pub mod store;

// The envelope types are owned by `orkia-shell-types` so external
// consumers (the bridge, `orkia-final-response`, future Team crates)
// can depend on them without pulling in the full shell. We re-export
// here so internal call sites can keep `use crate::journal::types::…`
// imports working unchanged.
pub use orkia_shell_types::journal::types;

// The hub (orkia.sock listener + bus + disk-backed subscribers) was
// extracted into `orkia-journal-hub` so the pty-daemon can host the same
// here so existing `crate::journal::…` call sites compile unchanged.
pub use orkia_journal_hub::{
    HookRouter, JournalHub, JournalHubConfig, JournalListener, LiveJournalHandlers, McpDispatcher,
    McpReply, event_summary, event_type_label, normalize_event_name, normalize_hook_value,
    notification_for, query_row, try_recover_hook_line,
};
pub use orkia_shell_types::journal::{EventType, JournalEnvelope, JournalFilter};
pub use query::{ParsedJournalArgs, help_text as journal_help_text};
pub use store::JournalStore;
