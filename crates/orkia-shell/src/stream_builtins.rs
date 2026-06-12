// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Shell-side dispatcher for `orkia stream {status,pause,resume}`.
//!
//! Symmetric to `team_builtins.rs`: this module owns the runtime side
//! (live handle, paused.flag, status rendering). The parser in
//! `orkia-builtin::stream` is pure.

use orkia_builtin::stream::StreamAction;
use orkia_shell_types::BlockContent;
use orkia_stream::{StreamHandle, StreamStatus};

pub fn dispatch(action: StreamAction, handle: Option<&StreamHandle>) -> Vec<BlockContent> {
    let text = match action {
        StreamAction::Status => render_status(handle),
        StreamAction::Pause => match handle {
            Some(h) => match orkia_stream::pause(h) {
                Ok(()) => "stream paused".to_string(),
                Err(e) => format!("stream pause failed: {e}"),
            },
            None => "stream: not running (no handle)".into(),
        },
        StreamAction::Resume => match handle {
            Some(h) => match orkia_stream::resume(h) {
                Ok(()) => "stream resumed".to_string(),
                Err(e) => format!("stream resume failed: {e}"),
            },
            None => "stream: not running (no handle)".into(),
        },
    };
    vec![BlockContent::Text(text)]
}

fn render_status(handle: Option<&StreamHandle>) -> String {
    let Some(h) = handle else {
        return "stream: not running (no auth, disabled, or never started)".into();
    };
    match orkia_stream::status(h) {
        StreamStatus::Running {
            events_published,
            lag,
            ..
        } => format!(
            "stream: running (events_published={events_published}, lag={:.2}s)",
            lag.as_secs_f64()
        ),
        StreamStatus::Paused => "stream: paused".into(),
        StreamStatus::NoAuth => "stream: no-auth — run 'orkia auth login' to enable".into(),
        StreamStatus::Unreachable { retry_count, .. } => {
            format!("stream: unreachable (retries={retry_count})")
        }
        StreamStatus::Disabled => "stream: disabled".into(),
    }
}
