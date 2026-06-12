// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia every` — natural-language cron builtin.
//!
//! Translates `orkia every "monday 9am" @faye rfc post` into a valid
//! crontab line that invokes `orkia -c "@faye rfc post"` on schedule.
//! crond remains the scheduler. We never run a daemon, never spawn
//! a tokio task — this builtin is a synchronous read-modify-write
//! over the user's crontab and exits.
//!

use std::io::{self, BufRead, Write};
use std::path::Path;

use orkia_shell_types::BlockContent;

pub mod binary_path;
pub mod crontab;
pub mod display;
pub mod migrate;
pub mod parse;

use binary_path::resolve_orkia_binary;
use crontab::{Crontab, EntryTag, slugify};
use parse::CronExpr;

/// Public entry point. The REPL's `dispatch_named("every", …)` dispatches
/// here; so does the `orkia every ...` CLI subcommand in main.rs.
///
/// `data_dir` is the orkia data root (usually `~/.orkia`) — used only
/// to check that referenced agents exist.
pub fn every(args: &[String], data_dir: &Path) -> Vec<BlockContent> {
    let parsed = match parse_args(args) {
        Ok(a) => a,
        Err(e) => return display::render_error(e),
    };
    match parsed {
        Action::Help => render_help(),
        Action::List => handle_list(),
        Action::Remove(n) => handle_remove(n),
        Action::Pause(n) => handle_pause(n, true),
        Action::Resume(n) => handle_pause(n, false),
        Action::Create(spec) => handle_create(&spec, data_dir),
    }
}

// ─── CLI parsing ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CreateSpec {
    /// Natural-language frequency, e.g. `"monday 9am"`.
    frequency: String,
    /// Target agent (without the `@`) or `"shell"` for bare commands.
    agent: String,
    /// The command body that follows the `@agent` (or the whole
    /// command, when no `@agent` was given).
    command: String,
    /// `--timeout 60m` if provided. Stored verbatim in the tag.
    timeout: Option<String>,
}

#[derive(Debug, Clone)]
enum Action {
    Help,
    List,
    Remove(usize),
    Pause(usize),
    Resume(usize),
    Create(CreateSpec),
}

fn parse_args(args: &[String]) -> Result<Action, String> {
    let first = match args.first() {
        Some(a) => a.as_str(),
        None => return Ok(Action::Help),
    };
    match first {
        "--help" | "-h" | "help" => Ok(Action::Help),
        "list" | "ls" => Ok(Action::List),
        "remove" | "rm" | "delete" => Ok(Action::Remove(parse_index(&args[1..])?)),
        "pause" => Ok(Action::Pause(parse_index(&args[1..])?)),
        "resume" => Ok(Action::Resume(parse_index(&args[1..])?)),
        _ => parse_create(args).map(Action::Create),
    }
}

fn parse_index(rest: &[String]) -> Result<usize, String> {
    let Some(raw) = rest.first() else {
        return Err("usage: orkia every <remove|pause|resume> <number>".into());
    };
    raw.parse::<usize>()
        .map_err(|_| format!("expected a job number, got `{raw}`"))
        .and_then(|n| {
            if n == 0 {
                Err("job numbers are 1-based; use `every list` to see them".into())
            } else {
                Ok(n)
            }
        })
}

fn parse_create(args: &[String]) -> Result<CreateSpec, String> {
    // Pop optional `--timeout <dur>` from anywhere in the arg list.
    let mut filtered: Vec<String> = Vec::with_capacity(args.len());
    let mut timeout = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--timeout" {
            let v = args
                .get(i + 1)
                .cloned()
                .ok_or_else(|| "--timeout requires an argument like 30m or 3600s".to_string())?;
            timeout = Some(v);
            i += 2;
        } else {
            filtered.push(args[i].clone());
            i += 1;
        }
    }

    let frequency = filtered
        .first()
        .cloned()
        .ok_or_else(|| "usage: orkia every \"<frequency>\" <command>".to_string())?;
    if filtered.len() < 2 {
        return Err("usage: orkia every \"<frequency>\" <command>".into());
    }
    let rest = &filtered[1..];
    let (agent, command) = if let Some(first) = rest.first().and_then(|s| s.strip_prefix('@')) {
        let body = rest[1..].join(" ");
        if body.is_empty() {
            return Err("missing command body after @agent".into());
        }
        (first.to_string(), body)
    } else {
        ("shell".to_string(), rest.join(" "))
    };

    Ok(CreateSpec {
        frequency,
        agent,
        command,
        timeout,
    })
}

// ─── Actions ───────────────────────────────────────────────────────────

fn handle_create(spec: &CreateSpec, data_dir: &Path) -> Vec<BlockContent> {
    if spec.agent != "shell" && !agent_exists(data_dir, &spec.agent) {
        return display::render_error(format!(
            "Agent '{}' not found. Run `orkia agent list` to see available agents.",
            spec.agent,
        ));
    }

    // 2. Parse the natural-language frequency.
    let cron = match parse::parse(&spec.frequency) {
        Ok(c) => c,
        Err(e) => return display::render_error(e.to_string()),
    };

    // 3. Validate the synthesised expression via the `cron` crate
    //    (paranoia — protects against a future rule that produces a
    //    malformed string).
    if let Err(e) = validate_cron(&cron) {
        return display::render_error(format!("internal: synthesised invalid cron `{cron}`: {e}"));
    }

    // 4. Resolve the orkia binary path. We bake the absolute path into
    //    the crontab line because crond has a minimal PATH.
    let bin = match resolve_orkia_binary() {
        Ok(p) => p,
        Err(e) => return display::render_error(e),
    };

    // 5. Build the user-visible command (`@agent body` or just body).
    let user_command = if spec.agent == "shell" {
        spec.command.clone()
    } else {
        format!("@{} {}", spec.agent, spec.command)
    };

    // 5b. The scheduled command appends `--once` for AGENT schedules.
    let scheduled_command = scheduled_command(&spec.agent, &user_command);

    // 6. Build the command line that crond will execute. We always go
    //    through `env ORKIA_SCHEDULED=1 ...` so the dispatch layer can
    //    tag SEAL records with `origin: "scheduled"`. When the agent
    //    has `[trust] approval = "required"`, also export
    //    `ORKIA_SCHEDULED_APPROVAL=required` so the seal consumer
    //    parks the result instead of letting it take effect.
    let cron_line = cron.to_line();
    let approval = spec.agent != "shell" && agent_requires_approval(data_dir, &spec.agent);
    let command_line = render_command_line(&CommandLineSpec {
        bin: &bin,
        user_command: &scheduled_command,
        timeout: spec.timeout.as_deref(),
        approval_required: approval,
        cron_line: &cron_line,
    });

    // 7. Read crontab, refuse duplicates, append, write back.
    let mut crontab = match Crontab::load() {
        Ok(c) => c,
        Err(e) => return display::render_error(e),
    };

    if let Some(existing) = crontab
        .orkia_entries()
        .iter()
        .find(|e| e.agent == spec.agent && e.command == scheduled_command && e.cron == cron_line)
        && !confirm_duplicate(existing)
    {
        return display::render_error("aborted: duplicate schedule");
    }

    let slug_source = format!("{} {}", spec.agent, spec.command);
    let tag = EntryTag {
        agent: spec.agent.clone(),
        slug: slugify(&slug_source),
        timeout: spec.timeout.clone(),
    };
    crontab.append_orkia(&tag, &cron_line, &command_line);
    if let Err(e) = crontab.save() {
        return display::render_error(e);
    }

    display::render_created(&cron_line, &command_line)
}

fn handle_list() -> Vec<BlockContent> {
    match Crontab::load() {
        Ok(c) => display::render_list(&c.orkia_entries()),
        Err(e) => display::render_error(e),
    }
}

fn handle_remove(n: usize) -> Vec<BlockContent> {
    let mut crontab = match Crontab::load() {
        Ok(c) => c,
        Err(e) => return display::render_error(e),
    };
    let Some(removed) = crontab.remove_orkia_at(n) else {
        return display::render_error(format!(
            "no orkia schedule at index {n} (use `every list` to see them)"
        ));
    };
    if let Err(e) = crontab.save() {
        return display::render_error(e);
    }
    display::render_removed(&removed)
}

fn handle_pause(n: usize, pause: bool) -> Vec<BlockContent> {
    let mut crontab = match Crontab::load() {
        Ok(c) => c,
        Err(e) => return display::render_error(e),
    };
    let Some(entry) = crontab.set_paused_at(n, pause) else {
        return display::render_error(format!(
            "no orkia schedule at index {n} (use `every list` to see them)"
        ));
    };
    if let Err(e) = crontab.save() {
        return display::render_error(e);
    }
    if pause {
        display::render_paused(&entry)
    } else {
        display::render_resumed(&entry)
    }
}

fn render_help() -> Vec<BlockContent> {
    vec![BlockContent::SystemInfo(
        "orkia every — schedule agents via crond\n\n\
         USAGE:\n\
         \x20 orkia every \"<frequency>\" [@<agent>] <command> [--timeout <dur>]\n\
         \x20 orkia every list\n\
         \x20 orkia every remove <number>\n\
         \x20 orkia every pause  <number>\n\
         \x20 orkia every resume <number>\n\n\
         FREQUENCY EXAMPLES:\n\
         \x20 \"monday 9am\"        \"weekdays 8am\"     \"every 2 hours\"\n\
         \x20 \"daily 5pm\"         \"weekends 10am\"    \"every 30 minutes\"\n\
         \x20 \"1st of month\"      \"15th of month\"    \"twice a day\"\n"
            .into(),
    )]
}

// ─── Helpers ───────────────────────────────────────────────────────────

/// Check whether `<data_dir>/agents/<name>/agent.toml` exists. Mirrors
/// what `orkia_shell::agent_dir::load_definition_by_name` does, but
/// kept inline so this crate doesn't take a dep on orkia-shell.
fn agent_exists(data_dir: &Path, name: &str) -> bool {
    data_dir
        .join("agents")
        .join(name)
        .join("agent.toml")
        .exists()
}

/// Read the target agent's `agent.toml` and return true when
/// `[trust] approval = "required"` (or a synonym). Used by the create
/// path to bake `ORKIA_SCHEDULED_APPROVAL=required` into the crontab
/// line, so the seal consumer can park results at fire time without
/// re-reading the agent registry. Best-effort: missing / malformed
/// files fall back to `false` — the runtime then runs the agent
/// as if approval weren't required, matching the default.
fn agent_requires_approval(data_dir: &Path, name: &str) -> bool {
    let path = data_dir.join("agents").join(name).join("agent.toml");
    let Ok(body) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(parsed) = toml::from_str::<orkia_shell_types::AgentConfigFile>(&body) else {
        return false;
    };
    matches!(
        parsed
            .trust
            .approval
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("required") | Some("require") | Some("yes") | Some("true"),
    )
}

/// Append `--once` to an AGENT schedule's command — a cron run is one-shot and
/// would hang the cron process forever. A `shell` schedule exits on its own and
/// is left untouched. The slug/tag are derived from the pre-`--once` command, so
/// the tag stays stable.
fn scheduled_command(agent: &str, user_command: &str) -> String {
    if agent == "shell" {
        user_command.to_string()
    } else {
        format!("{user_command} --once")
    }
}

/// Arguments for building the crontab command-line string.
struct CommandLineSpec<'a> {
    bin: &'a Path,
    user_command: &'a str,
    timeout: Option<&'a str>,
    approval_required: bool,
    cron_line: &'a str,
}

/// Build the `... orkia -c "..."` portion of the crontab line. The
/// `env ORKIA_SCHEDULED=1` prefix is what the dispatch layer reads to
fn render_command_line(spec: &CommandLineSpec<'_>) -> String {
    let bin = spec.bin;
    let user_command = spec.user_command;
    let timeout = spec.timeout;
    let approval_required = spec.approval_required;
    let cron_line = spec.cron_line;
    let bin_disp = bin.display();
    let timeout_flag = match timeout.and_then(parse_duration_secs) {
        Some(secs) => format!(" --timeout {secs}"),
        None => String::new(),
    };
    let mut envs = String::from("ORKIA_SCHEDULED=1");
    if approval_required {
        envs.push_str(" ORKIA_SCHEDULED_APPROVAL=required");
    }
    // crontab itself doesn't expand %; the % char is interpreted as
    // a newline in the command field. Replace any % with a literal
    // backslash-percent so the env value survives. Spaces inside the
    // cron expression don't matter because we double-quote.
    let safe_cron = cron_line.replace('%', "\\%");
    envs.push_str(&format!(" ORKIA_SCHEDULED_CRON=\"{safe_cron}\""));
    // Escape rules shared with the migration rewriter — see
    // `crontab::escape_dash_c_arg` (BUG-030/031).
    let safe_cmd = crontab::escape_dash_c_arg(user_command);
    format!("/usr/bin/env {envs} {bin_disp} -c{timeout_flag} \"{safe_cmd}\"")
}

/// Accept a duration like `30m`, `60m`, `2h`, `3600s`, or a bare
/// integer (treated as seconds). Returns total seconds or None.
fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let unit = s.chars().last()?;
    let digits = &s[..s.len() - unit.len_utf8()];
    let n: u64 = digits.parse().ok()?;
    let mult = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// Re-validate a synthesised [`CronExpr`] via the `cron` crate. The
/// crate expects 7-field Quartz syntax (sec + 5 + year), so we adapt
/// by prepending `0 ` and appending ` *`.
fn validate_cron(c: &CronExpr) -> Result<(), String> {
    use std::str::FromStr;
    let quartz = format!("0 {} *", c.to_line());
    cron::Schedule::from_str(&quartz)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Prompt for `[y/n]` confirmation on a duplicate-schedule create.
/// Only consults stdin when it's a TTY; otherwise refuses (safer
/// default for cron-style invocations).
fn confirm_duplicate(existing: &crontab::OrkiaEntry) -> bool {
    use std::io::IsTerminal;
    eprintln!(
        "schedule already exists: {} @{} {}",
        existing.cron, existing.agent, existing.command,
    );
    if !io::stdin().is_terminal() {
        eprintln!(
            "(non-interactive — aborting; re-run with a different schedule or remove the duplicate first)"
        );
        return false;
    }
    eprint!("Add anyway? [y/N] ");
    let _ = io::stderr().flush();
    let mut buf = String::new();
    if io::stdin().lock().read_line(&mut buf).is_err() {
        return false;
    }
    matches!(buf.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_create_with_agent() {
        let args: Vec<String> = ["monday 9am", "@faye", "rfc", "generate-linkedin-post"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let spec = match parse_args(&args).unwrap() {
            Action::Create(s) => s,
            _ => panic!("expected Create"),
        };
        assert_eq!(spec.frequency, "monday 9am");
        assert_eq!(spec.agent, "faye");
        assert_eq!(spec.command, "rfc generate-linkedin-post");
        assert!(spec.timeout.is_none());
    }

    #[test]
    fn parse_create_with_timeout_flag_anywhere() {
        let args: Vec<String> = ["monday 9am", "--timeout", "60m", "@faye", "rfc", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let spec = match parse_args(&args).unwrap() {
            Action::Create(s) => s,
            _ => panic!(),
        };
        assert_eq!(spec.timeout.as_deref(), Some("60m"));
        assert_eq!(spec.command, "rfc x");
    }

    #[test]
    fn parse_create_without_agent_defaults_to_shell() {
        let args: Vec<String> = ["weekdays 8am", "/usr/local/bin/backup"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let spec = match parse_args(&args).unwrap() {
            Action::Create(s) => s,
            _ => panic!(),
        };
        assert_eq!(spec.agent, "shell");
        assert_eq!(spec.command, "/usr/local/bin/backup");
    }

    #[test]
    fn parse_subcommands() {
        let to_args = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(matches!(
            parse_args(&to_args(&["list"])).unwrap(),
            Action::List
        ));
        assert!(matches!(
            parse_args(&to_args(&["remove", "2"])).unwrap(),
            Action::Remove(2)
        ));
        assert!(matches!(
            parse_args(&to_args(&["pause", "3"])).unwrap(),
            Action::Pause(3)
        ));
        assert!(matches!(
            parse_args(&to_args(&["resume", "1"])).unwrap(),
            Action::Resume(1)
        ));
        assert!(parse_args(&to_args(&["remove", "0"])).is_err());
        assert!(parse_args(&to_args(&["pause"])).is_err());
    }

    #[test]
    fn duration_parsing_covers_common_units() {
        assert_eq!(parse_duration_secs("60"), Some(60));
        assert_eq!(parse_duration_secs("30s"), Some(30));
        assert_eq!(parse_duration_secs("5m"), Some(300));
        assert_eq!(parse_duration_secs("2h"), Some(7200));
        assert_eq!(parse_duration_secs("1d"), Some(86_400));
        assert!(parse_duration_secs("nope").is_none());
    }

    #[test]
    fn duration_parsing_rejects_malformed_without_panic() {
        // Empty / whitespace-only: no `s.len() - 1` underflow.
        assert!(parse_duration_secs("").is_none());
        assert!(parse_duration_secs("   ").is_none());
        // Multibyte trailing char: no mid-char-boundary split panic.
        assert!(parse_duration_secs("5€").is_none());
        assert!(parse_duration_secs("€").is_none());
        // Multiplication overflow: returns None instead of wrapping/panicking.
        assert!(parse_duration_secs("9999999999999999999d").is_none());
    }

    #[test]
    fn render_command_line_uses_env_wrapper() {
        let bin = PathBuf::from("/opt/orkia");
        let line = render_command_line(&CommandLineSpec {
            bin: &bin,
            user_command: "@faye rfc post",
            timeout: Some("60m"),
            approval_required: false,
            cron_line: "0 9 * * MON",
        });
        assert!(line.starts_with("/usr/bin/env ORKIA_SCHEDULED=1 "));
        assert!(line.contains("ORKIA_SCHEDULED_CRON=\"0 9 * * MON\""));
        assert!(line.contains(" /opt/orkia -c --timeout 3600 \""));
        assert!(line.ends_with("\"@faye rfc post\""));
        assert!(!line.contains("ORKIA_SCHEDULED_APPROVAL"));
    }

    #[test]
    fn render_command_line_emits_approval_env_when_required() {
        let bin = PathBuf::from("/opt/orkia");
        let line = render_command_line(&CommandLineSpec {
            bin: &bin,
            user_command: "@faye rfc post",
            timeout: None,
            approval_required: true,
            cron_line: "0 9 * * MON",
        });
        assert!(line.contains("ORKIA_SCHEDULED_APPROVAL=required"));
    }

    #[test]
    fn scheduled_command_appends_once_for_agents_only() {
        // Agent schedules become one-shot.
        assert_eq!(
            scheduled_command("faye", "@faye rfc post"),
            "@faye rfc post --once"
        );
        // Shell schedules exit on their own — untouched.
        assert_eq!(
            scheduled_command("shell", "backup.sh --full"),
            "backup.sh --full"
        );
    }

    #[test]
    fn validate_cron_accepts_well_formed_expressions() {
        let c = parse::parse("monday 9am").unwrap();
        validate_cron(&c).expect("monday 9am must validate");
        let c = parse::parse("every 5 minutes").unwrap();
        validate_cron(&c).expect("every 5 minutes must validate");
    }
}
