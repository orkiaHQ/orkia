// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//!
//! Hosts the runtime that the vocabulary in `orkia-shell-types::exec` cannot:
//! the command registry, the typed-pipeline parser, the argument evaluator,
//! the streaming engine, the conversion boundary, the display renderer, and
//! the concrete pilot commands.

pub mod commands;
pub mod convert;
pub mod display;
pub mod engine;
pub mod eval;
pub mod parse;
pub mod registry;
pub mod tokenize;

#[cfg(test)]
pub mod probe;

#[cfg(test)]
mod pilot_tests;

#[cfg(test)]
mod migration_v1_tests;

#[cfg(test)]
mod migration_v2a_tests;

#[cfg(test)]
mod migration_v2b_tests;

pub use engine::{PipelineInput, run_plan};
pub use parse::{AgentLeft, agent_left_type_mismatch, classify_agent_on_left, try_parse_exec};
pub use registry::CommandRegistry;
