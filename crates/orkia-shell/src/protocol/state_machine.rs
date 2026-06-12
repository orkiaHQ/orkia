// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `terminal_state::DetectorEvent` → [`OrkiaEvent`] converter.
//!
//! The detector is the inference-based source (write-stopped +
//! cursor positioned + process_in_read). Confidence rides through
//! from `JobAttention.confidence` so consumers can apply tighter
//! thresholds (e.g. only auto-resolve at `>= 0.85`).
//!
//! `Closed` is a transport signal (the agent's PTY closed); it has
//! no user-facing meaning so we filter it out. The matching
//! `SessionEnd` event arrives via the hook converter (`Stop` hook)
//! or the lifecycle pipeline.

use super::{EventPayload, EventSource, OrkiaEvent, PromptType};
use crate::terminal_state::{DetectorEvent, JobAttention};

pub fn convert_detector_event(event: &DetectorEvent, agent_name: &str) -> Option<OrkiaEvent> {
    match event {
        DetectorEvent::Attention(att) => Some(from_attention(att, agent_name)),
        // The decision to inject writes no bytes yet — the `UserMessage`
        // fact is emitted on `Delivered`, once the body has landed.
        DetectorEvent::Injected { .. } => None,
        DetectorEvent::Delivered {
            job_id,
            agent_name: name,
            body,
        } => Some(OrkiaEvent {
            source: EventSource::StateMachine,
            event: EventPayload::UserMessage { text: body.clone() },
            confidence: 1.0, // we definitely wrote these bytes
            timestamp: chrono::Utc::now(),
            job_id: *job_id,
            agent_name: name.clone(),
            rfc_id: None,
        }),
        DetectorEvent::Closed { .. } => None,
    }
}

fn from_attention(att: &JobAttention, agent_name: &str) -> OrkiaEvent {
    let payload = if att.prompt_type == PromptType::ShellPrompt {
        // The state machine inferring "shell prompt is up" maps to
        // the same semantic point OSC 133 `B` would mark: the agent
        // is ready for input.
        EventPayload::PromptReady
    } else {
        EventPayload::Attention {
            prompt_type: att.prompt_type.clone(),
            last_line: att.last_line.clone(),
        }
    };
    OrkiaEvent {
        source: EventSource::StateMachine,
        event: payload,
        confidence: att.confidence,
        timestamp: chrono::Utc::now(),
        job_id: att.job_id,
        agent_name: if agent_name.is_empty() {
            att.agent_name.clone()
        } else {
            agent_name.to_string()
        },
        rfc_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::JobId;

    fn attention(prompt_type: PromptType) -> JobAttention {
        JobAttention {
            job_id: JobId(1),
            agent_name: "faye".into(),
            confidence: 0.7,
            prompt_type,
            last_line: "anything".into(),
            has_pending_body: false,
            pending_body_preview: None,
        }
    }

    #[test]
    fn shell_prompt_attention_becomes_prompt_ready() {
        let det = DetectorEvent::Attention(attention(PromptType::ShellPrompt));
        let evt = convert_detector_event(&det, "faye").expect("event");
        assert!(matches!(evt.event, EventPayload::PromptReady));
        assert!(matches!(evt.source, EventSource::StateMachine));
        assert!((evt.confidence - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn yes_no_attention_becomes_attention_payload() {
        let det = DetectorEvent::Attention(attention(PromptType::YesNo));
        let evt = convert_detector_event(&det, "faye").expect("event");
        match evt.event {
            EventPayload::Attention {
                prompt_type,
                last_line,
            } => {
                assert_eq!(prompt_type, PromptType::YesNo);
                assert_eq!(last_line, "anything");
            }
            other => panic!("expected Attention, got {other:?}"),
        }
    }

    #[test]
    fn injected_decision_emits_no_event() {
        // The decision to inject writes no bytes yet — no `UserMessage`
        // until the body actually lands (`Delivered`).
        let det = DetectorEvent::Injected {
            job_id: JobId(1),
            agent_name: "faye".into(),
            body: "fix the tests".into(),
        };
        assert!(convert_detector_event(&det, "faye").is_none());
    }

    #[test]
    fn delivered_becomes_user_message_with_full_confidence() {
        let det = DetectorEvent::Delivered {
            job_id: JobId(1),
            agent_name: "faye".into(),
            body: "fix the tests".into(),
        };
        let evt = convert_detector_event(&det, "faye").expect("event");
        match evt.event {
            EventPayload::UserMessage { ref text } => assert_eq!(text, "fix the tests"),
            other => panic!("expected UserMessage, got {other:?}"),
        }
        assert_eq!(evt.confidence, 1.0);
    }

    #[test]
    fn closed_event_returns_none() {
        let det = DetectorEvent::Closed {
            job_id: JobId(1),
            exit_code: None,
        };
        assert!(convert_detector_event(&det, "faye").is_none());
    }
}
