// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! RFC primitive for Orkia: typed state machine, frontmatter, content hashing,
//! decision log, and filesystem persistence.
//!
//! This crate is pure I/O over the filesystem; it does **not** own any
//! concurrency primitives. Higher layers (orkia-rfc-lock, orkia-rfc-state)
//! serialize access through an owning task.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod decision;
pub mod error;
pub mod frontmatter;
pub mod hash;
pub mod id;
pub mod matrix;
pub mod scope;
pub mod state;
pub mod store;

pub use decision::{DecisionId, DecisionKind, DecisionRecord, DecisionStatus};
pub use error::RfcError;
pub use frontmatter::{RfcFrontmatter, parse_frontmatter, render_frontmatter};
pub use hash::{ContentHash, content_hash_of};
pub use id::{AgentId, RfcId, SectionPath};
pub use matrix::{RfcTool, tool_allowed};
pub use scope::Scope;
pub use state::{RfcState, Transition, TransitionCtx, validate_transition};
pub use store::{RfcRecord, RfcStore};
