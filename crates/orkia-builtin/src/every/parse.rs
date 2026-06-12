// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Tier 1 rule-based natural-language → 5-field crontab expression.
//!
//! The grammar is intentionally tiny — anything outside this table
//! returns [`ParseError::Unrecognised`] so the caller can print the
//!
//! ```text
//! every N minutes               → */N * * * *
//! every N hours                 → 0 */N * * *
//! every hour                    → 0 * * * *
//! daily <time>                  → M H * * *
//! weekdays <time>               → M H * * MON-FRI
//! weekends <time>               → M H * * SAT,SUN
//! <day> [and <day>...] <time>   → M H * * DOW[,DOW...]
//! 1st of month [<time>]         → M H D * *
//! Nth of month [<time>]         → M H D * *
//! every N days                  → 0 0 */N * *
//! twice a day                   → 0 9,18 * * *
//! <time> alone                  → M H * * *  (i.e. daily)
//! ```
//!
//! `<time>` accepts `9am`, `2:30pm`, `09:00`, `midnight`, `noon`. If no
//!
//! Day name spelling: case-insensitive, English long or 3-letter form
//! (`mon`, `monday`, `MON`, `Monday`). DOW strings emitted use the
//! crontab/`cron` crate convention (`MON`, `TUE`, …).
//!
//! After synthesising a candidate expression the caller pipes it
//! through [`super::validate_cron`]; only validated strings are
//! written to the crontab.

use std::fmt;

/// Generated cron expression in the standard 5-field form:
/// `minute hour day-of-month month day-of-week`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    pub minute: String,
    pub hour: String,
    pub dom: String,
    pub month: String,
    pub dow: String,
}

impl CronExpr {
    /// Render as a crontab line fragment (`M H DOM MON DOW`).
    pub fn to_line(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.minute, self.hour, self.dom, self.month, self.dow,
        )
    }
}

impl fmt::Display for CronExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_line())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Unrecognised,
    /// A rule matched but a numeric argument was out of range
    /// (e.g. `every 0 minutes`, `every 99 hours`). Includes a short
    /// explanation for the user.
    InvalidValue(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unrecognised => f.write_str(
                "Could not parse schedule. Try a simpler format like 'monday 9am' or 'every 2 hours'.",
            ),
            Self::InvalidValue(msg) => write!(f, "Invalid schedule value: {msg}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Public entry point. Lowercases + normalises whitespace once, then
/// dispatches across the rule table.
pub fn parse(input: &str) -> Result<CronExpr, ParseError> {
    let norm = normalise(input);
    if norm.is_empty() {
        return Err(ParseError::Unrecognised);
    }

    if let Some(c) = rule_every_n_minutes(&norm)? {
        return Ok(c);
    }
    if let Some(c) = rule_every_n_hours(&norm)? {
        return Ok(c);
    }
    if let Some(c) = rule_every_hour(&norm) {
        return Ok(c);
    }
    if let Some(c) = rule_every_n_days(&norm)? {
        return Ok(c);
    }
    if let Some(c) = rule_twice_a_day(&norm) {
        return Ok(c);
    }
    if let Some(c) = rule_daily(&norm)? {
        return Ok(c);
    }
    if let Some(c) = rule_weekdays(&norm)? {
        return Ok(c);
    }
    if let Some(c) = rule_weekends(&norm)? {
        return Ok(c);
    }
    if let Some(c) = rule_nth_of_month(&norm)? {
        return Ok(c);
    }
    if let Some(c) = rule_named_days(&norm)? {
        return Ok(c);
    }
    if let Some(c) = rule_bare_time(&norm)? {
        return Ok(c);
    }
    Err(ParseError::Unrecognised)
}

// ─── Normalisation ──────────────────────────────────────────────────────

/// Lowercase + collapse runs of whitespace + strip leading/trailing
/// quotes. The rule table is written against this canonical form, so
/// every rule can pattern-match without re-doing the work.
fn normalise(input: &str) -> String {
    let trimmed = input.trim().trim_matches(|c| c == '"' || c == '\'');
    trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

// ─── Time tokens ───────────────────────────────────────────────────────

/// Try to extract a time-of-day from the tail of the input. Returns
/// `(minute, hour, remaining_prefix)`. If no time was found, returns
/// `None`. The remaining prefix is whitespace-trimmed.
///
/// Recognised forms:
/// * `midnight` → (0, 0)
/// * `noon`     → (0, 12)
/// * `Hpm` / `Ham` (e.g. `9am`, `5pm`)
/// * `H:Mam` / `H:Mpm` (e.g. `2:30pm`, `8:00am`)
/// * `HH:MM` 24-hour (e.g. `09:00`, `17:00`)
fn extract_trailing_time(input: &str) -> Option<(u8, u8, String)> {
    let (head, tail) = match input.rsplit_once(' ') {
        Some((h, t)) => (h, t),
        None => ("", input),
    };
    parse_time_token(tail).map(|(m, h)| (m, h, head.trim().to_string()))
}

fn parse_time_token(tok: &str) -> Option<(u8, u8)> {
    if tok == "midnight" {
        return Some((0, 0));
    }
    if tok == "noon" {
        return Some((0, 12));
    }
    // 24-hour HH:MM (no am/pm suffix)
    if let Some((hh, mm)) = tok.split_once(':')
        && !tok.ends_with("am")
        && !tok.ends_with("pm")
        && let (Ok(h), Ok(m)) = (hh.parse::<u8>(), mm.parse::<u8>())
        && h < 24
        && m < 60
    {
        return Some((m, h));
    }
    // 12-hour: <H>am/pm, <H>:<M>am/pm
    let (body, is_pm) = if let Some(b) = tok.strip_suffix("pm") {
        (b, true)
    } else if let Some(b) = tok.strip_suffix("am") {
        (b, false)
    } else {
        return None;
    };
    let (h, m) = if let Some((hh, mm)) = body.split_once(':') {
        (hh.parse::<u8>().ok()?, mm.parse::<u8>().ok()?)
    } else {
        (body.parse::<u8>().ok()?, 0u8)
    };
    if h == 0 || h > 12 || m >= 60 {
        return None;
    }
    let h24 = match (h, is_pm) {
        (12, false) => 0,
        (12, true) => 12,
        (n, false) => n,
        (n, true) => n + 12,
    };
    Some((m, h24))
}

/// Pull the time off the tail; if none present, return 09:00 and the
fn time_or_default(input: &str) -> ((u8, u8), String) {
    match extract_trailing_time(input) {
        Some((m, h, rest)) => ((m, h), rest),
        None => ((0, 9), input.to_string()),
    }
}

// ─── Day names ─────────────────────────────────────────────────────────

const DAY_NAMES: &[(&str, &str)] = &[
    ("mon", "MON"),
    ("monday", "MON"),
    ("tue", "TUE"),
    ("tues", "TUE"),
    ("tuesday", "TUE"),
    ("wed", "WED"),
    ("wednesday", "WED"),
    ("thu", "THU"),
    ("thurs", "THU"),
    ("thursday", "THU"),
    ("fri", "FRI"),
    ("friday", "FRI"),
    ("sat", "SAT"),
    ("saturday", "SAT"),
    ("sun", "SUN"),
    ("sunday", "SUN"),
];

fn day_name(tok: &str) -> Option<&'static str> {
    DAY_NAMES.iter().find(|(k, _)| *k == tok).map(|(_, v)| *v)
}

// ─── Rules ─────────────────────────────────────────────────────────────

fn rule_every_n_minutes(s: &str) -> Result<Option<CronExpr>, ParseError> {
    let Some(rest) = s.strip_prefix("every ") else {
        return Ok(None);
    };
    let Some(n_str) = rest
        .strip_suffix(" minutes")
        .or_else(|| rest.strip_suffix(" minute"))
    else {
        return Ok(None);
    };
    let n: u8 = n_str.parse().map_err(|_| ParseError::Unrecognised)?;
    if n == 0 || n > 59 {
        return Err(ParseError::InvalidValue(
            "minutes must be between 1 and 59".into(),
        ));
    }
    Ok(Some(CronExpr {
        minute: format!("*/{n}"),
        hour: "*".into(),
        dom: "*".into(),
        month: "*".into(),
        dow: "*".into(),
    }))
}

fn rule_every_n_hours(s: &str) -> Result<Option<CronExpr>, ParseError> {
    let Some(rest) = s.strip_prefix("every ") else {
        return Ok(None);
    };
    let Some(n_str) = rest
        .strip_suffix(" hours")
        .or_else(|| rest.strip_suffix(" hour"))
    else {
        return Ok(None);
    };
    // `every hour` (no number) is handled by rule_every_hour
    let n: u8 = n_str.parse().map_err(|_| ParseError::Unrecognised)?;
    if n == 0 || n > 23 {
        return Err(ParseError::InvalidValue(
            "hours must be between 1 and 23".into(),
        ));
    }
    Ok(Some(CronExpr {
        minute: "0".into(),
        hour: format!("*/{n}"),
        dom: "*".into(),
        month: "*".into(),
        dow: "*".into(),
    }))
}

fn rule_every_hour(s: &str) -> Option<CronExpr> {
    if s == "every hour" || s == "hourly" {
        Some(CronExpr {
            minute: "0".into(),
            hour: "*".into(),
            dom: "*".into(),
            month: "*".into(),
            dow: "*".into(),
        })
    } else {
        None
    }
}

fn rule_every_n_days(s: &str) -> Result<Option<CronExpr>, ParseError> {
    let Some(rest) = s.strip_prefix("every ") else {
        return Ok(None);
    };
    let Some(n_str) = rest
        .strip_suffix(" days")
        .or_else(|| rest.strip_suffix(" day"))
    else {
        return Ok(None);
    };
    let n: u8 = n_str.parse().map_err(|_| ParseError::Unrecognised)?;
    if n == 0 || n > 31 {
        return Err(ParseError::InvalidValue(
            "days must be between 1 and 31".into(),
        ));
    }
    Ok(Some(CronExpr {
        minute: "0".into(),
        hour: "0".into(),
        dom: format!("*/{n}"),
        month: "*".into(),
        dow: "*".into(),
    }))
}

fn rule_twice_a_day(s: &str) -> Option<CronExpr> {
    if s == "twice a day" || s == "twice daily" {
        Some(CronExpr {
            minute: "0".into(),
            hour: "9,18".into(),
            dom: "*".into(),
            month: "*".into(),
            dow: "*".into(),
        })
    } else {
        None
    }
}

fn rule_daily(s: &str) -> Result<Option<CronExpr>, ParseError> {
    let Some(rest) = s
        .strip_prefix("daily")
        .or_else(|| s.strip_prefix("every day"))
    else {
        return Ok(None);
    };
    let rest = rest.trim();
    let ((m, h), leftover) = if rest.is_empty() {
        ((0, 9), String::new())
    } else {
        match extract_trailing_time(rest) {
            Some((m, h, lo)) => ((m, h), lo),
            None => return Err(ParseError::Unrecognised),
        }
    };
    if !leftover.is_empty() {
        return Err(ParseError::Unrecognised);
    }
    Ok(Some(CronExpr {
        minute: m.to_string(),
        hour: h.to_string(),
        dom: "*".into(),
        month: "*".into(),
        dow: "*".into(),
    }))
}

fn rule_weekdays(s: &str) -> Result<Option<CronExpr>, ParseError> {
    let Some(rest) = s.strip_prefix("weekdays") else {
        return Ok(None);
    };
    let rest = rest.trim();
    let ((m, h), leftover) = if rest.is_empty() {
        ((0, 9), String::new())
    } else {
        time_or_default(rest)
    };
    if !leftover.is_empty() {
        return Err(ParseError::Unrecognised);
    }
    Ok(Some(CronExpr {
        minute: m.to_string(),
        hour: h.to_string(),
        dom: "*".into(),
        month: "*".into(),
        dow: "MON-FRI".into(),
    }))
}

fn rule_weekends(s: &str) -> Result<Option<CronExpr>, ParseError> {
    let Some(rest) = s.strip_prefix("weekends") else {
        return Ok(None);
    };
    let rest = rest.trim();
    let ((m, h), leftover) = if rest.is_empty() {
        ((0, 9), String::new())
    } else {
        time_or_default(rest)
    };
    if !leftover.is_empty() {
        return Err(ParseError::Unrecognised);
    }
    Ok(Some(CronExpr {
        minute: m.to_string(),
        hour: h.to_string(),
        dom: "*".into(),
        month: "*".into(),
        dow: "SAT,SUN".into(),
    }))
}

fn rule_nth_of_month(s: &str) -> Result<Option<CronExpr>, ParseError> {
    // Match `<N>(st|nd|rd|th) of [the] month [<time>]`.
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 3 {
        return Ok(None);
    }
    let ord = parts[0];
    // Ordinals are ASCII (`1st`/`15th`); bail before slicing so a multibyte
    // `ord` can never land `split_at` mid-char (would panic on a char boundary).
    if !ord.is_ascii() {
        return Ok(None);
    }
    let (digits, suffix) = ord.split_at(ord.len().saturating_sub(2));
    if !matches!(suffix, "st" | "nd" | "rd" | "th") {
        return Ok(None);
    }
    let Ok(day) = digits.parse::<u8>() else {
        return Ok(None);
    };
    if !(1..=31).contains(&day) {
        return Err(ParseError::InvalidValue(
            "day of month must be between 1 and 31".into(),
        ));
    }
    // Accept `of month` or `of the month`.
    let rest_idx = match (parts.get(1), parts.get(2), parts.get(3)) {
        (Some(&"of"), Some(&"month"), _) => 3,
        (Some(&"of"), Some(&"the"), Some(&"month")) => 4,
        _ => return Ok(None),
    };
    let tail = parts[rest_idx..].join(" ");
    let ((m, h), leftover) = if tail.is_empty() {
        ((0, 9), String::new())
    } else {
        time_or_default(&tail)
    };
    if !leftover.is_empty() {
        return Err(ParseError::Unrecognised);
    }
    Ok(Some(CronExpr {
        minute: m.to_string(),
        hour: h.to_string(),
        dom: day.to_string(),
        month: "*".into(),
        dow: "*".into(),
    }))
}

fn rule_named_days(s: &str) -> Result<Option<CronExpr>, ParseError> {
    // Strip an optional trailing time first so the remainder is just
    // day names + connectors. Default to 09:00.
    let ((m, h), no_time) = time_or_default(s);

    // Build the day list. Accept tokens separated by `,`, `and`, `&`,
    // or whitespace. Anything that isn't a day name → not our rule.
    let mut days: Vec<&'static str> = Vec::new();
    for raw in no_time.split([',', ' ']) {
        let tok = raw.trim();
        if tok.is_empty() || tok == "and" || tok == "&" {
            continue;
        }
        match day_name(tok) {
            Some(d) => {
                if !days.contains(&d) {
                    days.push(d);
                }
            }
            None => return Ok(None),
        }
    }
    if days.is_empty() {
        return Ok(None);
    }
    Ok(Some(CronExpr {
        minute: m.to_string(),
        hour: h.to_string(),
        dom: "*".into(),
        month: "*".into(),
        dow: days.join(","),
    }))
}

fn rule_bare_time(s: &str) -> Result<Option<CronExpr>, ParseError> {
    // The whole input must be a single time token (no other words).
    if s.contains(' ') {
        return Ok(None);
    }
    let Some((m, h)) = parse_time_token(s) else {
        return Ok(None);
    };
    Ok(Some(CronExpr {
        minute: m.to_string(),
        hour: h.to_string(),
        dom: "*".into(),
        month: "*".into(),
        dow: "*".into(),
    }))
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
