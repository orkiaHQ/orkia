// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! One-time crontab migration: append `--once` to pre-existing AGENT
//!
//! Entries written before `--once` existed dispatch a bare `@agent …`,
//! which under persistent-by-default semantics never exits — every cron
//! firing leaves a hung `orkia -c` process behind. `handle_create` now
//! appends `--once` at creation time; this migration retrofits the
//! entries already in the user's spool.
//!
//! Called from the interactive shell boot only (never from `-c` runs —
//! concurrent cron firings racing `crontab -` could clobber each other).
//! Best-effort and idempotent: an entry whose command already carries a
//! `--once` token is left untouched, as are `shell` schedules (they exit
//! on their own) and every non-orkia line.

use super::crontab::{Crontab, rewrite_dash_c_arg};

/// Load the user's crontab, retrofit `--once` onto agent entries, and
/// save it back only when something changed. Returns the number of
/// migrated entries. A missing/unreadable crontab is not an error — there
/// is nothing to migrate.
pub fn migrate_missing_once() -> Result<usize, String> {
    let mut crontab = Crontab::load()?;
    let migrated = add_once_to_agent_entries(&mut crontab);
    if migrated > 0 {
        crontab.save()?;
    }
    Ok(migrated)
}

/// Append `--once` to every orkia AGENT entry whose command lacks the
/// token. Pure rewrite over the in-memory lines; returns how many command
/// lines changed. A `# PAUSED: ` prefix is preserved (the entry stays
/// paused, just correct when resumed).
fn add_once_to_agent_entries(crontab: &mut Crontab) -> usize {
    let mut migrated = 0;
    // Walk tag/command line pairs the same way `orkia_entries` does, but
    // mutate the raw command line in place so every surrounding byte
    // (env prefix, flags, PAUSED marker) survives untouched.
    for entry in crontab.orkia_entries() {
        if entry.agent == "shell" || has_once_token(&entry.command) {
            continue;
        }
        let new_command = format!("{} --once", entry.command);
        if rewrite_entry_command(crontab, &entry.slug, &new_command) {
            migrated += 1;
        }
    }
    migrated
}

/// Whitespace-token scan for `--once` — mirrors the shell's
/// `line_requests_once` (a substring match would false-positive on e.g.
/// `--once-a-day` in a command body).
fn has_once_token(command: &str) -> bool {
    command.split_whitespace().any(|t| t == "--once")
}

/// Rewrite the command line right below the `# orkia:…:<slug>` tag.
/// Returns false when the slug or a well-formed `-c "…"` argument can't
/// be found (malformed entry — leave it alone, never corrupt the spool).
fn rewrite_entry_command(crontab: &mut Crontab, slug: &str, new_command: &str) -> bool {
    let needle = format!(":{slug}");
    for i in 0..crontab.lines.len().saturating_sub(1) {
        let is_tag = crontab.lines[i].starts_with("# orkia:") && crontab.lines[i].contains(&needle);
        if !is_tag {
            continue;
        }
        if let Some(rewritten) = rewrite_dash_c_arg(&crontab.lines[i + 1], new_command) {
            crontab.lines[i + 1] = rewritten;
            return true;
        }
        return false;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crontab_from(body: &str) -> Crontab {
        Crontab {
            lines: body.lines().map(str::to_string).collect(),
        }
    }

    #[test]
    fn legacy_agent_entry_gains_once() {
        let mut c = crontab_from(
            "# orkia:faye:rfc-post\n\
             0 9 * * MON /usr/bin/env ORKIA_SCHEDULED=1 /opt/orkia -c \"@faye rfc post\"\n",
        );
        assert_eq!(add_once_to_agent_entries(&mut c), 1);
        assert!(c.lines[1].ends_with("\"@faye rfc post --once\""));
        // Idempotent: a second pass changes nothing.
        assert_eq!(add_once_to_agent_entries(&mut c), 0);
    }

    #[test]
    fn shell_entry_and_oncey_substring_are_untouched() {
        let body = "\
# orkia:shell:backup\n\
0 3 * * * /usr/bin/env ORKIA_SCHEDULED=1 /opt/orkia -c \"backup.sh --full\"\n\
# orkia:faye:already\n\
0 9 * * MON /opt/orkia -c \"@faye post --once\"\n";
        let mut c = crontab_from(body);
        assert_eq!(add_once_to_agent_entries(&mut c), 0);
        assert_eq!(c.lines.join("\n") + "\n", body);
    }

    #[test]
    fn once_like_substring_still_migrates() {
        let mut c = crontab_from(
            "# orkia:faye:digest\n\
             0 9 * * * /opt/orkia -c \"@faye send the --once-a-day digest\"\n",
        );
        assert_eq!(add_once_to_agent_entries(&mut c), 1);
        assert!(c.lines[1].ends_with("digest --once\""));
    }

    #[test]
    fn paused_entry_migrates_and_stays_paused() {
        let mut c = crontab_from(
            "# orkia:sage:check\n\
             # PAUSED: 0 8 * * 1-5 /opt/orkia -c \"@sage check\"\n",
        );
        assert_eq!(add_once_to_agent_entries(&mut c), 1);
        assert!(c.lines[1].starts_with("# PAUSED: "));
        assert!(c.lines[1].ends_with("\"@sage check --once\""));
        let entry = &c.orkia_entries()[0];
        assert!(entry.paused);
        assert_eq!(entry.command, "@sage check --once");
    }

    #[test]
    fn escaped_quotes_in_command_survive_rewrite() {
        // render_command_line escapes `"` → `\"`; the rewrite must keep
        // the escaping intact and re-escape the appended form.
        let mut c = crontab_from(
            "# orkia:faye:say\n\
             0 9 * * * /opt/orkia -c \"@faye say \\\"hello\\\"\"\n",
        );
        assert_eq!(add_once_to_agent_entries(&mut c), 1);
        let entry = &c.orkia_entries()[0];
        assert_eq!(entry.command, "@faye say \"hello\" --once");
    }

    #[test]
    fn foreign_lines_and_timeout_flags_are_preserved() {
        let mut c = crontab_from(
            "# user's own job\n\
             */15 * * * * /usr/local/bin/healthcheck\n\
             # orkia:strict:produce:timeout=30m\n\
             0 9 * * MON /usr/bin/env ORKIA_SCHEDULED=1 /opt/orkia -c --timeout 1800 \"@strict produce\"\n",
        );
        assert_eq!(add_once_to_agent_entries(&mut c), 1);
        assert!(c.lines[1].contains("healthcheck"));
        assert!(c.lines[3].contains("-c --timeout 1800 \"@strict produce --once\""));
    }
}
