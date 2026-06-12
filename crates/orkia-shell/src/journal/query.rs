// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Argument parsing for `journal` queries (shell builtin + the
//! standalone `orkia journal` subcommand).
//!
//! Kept here so the two entry points share one CLI vocabulary and
//! `--help` text.

use super::types::{EventType, JournalFilter};

#[derive(Debug)]
pub struct ParsedJournalArgs {
    pub filter: JournalFilter,
    pub help: bool,
}

impl ParsedJournalArgs {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut filter = JournalFilter::default();
        let mut help = false;
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "-h" | "--help" => help = true,
                "--agent" => {
                    let v = iter
                        .next()
                        .ok_or_else(|| "--agent requires a value".to_string())?;
                    filter.agent = Some(v.clone());
                }
                "--job" => {
                    let v = iter
                        .next()
                        .ok_or_else(|| "--job requires a value".to_string())?;
                    filter.job_id = Some(
                        v.parse()
                            .map_err(|_| format!("--job: '{v}' is not a valid id"))?,
                    );
                }
                "--type" => {
                    let v = iter
                        .next()
                        .ok_or_else(|| "--type requires a value".to_string())?;
                    filter.event_type = Some(parse_event_type(v)?);
                }
                "--event" => {
                    let v = iter
                        .next()
                        .ok_or_else(|| "--event requires a value".to_string())?;
                    filter.event = Some(v.clone());
                }
                "--last-response" => {
                    // Sugar: `--last-response <agent>` → most recent
                    let v = iter
                        .next()
                        .ok_or_else(|| "--last-response requires an agent".to_string())?;
                    filter.agent = Some(v.clone());
                    filter.event = Some("AgentFinalResponse".into());
                    filter.last_n = Some(1);
                }
                "--source" => {
                    let v = iter
                        .next()
                        .ok_or_else(|| "--source requires a value".to_string())?;
                    filter.source = Some(v.clone());
                }
                "--last" => {
                    let v = iter
                        .next()
                        .ok_or_else(|| "--last requires a value".to_string())?;
                    filter.last_n = Some(
                        v.parse()
                            .map_err(|_| format!("--last: '{v}' is not a valid count"))?,
                    );
                }
                "--since" => {
                    let v = iter
                        .next()
                        .ok_or_else(|| "--since requires a value".to_string())?;
                    filter.since = Some(parse_since(v)?);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(Self { filter, help })
    }
}

fn parse_event_type(s: &str) -> Result<EventType, String> {
    match s.to_ascii_lowercase().as_str() {
        "hook" => Ok(EventType::Hook),
        "approval" => Ok(EventType::Approval),
        "lifecycle" => Ok(EventType::Lifecycle),
        "shell" => Ok(EventType::Shell),
        "tell" => Ok(EventType::Tell),
        "seal" => Ok(EventType::Seal),
        "scopechange" | "scope_change" => Ok(EventType::ScopeChange),
        other => Err(format!(
            "--type: '{other}' must be one of: hook, approval, lifecycle, shell, tell, seal, scope_change"
        )),
    }
}

/// Accept either an absolute RFC3339 timestamp or a relative duration
/// suffix: `30s`, `5m`, `2h`, `7d`. Anything else errors.
fn parse_since(s: &str) -> Result<chrono::DateTime<chrono::Utc>, String> {
    if let Ok(absolute) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(absolute.with_timezone(&chrono::Utc));
    }
    // Split off the last *char* (not byte): a non-ASCII suffix would make
    // `split_at(len-1)` land mid-codepoint and panic (BUG-036).
    let unit_char = s
        .chars()
        .last()
        .ok_or_else(|| format!("--since: '{s}' is not RFC3339 or NN[smhd]"))?;
    let (num_part, unit) = s.split_at(s.len() - unit_char.len_utf8());
    let n: i64 = num_part
        .parse()
        .map_err(|_| format!("--since: '{s}' is not RFC3339 or NN[smhd]"))?;
    let seconds = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3_600,
        "d" => n * 86_400,
        _ => return Err(format!("--since: '{s}' has unsupported unit '{unit}'")),
    };
    Ok(chrono::Utc::now() - chrono::Duration::seconds(seconds))
}

pub fn help_text() -> &'static str {
    "Usage: journal [--agent NAME] [--job ID] [--type TYPE] [--event NAME]
                [--source SRC] [--last N] [--since (RFC3339 | NN[smhd])]
                [--last-response AGENT]

Query the unified event journal at ~/.orkia/journal.jsonl. Filters
are AND-combined; --last truncates to the most recent N matches.

TYPE values: hook, approval, lifecycle, shell, tell, seal
--event matches the hook event name (e.g. Stop, AgentFinalResponse).
--last-response AGENT is sugar for `--agent AGENT --event AgentFinalResponse --last 1`."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_args_parse_to_default() {
        let p = ParsedJournalArgs::parse(&args(&[])).expect("parse");
        assert!(!p.help);
        assert!(p.filter.agent.is_none());
        assert!(p.filter.event_type.is_none());
    }

    #[test]
    fn agent_and_type_and_job() {
        let p =
            ParsedJournalArgs::parse(&args(&["--agent", "faye", "--job", "2", "--type", "hook"]))
                .expect("parse");
        assert_eq!(p.filter.agent.as_deref(), Some("faye"));
        assert_eq!(p.filter.job_id, Some(2));
        assert_eq!(p.filter.event_type, Some(EventType::Hook));
    }

    #[test]
    fn type_is_case_insensitive() {
        let p = ParsedJournalArgs::parse(&args(&["--type", "HOOK"])).expect("parse");
        assert_eq!(p.filter.event_type, Some(EventType::Hook));
    }

    #[test]
    fn type_rejects_unknown() {
        let err = ParsedJournalArgs::parse(&args(&["--type", "weird"])).unwrap_err();
        assert!(err.contains("weird"));
    }

    #[test]
    fn last_parses_count() {
        let p = ParsedJournalArgs::parse(&args(&["--last", "10"])).expect("parse");
        assert_eq!(p.filter.last_n, Some(10));
    }

    #[test]
    fn since_accepts_relative_minutes() {
        let p = ParsedJournalArgs::parse(&args(&["--since", "5m"])).expect("parse");
        assert!(p.filter.since.is_some());
    }

    #[test]
    fn since_accepts_rfc3339() {
        let p = ParsedJournalArgs::parse(&args(&["--since", "2026-05-20T10:00:00+00:00"]))
            .expect("parse");
        assert!(p.filter.since.is_some());
    }

    #[test]
    fn since_rejects_garbage() {
        let err = ParsedJournalArgs::parse(&args(&["--since", "abc"])).unwrap_err();
        assert!(err.contains("--since"));
    }

    #[test]
    fn help_short_circuits() {
        let p = ParsedJournalArgs::parse(&args(&["--help"])).expect("parse");
        assert!(p.help);
    }

    #[test]
    fn event_filter_parses() {
        let p = ParsedJournalArgs::parse(&args(&["--event", "AgentFinalResponse"])).expect("parse");
        assert_eq!(p.filter.event.as_deref(), Some("AgentFinalResponse"));
    }

    #[test]
    fn last_response_sugar_expands_filter() {
        let p = ParsedJournalArgs::parse(&args(&["--last-response", "faye"])).expect("parse");
        assert_eq!(p.filter.agent.as_deref(), Some("faye"));
        assert_eq!(p.filter.event.as_deref(), Some("AgentFinalResponse"));
        assert_eq!(p.filter.last_n, Some(1));
    }

    #[test]
    fn unknown_arg_errors() {
        let err = ParsedJournalArgs::parse(&args(&["--whatever"])).unwrap_err();
        assert!(err.contains("unknown"));
    }
}
