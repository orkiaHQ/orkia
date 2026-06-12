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
pub mod agent_templates;
pub mod approve;
pub mod briefing;
pub mod config;
pub mod every;
pub mod forge;
pub mod help;
pub mod history;
pub mod invite;
pub mod issue;
pub mod kill;
pub mod leave;
pub mod members;
pub mod migrate_rc;
pub mod project;
pub mod ps;
pub mod rfc;
pub mod route;
pub mod scope_flag;
pub mod scope_validation;
pub mod share;
pub mod stream;
pub mod team;

pub use scope_flag::parse_scope_flag;
// `seal` lives in `orkia-shell::seal::builtin` now — it needs
// SHA-256 + chain types, which would bloat this crate's deps.

use orkia_shell_types::BlockContent;
pub type BuiltinResult = Vec<BlockContent>;
