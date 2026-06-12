// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.
#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Pure domain types for the Orkia reasoning graph.
//!
//! No I/O. Every closed graph domain is an enum (see [`enums`]) with a single
//! serde representation shared by the HTTP wire and the SQLite/Postgres TEXT
//! columns. The store ([`orkia-reasoning-store`]) and client
//! ([`orkia-reasoning-client`]) crates build on these types.

pub mod cache;
pub mod dto;
pub mod enrich;
pub mod enums;
pub mod error;
pub mod phase;

pub use cache::PreferenceCache;
pub use dto::{KnowledgeNode, PreferenceDto, RfcRef, SignalDto, TurnDto, compile_context_block};
pub use enrich::{append_knowledge_protocol, inject_preferences};
pub use enums::{
    ConversationPhase, Dimension, KnowledgeNodeKind, NodeOrigin, PreferenceScope, SessionStatus,
    SignalDirection, TurnKind, TurnRelation, TurnRole,
};
pub use error::{ReasoningError, Result};
pub use phase::{
    ClassifiedTurn, KnowledgeNodeSummary, ReasoningContext, UserPreferenceSummary, infer_phase,
    render_reasoning_block,
};
