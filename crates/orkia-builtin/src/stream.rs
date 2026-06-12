// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `stream` builtin — parse `stream {status,pause,resume}` into a
//!
//! The shell-side dispatcher (in `orkia-shell::stream_builtins`) drives
//! the running `orkia-stream` task with the parsed action.

#[derive(Debug, Clone, PartialEq)]
pub enum StreamAction {
    Status,
    Pause,
    Resume,
}

pub fn parse(args: &[String]) -> Result<StreamAction, String> {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    match sub {
        "status" | "info" => Ok(StreamAction::Status),
        "pause" => Ok(StreamAction::Pause),
        "resume" | "unpause" => Ok(StreamAction::Resume),
        other => Err(format!("unknown stream subcommand: {other}")),
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
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse(&s(&["frobnicate"])).is_err());
    }
}
