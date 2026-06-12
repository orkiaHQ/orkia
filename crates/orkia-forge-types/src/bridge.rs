// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Tagged-union of bridge requests the JS layer in the viewer can issue.
///
/// V0/V1 carried only the storage variants. V2 adds the three privileged
/// surfaces — `agent.*`, `network.*`, `notification.*` — each of which the
/// viewer permission-checks against the per-app manifest before forwarding
/// to the runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeMessage {
    // ── V0/V1 ───────────────────────────────────────────────────────
    StorageGet {
        key: String,
    },
    StorageSet {
        key: String,
        value: String,
    },
    StorageDelete {
        key: String,
    },
    StorageKeys,

    // ── V2 ──────────────────────────────────────────────────────────
    AgentInvoke {
        invocation_id: Uuid,
        task: String,
        #[serde(default)]
        payload: serde_json::Value,
    },
    /// Cancel an in-flight invocation. V2 best-effort — the orchestrator
    /// may have already started LLM work that can't be cleanly aborted.
    AgentCancel {
        invocation_id: Uuid,
    },
    NetworkFetch {
        request_id: Uuid,
        url: String,
        method: HttpMethod,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u32,
    },
    NotificationSend {
        title: String,
        body: String,
        #[serde(default)]
        icon: NotifIcon,
        #[serde(default)]
        silent: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    #[default]
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotifIcon {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

fn default_timeout_ms() -> u32 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeResponse {
    pub id: u64,
    pub result: Result<serde_json::Value, BridgeError>,
    /// V2: when the response was the result of a privileged call that
    /// produced a SEAL event, the per-app SEAL event id is surfaced
    /// here so the app's JS can correlate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seal_event_id: Option<u64>,
}

/// Errors the bridge can return to JS.
///
/// V0/V1 variants are preserved for compatibility with existing storage
/// denied, policy denied, timeout, cancelled, runtime error).
#[derive(Debug, Clone, Serialize, Deserialize, Error)]
#[serde(tag = "code", content = "message", rename_all = "snake_case")]
pub enum BridgeError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("not permitted: {0}")]
    Forbidden(String),
    #[error("internal error: {0}")]
    Internal(String),

    // V2 additions ───────────────────────────────────────────────────
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("policy denied: {0}")]
    PolicyDenied(String),
    #[error("timeout")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
    #[error("runtime error: {0}")]
    RuntimeError(String),
}

/// Result payload the JS-side `agent.invoke()` resolves to. Mirrors the
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    pub invocation_id: Uuid,
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seal_event_id: Option<u64>,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cents: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Success,
    Error,
    Denied,
    Timeout,
    Cancelled,
}

/// Wire shape the viewer hands back to JS from `network.fetch`. Body is
/// always a string — the app is responsible for parsing JSON itself per
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_serde_storage_set() {
        let m = BridgeMessage::StorageSet {
            key: "k".into(),
            value: "v".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"kind\":\"storage_set\""));
        let parsed: BridgeMessage = serde_json::from_str(&s).unwrap();
        match parsed {
            BridgeMessage::StorageSet { key, value } => {
                assert_eq!(key, "k");
                assert_eq!(value, "v");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn message_serde_agent_invoke() {
        let id = Uuid::nil();
        let m = BridgeMessage::AgentInvoke {
            invocation_id: id,
            task: "refresh".into(),
            payload: serde_json::json!({"k": "v"}),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"kind\":\"agent_invoke\""));
        let parsed: BridgeMessage = serde_json::from_str(&s).unwrap();
        match parsed {
            BridgeMessage::AgentInvoke {
                invocation_id,
                task,
                payload,
            } => {
                assert_eq!(invocation_id, id);
                assert_eq!(task, "refresh");
                assert_eq!(payload, serde_json::json!({"k": "v"}));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn message_serde_network_fetch_defaults() {
        let raw = r#"{
            "kind": "network_fetch",
            "request_id": "00000000-0000-0000-0000-000000000000",
            "url": "https://api.example.com/x",
            "method": "GET"
        }"#;
        let parsed: BridgeMessage = serde_json::from_str(raw).unwrap();
        match parsed {
            BridgeMessage::NetworkFetch {
                timeout_ms,
                headers,
                body,
                ..
            } => {
                assert_eq!(timeout_ms, 30_000, "default timeout");
                assert!(headers.is_empty());
                assert_eq!(body, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn message_serde_notification_send() {
        let m = BridgeMessage::NotificationSend {
            title: "t".into(),
            body: "b".into(),
            icon: NotifIcon::Warning,
            silent: false,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"icon\":\"warning\""));
    }

    #[test]
    fn error_serde_v0() {
        let e = BridgeError::Storage("disk full".into());
        let s = serde_json::to_string(&e).unwrap();
        let parsed: BridgeError = serde_json::from_str(&s).unwrap();
        match parsed {
            BridgeError::Storage(m) => assert_eq!(m, "disk full"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn error_serde_v2_permission_denied() {
        let e = BridgeError::PermissionDenied("host 'evil.com' not in whitelist".into());
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"code\":\"permission_denied\""));
        let parsed: BridgeError = serde_json::from_str(&s).unwrap();
        match parsed {
            BridgeError::PermissionDenied(m) => assert!(m.contains("evil.com")),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn error_serde_timeout_no_message() {
        let e = BridgeError::Timeout;
        let s = serde_json::to_string(&e).unwrap();
        let parsed: BridgeError = serde_json::from_str(&s).unwrap();
        assert!(matches!(parsed, BridgeError::Timeout));
    }

    #[test]
    fn agent_result_serde() {
        let r = AgentResult {
            invocation_id: Uuid::nil(),
            status: AgentStatus::Success,
            output: Some(serde_json::json!({"x": 1})),
            error: None,
            seal_event_id: Some(42),
            duration_ms: 1234,
            cost_cents: Some(12),
        };
        let s = serde_json::to_string(&r).unwrap();
        let parsed: AgentResult = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.status, AgentStatus::Success);
        assert_eq!(parsed.seal_event_id, Some(42));
    }

    #[test]
    fn http_method_as_str() {
        assert_eq!(HttpMethod::Get.as_str(), "GET");
        assert_eq!(HttpMethod::Patch.as_str(), "PATCH");
    }
}
