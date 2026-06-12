// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! JSON-RPC 2.0 message helpers shared between requests and tests.

use orkia_shell_types::{IntentGuess, KernelRpcError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Serialize, Deserialize)]
pub struct ClassifyRequestParams {
    pub line: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClassifyResponse {
    /// Future fields: `confidence`, `agent_name`, etc.
    pub intent: String,
}

impl ClassifyResponse {
    pub fn into_intent_guess(self) -> IntentGuess {
        match self.intent.as_str() {
            "agent" => IntentGuess::Agent,
            _ => IntentGuess::Command,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HandshakeResponse {
    /// Wire-protocol revision spoken by the daemon (monotonic int).
    pub protocol: u32,
    /// Daemon build version (informational).
    pub kernel: String,
    /// Lowest client revision the daemon accepts. Absent → no floor.
    #[serde(default)]
    pub min_client: Option<u32>,
    /// Optional feature flags the daemon advertises. Absent → empty.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// Build a JSON-RPC 2.0 request envelope.
pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Pull the `result` field out of a JSON-RPC 2.0 response and decode
/// it into `T`. Surfaces JSON-RPC errors via [`KernelRpcError::Protocol`].
pub fn extract_result<T: for<'de> Deserialize<'de>>(resp: &Value) -> Result<T, KernelRpcError> {
    if let Some(err) = resp.get("error") {
        return Err(KernelRpcError::Protocol(err.to_string()));
    }
    let result = resp
        .get("result")
        .ok_or_else(|| KernelRpcError::Protocol("response missing `result`".into()))?;
    serde_json::from_value(result.clone()).map_err(|e| KernelRpcError::Protocol(e.to_string()))
}
