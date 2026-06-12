// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! JSON-RPC MCP tool server for the Orkia RFC primitive.
//!
//! `orkia_rfc_get_context`, `orkia_rfc_state`, `orkia_rfc_list_decisions`,
//! `orkia_rfc_ask`, `orkia_rfc_log_decision`, `orkia_rfc_propose_edit`,
//! `orkia_rfc_propose_promote`.
//!
//! Transport (multiplexing on `~/.orkia/run/orkia.sock` next to the journal
//! listener) is wired by the `orkia-shell` crate, which calls
//! [`dispatch_request`] for each parsed JSON-RPC frame.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod handler;
mod rpc;

pub use handler::dispatch_request;
pub use rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
