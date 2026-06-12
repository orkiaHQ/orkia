// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Filesystem agent definition (`~/.orkia/agents/<name>/agent.toml`).
//!
//! lives in `orkia-shell::agent_dir` so the types crate stays free of
//! filesystem I/O.

use std::path::PathBuf;

use crate::provider::ProviderId;

/// Top-level shape of `agent.toml`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AgentConfigFile {
    pub agent: AgentSection,
    #[serde(default)]
    pub runtime: AgentRuntimeSection,
    #[serde(default)]
    pub trust: AgentTrustSection,
    #[serde(default)]
    pub projects: AgentProjectsSection,
    #[serde(default)]
    pub context: AgentContextSection,
    #[serde(default)]
    pub hooks: AgentHooksSection,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AgentSection {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_archetype")]
    pub archetype: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AgentRuntimeSection {
    /// `type = "vendor" | "native"`. Absent means vendor — every
    /// pre-existing agent.toml keeps its meaning unchanged.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Vendor CLI to spawn. Defaults to `"claude"` at resolve time so
    /// the section can distinguish "user wrote a command" (rejected for
    /// native) from "left it absent".
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Native-runtime model reference (e.g. `"kimi:k2"`). Required for
    /// `type = "native"`, rejected for vendor.
    #[serde(default)]
    pub model: Option<String>,
}

/// `[runtime]` validation failures. The loader warns and skips the
/// agent — a malformed definition never half-loads (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeKindError {
    #[error("unknown [runtime] type {0:?} (expected \"vendor\" or \"native\")")]
    UnknownKind(String),
    #[error("[runtime] type = \"native\" requires model = \"...\"")]
    NativeMissingModel,
    #[error("[runtime] type = \"native\" does not accept `{0}`")]
    NativeRejectsField(&'static str),
    #[error("[runtime] `model` is only valid with type = \"native\"")]
    VendorRejectsModel,
}

/// Resolved execution model of an agent definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRuntimeKind {
    /// Interactive vendor CLI in a PTY (the only vendor execution model
    /// — never print/headless mode).
    Vendor {
        command: String,
        args: Vec<String>,
        provider: ProviderId,
    },
    /// Orkia-owned LLM loop; the model runs via the kernel relay.
    Native { model: String },
}

impl AgentRuntimeSection {
    /// Validate the section into an [`AgentRuntimeKind`].
    ///
    /// `hooks_provider` is the agent's `[hooks] provider` value: an
    /// explicit provider identity wins over the command basename
    /// (see [`ProviderId::derive`]).
    pub fn resolve(
        &self,
        hooks_provider: Option<&str>,
    ) -> Result<AgentRuntimeKind, RuntimeKindError> {
        let kind = self
            .kind
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase);
        match kind.as_deref() {
            None | Some("vendor") => {
                if self.model.is_some() {
                    return Err(RuntimeKindError::VendorRejectsModel);
                }
                let command = self.command.clone().unwrap_or_else(default_command);
                let provider = ProviderId::derive(hooks_provider, &command);
                Ok(AgentRuntimeKind::Vendor {
                    command,
                    args: self.args.clone(),
                    provider,
                })
            }
            Some("native") => {
                if self.command.is_some() {
                    return Err(RuntimeKindError::NativeRejectsField("command"));
                }
                if !self.args.is_empty() {
                    return Err(RuntimeKindError::NativeRejectsField("args"));
                }
                match self.model.as_deref().map(str::trim) {
                    Some(model) if !model.is_empty() => Ok(AgentRuntimeKind::Native {
                        model: model.to_string(),
                    }),
                    _ => Err(RuntimeKindError::NativeMissingModel),
                }
            }
            Some(other) => Err(RuntimeKindError::UnknownKind(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AgentTrustSection {
    #[serde(default = "default_trust")]
    pub score: f32,
    /// `"required"` → results must be reviewed before they take effect.
    /// scheduled-run path parks results when this is `required`; the
    /// interactive path delegates to the existing approval watcher.
    #[serde(default)]
    pub approval: Option<String>,
}

impl Default for AgentTrustSection {
    fn default() -> Self {
        Self {
            score: default_trust(),
            approval: None,
        }
    }
}

/// `[hooks]` section. Selects the provider whose hook config orkia
/// installs at spawn time. Unknown / absent values fall through to
/// generic (no hooks) and the agent uses the V3 file-based approval
/// fallback only.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AgentHooksSection {
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AgentProjectsSection {
    #[serde(default)]
    pub assigned: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AgentContextSection {
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: usize,
    #[serde(default = "default_true")]
    pub include_rfcs: bool,
    #[serde(default = "default_true")]
    pub include_issues: bool,
}

impl Default for AgentContextSection {
    fn default() -> Self {
        Self {
            max_context_tokens: default_max_context_tokens(),
            include_rfcs: true,
            include_issues: true,
        }
    }
}

fn default_archetype() -> String {
    "general".into()
}
fn default_command() -> String {
    "claude".into()
}
fn default_trust() -> f32 {
    0.70
}
fn default_max_context_tokens() -> usize {
    4000
}
fn default_true() -> bool {
    true
}

/// Resolved on-disk definition of an agent. Built by the loader from
/// the directory layout + `agent.toml`. Read by spawn, sidebar, builtins.
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    pub name: String,
    pub description: Option<String>,
    pub archetype: String,
    /// Resolved execution model. Authoritative — `command`/`args` below
    /// are the legacy vendor view kept for existing consumers.
    pub runtime: AgentRuntimeKind,
    /// Vendor command. Empty when `runtime` is Native (native agents
    /// never enter the command map, so nothing spawns this).
    pub command: String,
    pub args: Vec<String>,
    pub trust: f32,
    pub assigned_projects: Vec<String>,
    pub max_context_tokens: usize,
    pub include_rfcs: bool,
    pub include_issues: bool,
    pub dir: PathBuf,
    /// Provider name from `[hooks] provider = "..."` in agent.toml.
    /// `None` (or unrecognised values) means no hooks are installed
    /// and the agent uses the V3 file-based approval path only.
    pub hooks_provider: Option<String>,
    /// True when `[trust] approval = "required"`. Scheduled runs of
    /// this agent park their results in `~/.orkia/pending/<job-id>.json`
    /// and emit a journal `approval_pending` event instead of letting
    pub approval_required: bool,
}

impl AgentDefinition {
    pub fn from_config(file: AgentConfigFile, dir: PathBuf) -> Result<Self, RuntimeKindError> {
        let approval_required = matches!(
            file.trust
                .approval
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("required") | Some("require") | Some("yes") | Some("true"),
        );
        let runtime = file.runtime.resolve(file.hooks.provider.as_deref())?;
        let (command, args) = match &runtime {
            AgentRuntimeKind::Vendor { command, args, .. } => (command.clone(), args.clone()),
            AgentRuntimeKind::Native { .. } => (String::new(), Vec::new()),
        };
        Ok(Self {
            name: file.agent.name,
            description: file.agent.description,
            archetype: file.agent.archetype,
            runtime,
            command,
            args,
            trust: file.trust.score,
            assigned_projects: file.projects.assigned,
            max_context_tokens: file.context.max_context_tokens,
            include_rfcs: file.context.include_rfcs,
            include_issues: file.context.include_issues,
            dir,
            hooks_provider: file.hooks.provider,
            approval_required,
        })
    }

    pub fn system_prompt_path(&self) -> PathBuf {
        self.dir.join("system-prompt.md")
    }

    pub fn memory_path(&self) -> PathBuf {
        self.dir.join("memory.md")
    }

    pub fn tools_path(&self) -> PathBuf {
        self.dir.join("tools.toml")
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("agent.toml")
    }
}

/// Tools manifest (`tools.toml`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AgentToolsFile {
    #[serde(default)]
    pub mcp: Vec<McpServerEntry>,
    #[serde(default)]
    pub tool: Vec<AgentToolEntry>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpServerEntry {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AgentToolEntry {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub description: Option<String>,
}
