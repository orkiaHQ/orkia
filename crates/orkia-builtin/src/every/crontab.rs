// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Crontab read/write/edit. crond is the scheduler; we just keep the
//! `# orkia:...` tagged entries in sync with the user's intent.
//!
//! On-disk format (two lines per orkia entry):
//!
//! ```text
//! # orkia:<agent>:<slug>[:timeout=<dur>]
//! <cron-expr> /usr/bin/env ORKIA_SCHEDULED=1 <orkia-bin> -c [--timeout <secs>] "<command>"
//! ```
//!
//! When `pause` is invoked we prepend `# PAUSED: ` to the command line
//! so crond skips it but the comment tag (and our list view) still
//! shows the entry. `resume` strips the prefix.
//!
//! Read/write goes through the `crontab` binary — we never touch the
//! system spool directly. That keeps us portable across cron flavours
//! and avoids permission grief.

use std::io::Write;
use std::process::{Command, Stdio};

/// One orkia-tagged entry, reconstructed from its `# orkia:...` tag
/// and the immediately following command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrkiaEntry {
    /// The full cron expression (5 fields, e.g. `0 9 * * MON`).
    pub cron: String,
    /// Agent name extracted from the tag.
    pub agent: String,
    /// Slug extracted from the tag — first 40 chars of the slugified
    /// command, alphanumerics + hyphens only. Used for `remove` /
    /// `pause` lookup.
    pub slug: String,
    /// Optional `--timeout <dur>` recorded in the tag (e.g. `60m`).
    pub timeout: Option<String>,
    /// Command body — what was passed after `@<agent>` or as a bare
    /// shell command. Re-shown in `every list`.
    pub command: String,
    /// True when the command line is prefixed with `# PAUSED: `.
    pub paused: bool,
}

/// Parsed view of the user's crontab. Non-orkia lines are preserved
/// verbatim so we can round-trip without disturbing them.
#[derive(Debug, Clone, Default)]
pub struct Crontab {
    /// Every line of the crontab, in order. Orkia entries appear here
    /// as their two raw lines (tag + command).
    pub lines: Vec<String>,
}

impl Crontab {
    /// Read the current user's crontab via `crontab -l`. An empty or
    /// missing crontab is not an error — we return an empty struct.
    pub fn load() -> Result<Self, String> {
        let out = Command::new("crontab")
            .arg("-l")
            .output()
            .map_err(|e| format!("crontab not available on this system: {e}"))?;
        // crontab(1) exits non-zero with "no crontab for <user>" on
        // first use. Treat that as an empty crontab.
        let body = if out.status.success() {
            String::from_utf8_lossy(&out.stdout).into_owned()
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("no crontab") {
                String::new()
            } else {
                // Some `crontab` builds emit nothing on stderr and a
                // non-zero status when the spool is empty. Treat any
                // empty stdout as an empty crontab; surface a real
                // error only when we have a message to show.
                if !stderr.trim().is_empty() {
                    return Err(format!("crontab -l failed: {}", stderr.trim()));
                }
                String::new()
            }
        };
        Ok(Self {
            lines: body.lines().map(str::to_string).collect(),
        })
    }

    /// Pipe the rendered crontab back through `crontab -`. Overwrites
    /// the user's spool with what we hold in `self.lines`.
    pub fn save(&self) -> Result<(), String> {
        let mut child = Command::new("crontab")
            .arg("-")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("crontab not available on this system: {e}"))?;
        if let Some(stdin) = child.stdin.as_mut() {
            let mut body = self.lines.join("\n");
            // crond is happier when the file ends in a newline.
            if !body.ends_with('\n') {
                body.push('\n');
            }
            stdin
                .write_all(body.as_bytes())
                .map_err(|e| format!("failed to write to crontab: {e}"))?;
        }
        let status = child
            .wait()
            .map_err(|e| format!("crontab process error: {e}"))?;
        if !status.success() {
            return Err(format!("crontab - exited with {status}"));
        }
        Ok(())
    }

    /// Enumerate orkia-tagged entries in declaration order.
    pub fn orkia_entries(&self) -> Vec<OrkiaEntry> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.lines.len() {
            if let Some(tag) = parse_tag(&self.lines[i])
                && let Some(cmd_line) = self.lines.get(i + 1)
            {
                let (paused, payload) = match cmd_line.strip_prefix("# PAUSED: ") {
                    Some(rest) => (true, rest.to_string()),
                    None => (false, cmd_line.to_string()),
                };
                if let Some(parsed) = parse_command_line(&payload) {
                    out.push(OrkiaEntry {
                        cron: parsed.cron,
                        agent: tag.agent,
                        slug: tag.slug,
                        timeout: tag.timeout,
                        command: parsed.command,
                        paused,
                    });
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }
        out
    }

    /// Append a new orkia entry (tag + command) to the crontab.
    pub fn append_orkia(&mut self, tag: &EntryTag, cron: &str, command_line: &str) {
        self.lines.push(tag.render());
        self.lines.push(format!("{cron} {command_line}"));
    }

    /// Remove the Nth orkia entry (1-indexed, matches `every list`).
    /// Returns the removed entry so the caller can show a success msg.
    pub fn remove_orkia_at(&mut self, index_one_based: usize) -> Option<OrkiaEntry> {
        let target = self
            .orkia_entries()
            .into_iter()
            .nth(index_one_based.saturating_sub(1))?;
        let mut new_lines = Vec::with_capacity(self.lines.len());
        let mut seen = 0usize;
        let mut i = 0;
        let mut removed = None;
        while i < self.lines.len() {
            if parse_tag(&self.lines[i]).is_some() {
                seen += 1;
                if seen == index_one_based {
                    removed = Some(target.clone());
                    i += 2; // skip tag + command
                    continue;
                }
            }
            new_lines.push(self.lines[i].clone());
            i += 1;
        }
        self.lines = new_lines;
        removed
    }

    /// Toggle the `# PAUSED: ` prefix on the Nth orkia entry. Returns
    /// the entry's new state on success.
    pub fn set_paused_at(&mut self, index_one_based: usize, pause: bool) -> Option<OrkiaEntry> {
        let mut seen = 0usize;
        let mut i = 0;
        let mut updated = None;
        while i + 1 < self.lines.len() {
            if parse_tag(&self.lines[i]).is_some() {
                seen += 1;
                if seen == index_one_based {
                    let cmd_idx = i + 1;
                    let current = self.lines[cmd_idx].clone();
                    let new_line = if pause {
                        if current.starts_with("# PAUSED: ") {
                            current
                        } else {
                            format!("# PAUSED: {current}")
                        }
                    } else {
                        current
                            .strip_prefix("# PAUSED: ")
                            .map(str::to_string)
                            .unwrap_or(current)
                    };
                    self.lines[cmd_idx] = new_line;
                    updated = self.orkia_entries().into_iter().nth(index_one_based - 1);
                    break;
                }
            }
            i += 1;
        }
        updated
    }
}

// ─── Tag handling ──────────────────────────────────────────────────────

/// Parsed `# orkia:<agent>:<slug>[:timeout=<dur>]` comment.
#[derive(Debug, Clone)]
pub struct EntryTag {
    pub agent: String,
    pub slug: String,
    pub timeout: Option<String>,
}

impl EntryTag {
    pub fn new(agent: &str, command: &str, timeout: Option<&str>) -> Self {
        Self {
            agent: agent.to_string(),
            slug: slugify(command),
            timeout: timeout.map(str::to_string),
        }
    }

    pub fn render(&self) -> String {
        match &self.timeout {
            Some(t) => format!("# orkia:{}:{}:timeout={t}", self.agent, self.slug),
            None => format!("# orkia:{}:{}", self.agent, self.slug),
        }
    }
}

fn parse_tag(line: &str) -> Option<EntryTag> {
    let rest = line.strip_prefix("# orkia:")?;
    let mut parts = rest.split(':');
    let agent = parts.next()?.to_string();
    let slug = parts.next()?.to_string();
    let mut timeout = None;
    for extra in parts {
        if let Some(v) = extra.strip_prefix("timeout=") {
            timeout = Some(v.to_string());
        }
    }
    Some(EntryTag {
        agent,
        slug,
        timeout,
    })
}

/// Slugify to alphanumerics + hyphens, lower-case, first 40 chars.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(40);
    let mut last_was_hyphen = false;
    for ch in input.chars() {
        if out.len() >= 40 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_hyphen = false;
        } else if !last_was_hyphen && !out.is_empty() {
            out.push('-');
            last_was_hyphen = true;
        }
    }
    out.trim_matches('-').to_string()
}

// ─── Command-line parsing ──────────────────────────────────────────────

struct ParsedCommandLine {
    cron: String,
    command: String,
}

/// Pull the 5-field cron expression off the front of an orkia command
/// line, then extract the quoted command argument to `-c`. Used by
/// [`Crontab::orkia_entries`] when re-reading what we previously wrote.
fn parse_command_line(line: &str) -> Option<ParsedCommandLine> {
    let mut it = line.split_whitespace();
    let fields: Vec<&str> = it.by_ref().take(5).collect();
    if fields.len() != 5 {
        return None;
    }
    let cron = fields.join(" ");
    let rest = it.collect::<Vec<_>>().join(" ");
    let command = extract_dash_c_arg(&rest)?;
    Some(ParsedCommandLine { cron, command })
}

/// Escape a command for embedding as the double-quoted `-c` argument of a
/// crontab line. In order: backslash (so it introduces the other escapes
/// unambiguously), double quote (so the shell doesn't end the `-c` arg
/// early), and percent (crond treats an unescaped `%` as a newline in the
/// command field). [`extract_dash_c_arg`] reverses exactly this
/// (BUG-030/031).
pub fn escape_dash_c_arg(command: &str) -> String {
    command
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "\\%")
}

/// Find `-c ... "<command>"` in the tail and return the unescaped
/// `<command>`. `render_command_line` writes the command as the last
/// double-quoted argument via [`escape_dash_c_arg`]. We scan from the
/// opening quote to the first UNescaped closing quote, undoing those
/// escapes. The old `rfind('"')` approach mis-parsed any command that
/// itself contained a quote (BUG-030).
fn extract_dash_c_arg(s: &str) -> Option<String> {
    let span = dash_c_arg_span(s)?;
    let mut out = String::new();
    let mut escaped = false;
    for c in s[span].chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Byte span of the (still-escaped) `-c` argument inside `s`, exclusive of
/// the surrounding quotes. `None` when the line has no well-formed
/// `-c "..."` argument.
fn dash_c_arg_span(s: &str) -> Option<std::ops::Range<usize>> {
    let pos = s.find("-c ")?;
    let after_start = pos + 3;
    let open_rel = s[after_start..].find('"')?;
    let content_start = after_start + open_rel + 1;
    let mut escaped = false;
    for (i, c) in s[content_start..].char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Some(content_start..content_start + i);
        }
    }
    None
}

/// Replace the quoted `-c` argument of a crontab command line with
/// `new_command` (escaped via [`escape_dash_c_arg`]). Everything around the
/// quotes — cron expression, env prefix, binary path, flags, a `# PAUSED: `
/// prefix — is preserved byte-for-byte. `None` when the line has no
/// well-formed `-c "..."` argument.
pub fn rewrite_dash_c_arg(line: &str, new_command: &str) -> Option<String> {
    let span = dash_c_arg_span(line)?;
    let mut out = String::with_capacity(line.len() + new_command.len());
    out.push_str(&line[..span.start]);
    out.push_str(&escape_dash_c_arg(new_command));
    out.push_str(&line[span.end..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_trims_and_limits() {
        assert_eq!(
            slugify("@faye rfc generate-linkedin-post"),
            "faye-rfc-generate-linkedin-post"
        );
        assert_eq!(slugify("hello   world!"), "hello-world");
        let long = "a".repeat(80);
        assert!(slugify(&long).len() <= 40);
    }

    #[test]
    fn tag_round_trip_with_timeout() {
        let t = EntryTag {
            agent: "faye".into(),
            slug: "rfc-generate-post".into(),
            timeout: Some("60m".into()),
        };
        let line = t.render();
        let parsed = parse_tag(&line).expect("re-parse");
        assert_eq!(parsed.agent, "faye");
        assert_eq!(parsed.slug, "rfc-generate-post");
        assert_eq!(parsed.timeout.as_deref(), Some("60m"));
    }

    #[test]
    fn parse_command_line_extracts_cron_and_body() {
        let raw = r#"0 9 * * MON /usr/bin/env ORKIA_SCHEDULED=1 /home/u/.orkia/bin/orkia -c "@faye rfc generate""#;
        let parsed = parse_command_line(raw).expect("parse");
        assert_eq!(parsed.cron, "0 9 * * MON");
        assert_eq!(parsed.command, "@faye rfc generate");
    }

    #[test]
    fn parse_command_line_handles_timeout_flag_between_c_and_quoted_arg() {
        // Regression: an earlier extractor took the first token after
        // `-c ` and ended up returning "--timeout" as the command.
        let raw = r#"0 9 * * MON /usr/bin/env ORKIA_SCHEDULED=1 ORKIA_SCHEDULED_CRON="0 9 * * MON" /opt/orkia -c --timeout 1800 "@strict produce""#;
        let parsed = parse_command_line(raw).expect("parse");
        assert_eq!(parsed.cron, "0 9 * * MON");
        assert_eq!(parsed.command, "@strict produce");
    }

    #[test]
    fn enumerate_and_remove_preserves_foreign_lines() {
        let body = "\
# user's own job
*/15 * * * * /usr/local/bin/healthcheck
# orkia:faye:rfc-x
0 9 * * MON /opt/orkia -c \"@faye rfc x\"
# orkia:sage:check
0 8 * * 1-5 /opt/orkia -c \"@sage check\"
";
        let mut crontab = Crontab {
            lines: body.lines().map(str::to_string).collect(),
        };
        let entries = crontab.orkia_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].agent, "faye");
        assert_eq!(entries[1].agent, "sage");

        let removed = crontab.remove_orkia_at(1).expect("remove");
        assert_eq!(removed.agent, "faye");
        let after = crontab.orkia_entries();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].agent, "sage");
        // foreign healthcheck job remains
        assert!(crontab.lines.iter().any(|l| l.contains("healthcheck")));
    }

    #[test]
    fn pause_and_resume_toggle_prefix() {
        let body = "\
# orkia:sage:check
0 8 * * 1-5 /opt/orkia -c \"@sage check\"
";
        let mut crontab = Crontab {
            lines: body.lines().map(str::to_string).collect(),
        };
        let paused = crontab.set_paused_at(1, true).expect("pause");
        assert!(paused.paused);
        assert!(crontab.lines[1].starts_with("# PAUSED: "));

        // Idempotent: pausing again does not double-prefix.
        let _ = crontab.set_paused_at(1, true);
        assert_eq!(
            crontab.lines[1].matches("# PAUSED: ").count(),
            1,
            "must not double-prefix on repeat pause"
        );

        let resumed = crontab.set_paused_at(1, false).expect("resume");
        assert!(!resumed.paused);
        assert!(!crontab.lines[1].starts_with("# PAUSED: "));
    }
}
