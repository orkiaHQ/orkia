// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Wire contract for the `kernel.v1.llm.complete` RPC — the native
//! (Orkia-owned) agent runtime's model call.
//!
//! In the native runtime the shell owns the agent loop: it assembles
//! the transcript, executes tools behind the policy gate, and emits
//! journal/SEAL/final-response directly. The kernel is a thin HTTP
//! relay to the configured model provider (BYO keys live kernel-side,
//! never in the shell process). One request = one completion; the
//! shell drives the tool-use loop by appending results and calling
//! again.
//!
//! Untrusted-bytes posture: everything in a response — text,
//! tool names, tool inputs — originates from a remote LLM. Callers
//! validate tool names against their own registry and treat inputs
//! as hostile; nothing here is pre-sanitized.

use serde::{Deserialize, Serialize};

/// JSON-RPC method name. Additive: an older kernel answers
/// method-not-found, which the client surfaces as `Unavailable` and
/// the shell refuses the native session (fail-closed, never a silent
/// vendor fallback).
pub const METHOD_LLM_COMPLETE: &str = "kernel.v1.llm.complete";

/// One message in the conversation transcript.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeChatMessage {
    /// `"system"`, `"user"`, or `"assistant"`. Open string so the
    /// relay can pass provider-specific roles through unchanged.
    pub role: String,
    pub content: Vec<NativeContentBlock>,
}

/// A block of message content. Tool calls and results are first-class
/// so the relay can map them mechanically onto each provider's wire
/// shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NativeContentBlock {
    Text {
        text: String,
    },
    /// Model-requested tool invocation. `input` is provider JSON,
    /// untrusted — the executor validates it against the tool schema.
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Shell-produced result for a prior [`NativeContentBlock::ToolCall`],
    /// matched by `id`.
    ToolResult {
        id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// A tool the shell offers the model for this completion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input.
    pub parameters: serde_json::Value,
}

/// Params for [`METHOD_LLM_COMPLETE`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeCompletionRequest {
    /// Catalog model ref, e.g. `"kimi:k2"`. The kernel resolves
    /// provider + endpoint + key from its own config; the shell never
    /// sees credentials.
    pub model: String,
    pub messages: Vec<NativeChatMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<NativeToolDef>,
    /// Hard output cap. `None` → the relay's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// Result of [`METHOD_LLM_COMPLETE`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeCompletionResponse {
    /// Assistant blocks: text and/or tool calls.
    pub content: Vec<NativeContentBlock>,
    pub finish: NativeFinish,
    /// Provider-reported usage when available (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<NativeUsage>,
}

/// Why the completion stopped. The turn machine loops on `ToolUse`
/// and stops on everything else; `Error` and `Length` are journaled
/// and end the turn fail-closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum NativeFinish {
    EndTurn,
    ToolUse,
    Length,
    Error { message: String },
}

/// Token accounting as reported by the provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> NativeCompletionRequest {
        NativeCompletionRequest {
            model: "kimi:k2".into(),
            messages: vec![NativeChatMessage {
                role: "user".into(),
                content: vec![NativeContentBlock::Text {
                    text: "list the rust files".into(),
                }],
            }],
            tools: vec![NativeToolDef {
                name: "shell".into(),
                description: "run a shell command".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"],
                }),
            }],
            max_tokens: Some(4096),
        }
    }

    #[test]
    fn request_round_trip() {
        let req = sample_request();
        let json = serde_json::to_string(&req).expect("serialize");
        let back: NativeCompletionRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, back);
    }

    #[test]
    fn request_omits_empty_optionals() {
        let req = NativeCompletionRequest {
            model: "kimi:k2".into(),
            messages: vec![],
            tools: vec![],
            max_tokens: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(!json.contains("\"tools\""));
        assert!(!json.contains("\"max_tokens\""));
    }

    #[test]
    fn content_blocks_tag_variants() {
        let blocks = vec![
            NativeContentBlock::Text { text: "hi".into() },
            NativeContentBlock::ToolCall {
                id: "tc_1".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            NativeContentBlock::ToolResult {
                id: "tc_1".into(),
                content: "main.rs".into(),
                is_error: false,
            },
        ];
        let json = serde_json::to_string(&blocks).expect("serialize");
        assert!(json.contains("\"type\":\"text\""));
        assert!(json.contains("\"type\":\"tool_call\""));
        assert!(json.contains("\"type\":\"tool_result\""));
        let back: Vec<NativeContentBlock> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(blocks, back);
    }

    #[test]
    fn tool_result_is_error_defaults_false() {
        let json = r#"{"type":"tool_result","id":"tc_1","content":"x"}"#;
        let back: NativeContentBlock = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            back,
            NativeContentBlock::ToolResult {
                id: "tc_1".into(),
                content: "x".into(),
                is_error: false,
            }
        );
    }

    #[test]
    fn finish_round_trip_all_variants() {
        for finish in [
            NativeFinish::EndTurn,
            NativeFinish::ToolUse,
            NativeFinish::Length,
            NativeFinish::Error {
                message: "boom".into(),
            },
        ] {
            let json = serde_json::to_string(&finish).expect("serialize");
            let back: NativeFinish = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(finish, back);
        }
        let json = serde_json::to_string(&NativeFinish::EndTurn).expect("serialize");
        assert!(json.contains("\"reason\":\"end_turn\""));
    }

    #[test]
    fn response_round_trip() {
        let resp = NativeCompletionResponse {
            content: vec![NativeContentBlock::ToolCall {
                id: "tc_1".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "ls"}),
            }],
            finish: NativeFinish::ToolUse,
            usage: Some(NativeUsage {
                input_tokens: 100,
                output_tokens: 20,
            }),
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: NativeCompletionResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(resp, back);
    }

    #[test]
    fn malformed_response_is_an_error_not_a_panic() {
        // bytes from the kernel socket are untrusted.
        for bad in [
            r#"{"content": "not-an-array", "finish": {"reason":"end_turn"}}"#,
            r#"{"content": [], "finish": {"reason":"warp_speed"}}"#,
            r#"{"content": [{"type":"tool_call","id":1}], "finish":{"reason":"tool_use"}}"#,
        ] {
            assert!(serde_json::from_str::<NativeCompletionResponse>(bad).is_err());
        }
    }
}
