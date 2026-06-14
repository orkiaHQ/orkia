// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod agent_context;
pub mod agent_dir;
pub mod agent_migration;
pub mod approval;
pub mod attention;
pub mod auth_builtins;
pub mod builtin_resolve;
pub mod builtin_table;
pub mod classifier;
pub mod completion;
pub mod config;
pub mod contribute_builtins;
pub mod decision;
pub mod detached_control;
pub mod engine;
pub mod error;
pub mod exec;
pub mod forge_noop;
pub mod history;
pub mod hooks;
pub mod injection_executor;
pub mod job;
pub mod journal;
pub mod kernel_builtins;
pub mod knowledge_activity;
pub mod native;
pub mod operator;
mod operator_context;
pub mod operator_projection;
mod operator_reconcile;
pub mod operator_routing;
pub mod operator_sources;
#[cfg(test)]
mod operator_tests;
pub mod pipeline;
pub mod plugins;
pub mod protocol;
pub mod providers;
pub mod reasoning_audit;
pub mod reasoning_backfill;
pub mod reasoning_builtins;
pub mod renderer;
pub mod renderers;
pub mod repl;
mod rfc_constraints;
pub mod rfc_state;
pub mod router;
pub mod scope_warnings;
pub mod seal;
pub mod session;
pub mod shape_routing;
pub mod shell_agent_pipe;
pub mod sink;
pub mod stream_builtins;
pub mod team_builtins;
pub mod team_cache;
pub mod terminal_state;
pub mod toml_policy;
pub mod trust;

pub use orkia_shell_types::workspace::{self, IssueSummary, Project, RfcSummary, Workspace};

pub use agent::{AgentInfo, AgentStatus};
pub use approval::{
    ApprovalRequest, ApprovalResponse, ApprovalSource, ApprovalWatcher, PendingApproval,
};
pub use classifier::{
    AdaptiveClassifier, AdaptiveHandle, HeuristicClassifier, IntentClassifier, IntentGuess,
    resolve_mode,
};
pub use config::{NotificationVerbosity, ShellConfig};
pub use decision::{BlockContent, Decision, Mode, NoOpReason, Outcome, PipelineStage};
pub use engine::{BrushSession, CommandOutput, ExecuteResult, ShellEngine};
pub use error::ShellError;
pub use forge_noop::NoopForgeBuilder;
pub use history::History;
pub use job::{JobController, JobId, JobKind, JobState};
pub use pipeline::parse_pipeline;
pub use renderer::{PromptContext, RenderEvent, ShellRenderer, WelcomeInfo};
pub use renderers::{ShellModeRenderer, StdoutRenderer};
pub use repl::Repl;
pub use router::{AgentRouter, HeuristicRouter, RoutingDecision, RoutingReason};
pub use seal::{SealChain, SealManager, SealRecord};
pub use session::Session;
pub use team_cache::{CachedTeamData, TeamCache, TeamCacheError};
pub use toml_policy::TomlPolicyLoader;
