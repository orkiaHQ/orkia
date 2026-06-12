// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Stdin/stderr prompt primitives for the setup wizard.
//!
//! Output goes to stderr so a piped stdout (`orkia setup | tee log`)
//! doesn't pull prompt text into the captured stream. The user's input
//! still comes from stdin, which is what the harness reads.

use std::io::{self, BufRead, Write};

/// Read one trimmed line from stdin. Empty input returns `default` when
/// provided, otherwise an empty string. Errors fall through as empty so
/// the wizard stays responsive when stdin closes (e.g. piped input).
pub fn ask(prompt: &str, default: Option<&str>) -> String {
    print_prompt(prompt, default);
    read_line()
        .unwrap_or_default()
        .trim()
        .to_string()
        .into_or_default(default)
}

/// Yes/no prompt. Returns `default_yes` on empty input or read error.
pub fn ask_yn(prompt: &str, default_yes: bool) -> bool {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    let _ = write!(io::stderr(), "  {prompt} {hint} ");
    let _ = io::stderr().flush();
    let line = read_line().unwrap_or_default().trim().to_lowercase();
    match line.as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default_yes,
    }
}

/// Render a numbered menu, parse a comma-separated answer, return the
/// selected indices (0-based) capped at `max` entries. Invalid entries
/// are dropped silently — the caller validates `len()` against its own
/// requirements (e.g. wizard step 4 wants exactly 3).
pub fn ask_select(prompt: &str, options: &[(String, String)], max: usize) -> Vec<usize> {
    let mut err = io::stderr();
    let _ = writeln!(err);
    for (i, (name, desc)) in options.iter().enumerate() {
        let _ = writeln!(err, "    [{}] {:<22} — {}", i + 1, name, desc);
    }
    let _ = writeln!(err);
    let _ = write!(err, "  {prompt} > ");
    let _ = err.flush();
    let line = read_line().unwrap_or_default();
    line.split(',')
        .filter_map(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n >= 1 && *n <= options.len())
        .map(|n| n - 1)
        .take(max)
        .collect()
}

fn print_prompt(prompt: &str, default: Option<&str>) {
    let mut err = io::stderr();
    match default {
        Some(d) => {
            let _ = write!(err, "  {prompt} [{d}] > ");
        }
        None => {
            let _ = write!(err, "  {prompt} > ");
        }
    }
    let _ = err.flush();
}

fn read_line() -> io::Result<String> {
    let stdin = io::stdin();
    let mut buf = String::new();
    stdin.lock().read_line(&mut buf)?;
    Ok(buf)
}

/// Tiny extension so `ask` can fall back to its default without an
/// `if-let` ladder at every call site.
trait IntoOrDefault {
    fn into_or_default(self, default: Option<&str>) -> String;
}

impl IntoOrDefault for String {
    fn into_or_default(self, default: Option<&str>) -> String {
        if self.is_empty() {
            default.unwrap_or("").to_string()
        } else {
            self
        }
    }
}
