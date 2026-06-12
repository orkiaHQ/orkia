// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! APC `Orkia` protocol parser.
//!
//! Wire format:
//!
//! ```text
//! \x1b _ Orkia ; <json> \x1b \
//! ```
//!
//! The `BlockParser` in `orkia-terminal-core` strips the `ESC _`
//! prefix and `ESC \` (or 0x9C ST) suffix and fires its `on_apc`
//! callback with the raw inner payload bytes. This module's
//! [`parse_orkia_apc`] takes those payload bytes, verifies the
//! `Orkia;` prefix, and decodes the remainder as JSON into an
//! [`EventPayload`].
//!
//! **No base64.** Standard JSON serialisers escape every control
//! byte (`0x00`-`0x1F`, `\`, `"`) to `\uXXXX` / `\\` / `\"`, so a
//! well-formed UTF-8 JSON document cannot contain the bytes that
//! would terminate the APC sequence early. If V2.x ever needs to
//! carry binary blobs (file contents, base64 images), Kitty-style
//! chunked base64 is a separate addition.

use super::EventPayload;

/// Parse an APC payload (the bytes between `ESC _` and the
/// terminator). Returns `None` when the prefix is wrong, the JSON
/// fails to decode, or the inner shape does not match
/// `EventPayload`. Foreign / corrupt payloads are dropped silently
/// so a misbehaving agent never crashes the parser.
pub fn parse_orkia_apc(payload: &[u8]) -> Option<EventPayload> {
    let text = std::str::from_utf8(payload).ok()?;
    let rest = text.strip_prefix("Orkia;")?;
    serde_json::from_str::<EventPayload>(rest).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_ready_round_trips() {
        let payload = br#"Orkia;{"type":"PromptReady"}"#;
        let ev = parse_orkia_apc(payload).expect("decode");
        assert!(matches!(ev, EventPayload::PromptReady));
    }

    #[test]
    fn session_start_with_model() {
        let payload = br#"Orkia;{"type":"SessionStart","model":"claude-opus-4-7"}"#;
        let ev = parse_orkia_apc(payload).expect("decode");
        match ev {
            EventPayload::SessionStart { model: Some(m) } => {
                assert_eq!(m, "claude-opus-4-7");
            }
            other => panic!("expected SessionStart, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_carries_target() {
        let payload = br#"Orkia;{"type":"ToolUse","tool":"Read","target":"src/auth/mod.rs"}"#;
        let ev = parse_orkia_apc(payload).expect("decode");
        match ev {
            EventPayload::ToolUse {
                tool,
                target,
                input_summary,
            } => {
                assert_eq!(tool, "Read");
                assert_eq!(target.as_deref(), Some("src/auth/mod.rs"));
                assert!(input_summary.is_none());
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn permission_request_with_risk() {
        let payload = br#"Orkia;{"type":"PermissionRequest","description":"rm -rf node_modules","risk":"medium"}"#;
        let ev = parse_orkia_apc(payload).expect("decode");
        match ev {
            EventPayload::PermissionRequest {
                description, risk, ..
            } => {
                assert_eq!(description, "rm -rf node_modules");
                assert_eq!(risk.as_deref(), Some("medium"));
            }
            other => panic!("expected PermissionRequest, got {other:?}"),
        }
    }

    #[test]
    fn missing_orkia_prefix_returns_none() {
        let payload = br#"Other;{"type":"PromptReady"}"#;
        assert!(parse_orkia_apc(payload).is_none());
    }

    #[test]
    fn malformed_json_returns_none() {
        let payload = br#"Orkia;{not json"#;
        assert!(parse_orkia_apc(payload).is_none());
    }

    #[test]
    fn empty_payload_returns_none() {
        assert!(parse_orkia_apc(b"").is_none());
    }

    #[test]
    fn non_utf8_payload_returns_none() {
        // Lone 0xFF is not valid UTF-8.
        assert!(parse_orkia_apc(&[0xff, 0xfe]).is_none());
    }

    #[test]
    fn unknown_event_type_returns_none() {
        let payload = br#"Orkia;{"type":"NotARealEvent"}"#;
        // EventPayload uses #[serde(tag = "type")]; unknown variants
        // fail to deserialize → None.
        assert!(parse_orkia_apc(payload).is_none());
    }
}
