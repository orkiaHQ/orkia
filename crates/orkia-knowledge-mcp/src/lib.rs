// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! JSON-RPC MCP tool server for the premium Orkia Knowledge Graph.
//!
//! `recall`, `knowledge_search`, `knowledge_node`. The agent pulls knowledge
//! through these (Orkia cannot push context mid-session); the L2a behavioral
//! block in `context.md` trains it to do so.
//!
//! Transport (multiplexing on `~/.orkia/run/orkia.sock` next to the journal
//! listener, exactly like `orkia-rfc-mcp`) is wired by the `orkia-shell` crate,
//! which calls [`dispatch_request`] for each parsed JSON-RPC frame and only
//! registers the tool set when the premium gate is open.
//!
//! Strictly read-only over the local [`orkia_reasoning_store::ReasoningStore`]
//! cache: it never writes. Served node ids ride out on [`Dispatched::accessed`]
//! so the transport can journal a `KnowledgeAccess` event; the REPL-owned
//! consumer (the single store writer, CLAUDE.md #2) applies the access-accounting
//! bump (`access_count` / `last_accessed`) — the decay signal the cloud later
//! on the network: a cache miss returns "sync pending", not a cloud fetch.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod handler;
mod rpc;

pub use handler::{Dispatched, dispatch_request};
pub use rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
