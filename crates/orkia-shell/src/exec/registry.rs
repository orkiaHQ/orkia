// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Replaces the two static `match` blocks in `repl.rs` with a `HashMap`
//! lookup. Registering a command is a single `register` call; it never
//! touches the REPL core.

use std::collections::HashMap;
use std::sync::Arc;

use orkia_shell_types::Signature;
use orkia_shell_types::exec::command::Command;

/// A name-keyed registry of typed commands.
#[derive(Default, Clone)]
pub struct CommandRegistry {
    commands: HashMap<String, Arc<dyn Command>>,
}

impl CommandRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry populated with the V1 pilot commands. `ps` is added by
    /// its migration step; here we register the structured pilots.
    pub fn with_pilots() -> Self {
        use crate::exec::commands::{
            attention::Attention, briefing::Briefing, first::First, from_json::FromJson,
            help::Help, history::HistoryCmd, jobs::Jobs, journal::Journal, log::Log, ls::Ls,
            plan::Plan, ps::Ps, route::Route, sort_by::SortBy, version::Version, where_cmd::Where,
            whoami::Whoami,
        };
        let mut registry = Self::new();
        registry.register(Arc::new(Ls));
        registry.register(Arc::new(Where));
        registry.register(Arc::new(First));
        registry.register(Arc::new(SortBy));
        registry.register(Arc::new(FromJson));
        registry.register(Arc::new(Ps));
        // migrated to native Commands. `try_parse_exec` routes these names here;
        // their legacy `BuiltinCmd` arms were deleted in Vague 5.
        registry.register(Arc::new(Help));
        registry.register(Arc::new(Version));
        registry.register(Arc::new(Route));
        registry.register(Arc::new(Briefing));
        // via the enriched `CommandCtx` — `log` (`data_dir` + `jobs`), and
        // `whoami`/`plan` (the `AuthView` service handle).
        registry.register(Arc::new(Log));
        registry.register(Arc::new(Whoami));
        registry.register(Arc::new(Plan));
        // `history` reads the on-disk mirror via `data_dir` and emits a table.
        registry.register(Arc::new(HistoryCmd));
        // `journal` reads the on-disk mirror via `data_dir` and emits a table.
        registry.register(Arc::new(Journal));
        // `jobs` reads the `CommandCtx.jobs` snapshot; `attention` the
        // `CommandCtx.attention` snapshot — both cheap, no new context surface.
        registry.register(Arc::new(Jobs));
        registry.register(Arc::new(Attention));
        registry
    }

    /// Register a command under its signature name. A later registration
    /// with the same name replaces the earlier one.
    pub fn register(&mut self, cmd: Arc<dyn Command>) {
        let name = cmd.signature().name;
        self.commands.insert(name, cmd);
    }

    /// Look up a command by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Command>> {
        self.commands.get(name).cloned()
    }

    /// Whether a command is registered under `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.commands.contains_key(name)
    }

    /// The signature of a registered command, if present.
    pub fn signature(&self, name: &str) -> Option<Signature> {
        self.commands.get(name).map(|c| c.signature())
    }

    /// All registered command names. Used to seed the line-editor snapshot
    /// and complete. Order is unspecified — callers sort if they need it.
    pub fn names(&self) -> Vec<String> {
        self.commands.keys().cloned().collect()
    }
}
