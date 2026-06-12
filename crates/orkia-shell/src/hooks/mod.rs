// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Provider-specific hook configurations.
//!
//! When orkia spawns an agent CLI (Claude Code, Codex, Gemini), it
//! writes a per-project hook config that points each hook at
//! `orkia bridge --source <name>`. The bridge then forwards the
//! payload to the journal socket. See `journal/listener.rs` and
//! `bins/orkia/src/bridge.rs` for the receiving end.
//!
//! Configs are written non-destructively: existing keys in the
//! project-scoped settings file are preserved; only the `hooks`
//! key is replaced. V1 does not clean up on job exit — that is a
//! deliberate non-goal so users can inspect hooks after a run.

pub mod providers;

pub use providers::{
    claude_hooks_config, codex_hooks_config, gemini_hooks_config, install_hooks,
    merge_hooks_config, merge_mcp_servers, write_hooks_array,
};
