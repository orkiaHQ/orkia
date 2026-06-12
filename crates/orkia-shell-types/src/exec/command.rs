// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The `Command` trait and its call/context types.
//!
//! returns a lazy `ListStream` immediately; a collecting command awaits the
//! full input before emitting. The kernel guarantees the input type before
//! `run` is ever called, so `run` only handles the happy path.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;

pub use crate::AttentionRow;
use crate::exec::capability::CapabilitySet;
use crate::exec::error::ExecError;
use crate::exec::pipeline_data::PipelineData;
use crate::exec::signature::Signature;
use crate::exec::value::Value;
use crate::extensions::{AuthView, JournalEnvelopeHook};
use crate::{AgentInfo, AttentionControl, JobInfo};

/// A command invocation with arguments already parsed and coerced against the
/// command's [`Signature`]. The author reads typed values, never raw strings.
pub struct EvaluatedCall {
    pub head: String,
    pub positional: Vec<Value>,
    /// Named flags. A boolean switch maps to `Some(None)` when present; a
    /// value flag maps to `Some(Some(value))`. Absent flags are not keyed.
    pub named: IndexMap<String, Option<Value>>,
}

impl EvaluatedCall {
    /// A required positional by index. Returns `MissingArg` if absent — though
    /// the engine normally catches missing required args during binding.
    pub fn req(&self, index: usize) -> Result<&Value, ExecError> {
        self.positional
            .get(index)
            .ok_or_else(|| ExecError::MissingArg {
                command: self.head.clone(),
                name: format!("#{index}"),
            })
    }

    /// An optional positional by index.
    pub fn opt(&self, index: usize) -> Option<&Value> {
        self.positional.get(index)
    }

    /// Whether a (boolean or value) flag was supplied.
    pub fn has_flag(&self, name: &str) -> bool {
        self.named.contains_key(name)
    }

    /// The value of a value-bearing flag, if present.
    pub fn get_flag(&self, name: &str) -> Option<&Value> {
        self.named.get(name).and_then(|v| v.as_ref())
    }
}

/// Read-only execution context passed to every command. A *snapshot* — never
/// a borrow of live REPL state — so commands stay free of global state and
/// own no shared mutable resource (invariant).
pub struct CommandCtx {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    /// Orkia's data directory (`~/.orkia` by default). A plain value — adding it
    /// couples nothing — that lets privileged builtins reading on-disk state
    /// (`log`, `seal`, …) migrate to `Command` without dragging heavy deps into
    pub data_dir: PathBuf,
    /// Host introspection snapshot for `ps`-like commands.
    pub agents: Vec<AgentInfo>,
    pub jobs: Vec<JobInfo>,
    /// Optional sink for surfacing journal events from within a command.
    pub journal: Option<Arc<dyn JournalEnvelopeHook>>,
    /// Read-only identity/plan view for `whoami`/`plan`. A lightweight service
    /// handle implemented in `orkia-shell` — keeps the auth/capability impl
    pub auth: Option<Arc<dyn AuthView>>,
    /// Snapshot of pending agent-attention prompts for the `attention` builtin.
    pub attention: Vec<AttentionRow>,
    /// Optional control handle for attention actions that mutate the queue actor.
    pub attention_control: Option<Arc<dyn AttentionControl>>,
    /// Effect capabilities **granted** to this invocation (fail-closed).
    /// Native commands consult it through the verified accessors below before
    /// any effect; the same type gates plugins (structurally, via the wasmtime
    /// linker). A lightweight description type — adding it pulls no heavy deps
    pub capabilities: CapabilitySet,
}

impl CommandCtx {
    /// Verified accessor: a native command must call this before reading a
    /// path. Fails closed (`CapabilityDenied`) when `fs_read` isn't granted for
    pub fn require_fs_read(&self, command: &str, path: &std::path::Path) -> Result<(), ExecError> {
        if self.capabilities.allows_fs_read(path) {
            Ok(())
        } else {
            Err(ExecError::CapabilityDenied {
                command: command.to_string(),
                capability: "fs_read".to_string(),
                detail: path.display().to_string(),
            })
        }
    }

    /// Verified accessor for filesystem writes. Fails closed when ungranted.
    pub fn require_fs_write(&self, command: &str, path: &std::path::Path) -> Result<(), ExecError> {
        if self.capabilities.allows_fs_write(path) {
            Ok(())
        } else {
            Err(ExecError::CapabilityDenied {
                command: command.to_string(),
                capability: "fs_write".to_string(),
                detail: path.display().to_string(),
            })
        }
    }

    /// Verified accessor for network access. Fails closed when ungranted.
    pub fn require_net(&self, command: &str, host: &str) -> Result<(), ExecError> {
        if self.capabilities.allows_net(host) {
            Ok(())
        } else {
            Err(ExecError::CapabilityDenied {
                command: command.to_string(),
                capability: "net".to_string(),
                detail: host.to_string(),
            })
        }
    }
}

/// A typed, streamed, registry-registered command.
#[async_trait]
pub trait Command: Send + Sync + 'static {
    /// The declarative signature: accepted IO types and argument grammar.
    fn signature(&self) -> Signature;

    /// A one-line human description.
    fn description(&self) -> &str;

    /// `true` (default): emits as input arrives (`where`, `first`, `get`).
    /// `false`: must consume all input before emitting (`sort-by`, `length`).
    /// Declarative — informs UX and optimization, not correctness.
    fn is_streaming(&self) -> bool {
        true
    }

    /// Run the command. The kernel guarantees `input`'s type already satisfies
    /// this command's signature, so type errors cannot occur here.
    async fn run(
        &self,
        ctx: &CommandCtx,
        call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, ExecError>;
}
