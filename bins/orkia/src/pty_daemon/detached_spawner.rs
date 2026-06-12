// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

//!
//! The main REPL no longer spawns agents in-process via `JobController::spawn`;
//! it spawns them in the `pty_daemon` so they survive a REPL exit. This impl of
//! [`DetachedSpawner`] maps the crate-side [`DetachedSpawnRequest`] onto the
//! binary-local `client_api::SpawnDetachedRequest` and returns the daemon job id.
//!
//! The transport model re-sends the ORIGINAL command line (`req.command`); the
//! daemon wraps it `orkia -c "<command> &"` and the detached runtime re-parses it
//! through the identical classifier → dispatch the REPL uses, re-deriving agent
//! context / cage wrapper / hooks / stdin handling from its own config. So only
//! the fields the runtime cannot re-derive travel on the request: command,
//! working_dir, agent_name, extra_env.
//!
//! Installed ONLY on the main REPL: [`provider`] returns `None` when this process
//! is itself a detached runtime (`ORKIA_DETACHED_JOB_ID` set), so a runtime never
//! recurses into its own daemon — it falls through to `JobController::spawn` and
//! hosts the agent in-process (it IS the host).

use std::sync::Arc;

use orkia_shell::ShellConfig;
use orkia_shell_types::{DetachedSpawnRequest, DetachedSpawner};

use super::client_api::SpawnDetachedRequest;

struct DetachedSpawnerBridge {
    config: ShellConfig,
}

impl DetachedSpawner for DetachedSpawnerBridge {
    fn spawn_detached(&self, req: DetachedSpawnRequest) -> Result<u32, String> {
        let mut wire = SpawnDetachedRequest::new(req.command);
        wire.working_dir = req.working_dir;
        wire.agent_name = req.agent_name;
        wire.extra_env = req.extra_env;
        super::client_api::spawn_detached_request_id(wire, &self.config)
    }
}

/// Build the bridge iff this process is the MAIN REPL (i.e. NOT a detached
/// runtime). `None` when `ORKIA_DETACHED_JOB_ID` is set so a runtime spawns
/// agents in-process (it is the daemon-owned host) instead of recursing.
pub(crate) fn provider(config: &ShellConfig) -> Option<Arc<dyn DetachedSpawner>> {
    if std::env::var("ORKIA_DETACHED_JOB_ID").is_ok() {
        return None;
    }
    Some(Arc::new(DetachedSpawnerBridge {
        config: config.clone(),
    }))
}
