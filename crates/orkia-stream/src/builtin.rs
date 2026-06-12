// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `orkia stream {status,pause,resume}` — parser + dispatcher.
//!
//! Mirrors the `orkia-builtin::team` pattern (parse into typed action,
//! dispatch in the shell). The dispatcher is exposed as `dispatch(action,
//! handle)` so the shell-side `stream_builtins.rs` is a one-liner.

use crate::{StreamHandle, StreamStatus, pause, resume, status};

#[derive(Debug, Clone, PartialEq)]
pub enum StreamAction {
    Status,
    Pause,
    Resume,
}

/// Parse `args` (everything after `stream`) into a `StreamAction`.
pub fn parse(args: &[String]) -> Result<StreamAction, String> {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    match sub {
        "status" | "info" => Ok(StreamAction::Status),
        "pause" => Ok(StreamAction::Pause),
        "resume" | "unpause" => Ok(StreamAction::Resume),
        other => Err(format!("unknown stream subcommand: {other}")),
    }
}

/// Synchronous dispatcher returning a human-readable result string. The
/// shell-side adapter wraps this in `BlockContent::Text` (or equivalent).
pub fn dispatch(action: StreamAction, handle: Option<&StreamHandle>) -> String {
    match action {
        StreamAction::Status => render_status(handle),
        StreamAction::Pause => match handle {
            Some(h) => match pause(h) {
                Ok(()) => "stream paused (flag file written)".into(),
                Err(e) => format!("stream pause failed: {e}"),
            },
            None => "stream not running (no handle)".into(),
        },
        StreamAction::Resume => match handle {
            Some(h) => match resume(h) {
                Ok(()) => "stream resumed".into(),
                Err(e) => format!("stream resume failed: {e}"),
            },
            None => "stream not running (no handle)".into(),
        },
    }
}

fn render_status(handle: Option<&StreamHandle>) -> String {
    let h = match handle {
        Some(h) => h,
        None => return "stream: not running (no-auth, disabled, or never started)".into(),
    };
    match status(h) {
        StreamStatus::Running {
            events_published,
            lag,
            ..
        } => format!(
            "stream: running ({events_published} events published, lag={:.2}s)",
            lag.as_secs_f64()
        ),
        StreamStatus::Paused => "stream: paused".into(),
        StreamStatus::NoAuth => "stream: no-auth (run 'orkia auth login')".into(),
        StreamStatus::Unreachable { retry_count, .. } => {
            format!("stream: unreachable (retries={retry_count})")
        }
        StreamStatus::Disabled => "stream: disabled".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn defaults_to_status() {
        assert_eq!(parse(&[]).unwrap(), StreamAction::Status);
    }

    #[test]
    fn parses_pause_resume() {
        assert_eq!(parse(&s(&["pause"])).unwrap(), StreamAction::Pause);
        assert_eq!(parse(&s(&["resume"])).unwrap(), StreamAction::Resume);
        assert_eq!(parse(&s(&["unpause"])).unwrap(), StreamAction::Resume);
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse(&s(&["frobnicate"])).is_err());
    }

    #[test]
    fn dispatch_without_handle_returns_diagnostic() {
        let out = dispatch(StreamAction::Status, None);
        assert!(out.starts_with("stream: not running"));
    }
}
