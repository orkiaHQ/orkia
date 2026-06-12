// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Formatting helpers for the `every` builtin's output blocks. Pure
//! string-shaping — no I/O beyond reading the system clock (for the
//! "next run" column in `list`).

use std::str::FromStr;

use chrono::{DateTime, Local, TimeZone};
use cron::Schedule;
use orkia_shell_types::BlockContent;

use super::crontab::OrkiaEntry;

/// Render the human-readable "next: Mon May 26 09:00" timestamp for a
/// 5-field cron expression, or `"-"` when the expression won't parse
/// or has no future occurrence inside the search window.
///
/// The `cron` crate uses 7-field Quartz syntax (sec + 5 + year), so we
/// adapt our 5-field crontab line by prepending `0 ` (seconds) and
/// appending ` *` (year).
pub fn next_run_display(cron: &str) -> String {
    match next_run(cron) {
        Some(dt) => dt.format("%a %b %d %H:%M").to_string(),
        None => "-".into(),
    }
}

pub fn next_run(cron: &str) -> Option<DateTime<Local>> {
    let quartz = format!("0 {cron} *");
    let schedule = Schedule::from_str(&quartz).ok()?;
    schedule.upcoming(Local).next()
}

/// Render a successful create as `BlockContent::SystemInfo` + a
pub fn render_created(cron: &str, command_line: &str) -> Vec<BlockContent> {
    let next = match next_run(cron) {
        Some(dt) => dt.format("%A, %B %-d %Y at %H:%M").to_string(),
        None => "unknown (cron expression has no upcoming occurrences)".into(),
    };
    vec![
        BlockContent::SystemInfo(format!("✓ Scheduled: {cron} {command_line}")),
        BlockContent::Text(format!("  Next run: {next}")),
    ]
}

/// 1-based to match the index `remove`/`pause`/`resume` accept.
pub fn render_list(entries: &[OrkiaEntry]) -> Vec<BlockContent> {
    if entries.is_empty() {
        return vec![BlockContent::SystemInfo("no scheduled jobs".into())];
    }
    let mut out = vec![
        BlockContent::SystemInfo("# Orkia scheduled jobs".into()),
        BlockContent::SystemInfo(
            "  N  CRON              AGENT     COMMAND                              NEXT".into(),
        ),
    ];
    for (i, e) in entries.iter().enumerate() {
        let next = if e.paused {
            "(paused)".into()
        } else {
            next_run_display(&e.cron)
        };
        out.push(BlockContent::Text(format!(
            "  {:<2} {:<17} {:<9} {:<36} {}",
            i + 1,
            truncate(&e.cron, 17),
            truncate(&e.agent, 9),
            truncate(&e.command, 36),
            next,
        )));
    }
    out
}

pub fn render_removed(entry: &OrkiaEntry) -> Vec<BlockContent> {
    vec![BlockContent::SystemInfo(format!(
        "✓ Removed: {}",
        entry.command
    ))]
}

pub fn render_paused(entry: &OrkiaEntry) -> Vec<BlockContent> {
    vec![BlockContent::SystemInfo(format!(
        "✓ Paused: {}",
        entry.command
    ))]
}

pub fn render_resumed(entry: &OrkiaEntry) -> Vec<BlockContent> {
    vec![BlockContent::SystemInfo(format!(
        "✓ Resumed: {} (next: {})",
        entry.command,
        next_run_display(&entry.cron),
    ))]
}

pub fn render_error(msg: impl Into<String>) -> Vec<BlockContent> {
    vec![BlockContent::Error(msg.into())]
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{end}…")
    }
}

// `chrono::Local` warm-up — silence `unused_imports` warning on
// platforms where `TimeZone` isn't transitively referenced.
#[allow(dead_code)]
fn _ensure_tz_import_used() {
    let _ = Local.timestamp_opt(0, 0);
}
