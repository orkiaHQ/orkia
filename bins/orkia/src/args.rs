// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

/// Parsed command-line flags. Hand-rolled to avoid pulling in clap for
/// what is fundamentally a four-option surface.
#[derive(Default)]
pub(crate) struct Args {
    /// Execute the given string and exit. Composes with brush's `run_string`
    /// — multi-statement input (e.g. `cd src && cargo build`) works the
    /// same way it would interactively.
    pub(crate) command: Option<String>,
    /// Force TUI mode at launch.
    pub(crate) tui: bool,
    /// Force shell/stdout mode at launch (back-compat with V4-1 `--no-tui`).
    pub(crate) no_tui: bool,
    /// Treat this orkia as a login shell (source `.profile` chain).
    /// Set automatically when argv[0] starts with `-` (`chsh -s` puts
    /// us in that mode) or when `--login` is passed explicitly.
    pub(crate) login: bool,
    /// Print version and exit.
    pub(crate) version: bool,
    /// Print usage and exit.
    pub(crate) help: bool,
    /// `--timeout <secs>` — hard cap on the entire `orkia -c` run.
    /// Set by the `every` builtin's crontab line so scheduled jobs
    /// can't hang the orkia process forever. Default (None) = no cap.
    pub(crate) timeout_secs: Option<u64>,
    /// `--audit` — opt-in audit for plain shell `-c` payloads. Agentic
    /// payloads already route through the REPL pipeline and SEAL.
    pub(crate) audit: bool,
    /// `--detach` — reserve the daemon-owned PTY mode. Parsed now so
    /// users get a precise error instead of "unknown argument".
    pub(crate) detach: bool,
    /// Subcommand routed at the CLI level (e.g. `orkia migrate-rc ...`).
    /// When `Some`, the REPL is not started.
    pub(crate) subcommand: Option<Subcommand>,
}

pub(crate) enum Subcommand {
    /// `orkia migrate-rc [--from PATH] [--dry-run] [--append] ...`
    MigrateRc(Vec<String>),
    /// `orkia setup [--minimal] [--force] [--offline]`
    Setup(Vec<String>),
    /// `orkia bridge --source <name>` — agent-hook shim to the journal
    /// socket. Reads payload on stdin; emits one NDJSON line. See
    /// `bridge.rs`. Not intended for direct human use.
    Bridge(Vec<String>),
    /// `orkia mcp-bridge` — stdio MCP bridge to Orkia socket tools.
    /// Spawned from an agent's generated `mcp-config.json`.
    McpBridge,
    /// `orkia mcp-pipe` — stdio MCP server exposing `submit_pipeline_output`.
    /// Spawned per pipeline stage from the stage agent's `mcp-config.json`
    /// as the output safety net. Context comes from env. See `mcp_pipe.rs`.
    McpPipe,
    /// Hidden PTY daemon that owns detached agent sessions.
    PtyDaemon,
    /// Hidden PTY daemon shutdown hook for integration tests / cleanup.
    PtyDaemonStop,
    /// `orkia ps` — top-level daemon job view for detached jobs.
    Ps(Vec<String>),
    /// `orkia attach <job_id>` — attach to a detached daemon job.
    Attach(Vec<String>),
    /// `orkia tell <job_id> <message>` — send input to a detached daemon job.
    Tell(Vec<String>),
    /// `orkia kill <job_id>` — terminate a detached daemon job.
    Kill(Vec<String>),
    /// `orkia stop <job_id>` — gracefully stop a detached daemon job.
    Stop(Vec<String>),
    /// `orkia wait <job_id> [--timeout SECS]` — wait for a daemon job terminal state.
    Wait(Vec<String>),
    /// `orkia inspect <job_id>` — show detailed daemon job diagnostics.
    Inspect(Vec<String>),
    /// `orkia logs <job_id> [--last N]` — show daemon SEAL/job log lines.
    Logs(Vec<String>),
    /// `orkia daemon status` — local daemon lifecycle diagnostics.
    Daemon(Vec<String>),
    /// `orkia journal [filters]` — query the unified journal at
    /// `~/.orkia/journal.jsonl`. Works without a running REPL.
    Journal(Vec<String>),
    /// `orkia every "<frequency>" [@agent] <command>` — schedule an
    /// agent run via crond. Also supports `list`, `remove <N>`,
    /// `pause <N>`, `resume <N>`. Works without a running REPL so
    /// scripts and one-shot `chsh`-less setups can schedule too.
    Every(Vec<String>),
    /// `orkia login` — GitHub OAuth flow. Stores the resulting token in
    /// the OS keychain (or file fallback). Forge V1.
    Login(Vec<String>),
    /// `orkia logout` — revokes the local token server-side and clears it.
    Logout(Vec<String>),
    /// `orkia whoami` — prints account + plan + usage from the backend.
    Whoami(Vec<String>),
    /// `orkia reasoning backfill <envelopes.jsonl> [--no-push] [--dry-run]`
    /// — stage a corpus of historical agent sessions into the reasoning
    /// graph and (by default) flush them to the cloud. Premium-gated.
    Reasoning(Vec<String>),
    /// `orkia update [--check] [--kernel]` — self-update the shell
    /// binaries from the signed `latest` release, or the kernel daemon.
    Update(Vec<String>),
}

pub(crate) fn parse_args(argv: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut out = Args::default();
    let mut iter = argv.into_iter().peekable();
    // argv[0] convention: a leading '-' (e.g. "-orkia") means the
    // process was started as a login shell by `login(1)` / `chsh -s`.
    if let Some(argv0) = iter.next()
        && let Some(base) = std::path::Path::new(&argv0)
            .file_name()
            .and_then(|s| s.to_str())
        && base.starts_with('-')
    {
        out.login = true;
    }
    // Subcommand path: remaining argv becomes the subcommand's args.
    // Subcommands short-circuit the rest of the parser.
    if let Some(sub) = try_parse_subcommand(&mut iter) {
        out.subcommand = Some(sub);
        return Ok(out);
    }
    parse_flags(&mut iter, &mut out)?;
    Ok(out)
}

/// Every verb `try_parse_subcommand` recognizes, colocated with its
/// match (the dispatch-arm-const discipline from orkia-shell). Each
/// name must be either a builtin-table entry (REPL twin) or a
/// `builtin_table::CLI_ONLY` verb (the REPL bridges `orkia <verb>` to
/// this binary via brush) — coverage is test-enforced below.
#[cfg(test)]
const SUBCOMMAND_NAMES: &[&str] = &[
    "migrate-rc",
    "setup",
    "bridge",
    "mcp-bridge",
    "mcp-pipe",
    "pty-daemon",
    "pty-daemon-stop",
    "ps",
    "attach",
    "tell",
    "kill",
    "stop",
    "wait",
    "inspect",
    "logs",
    "daemon",
    "journal",
    "every",
    "login",
    "logout",
    "whoami",
    "reasoning",
    "update",
];

/// If the next token names a known subcommand, consume it and the rest
/// of the iterator, returning the `Subcommand` variant. Otherwise no
/// tokens are consumed and `None` is returned.
fn try_parse_subcommand(
    iter: &mut std::iter::Peekable<impl Iterator<Item = String>>,
) -> Option<Subcommand> {
    let sub = match iter.peek().map(String::as_str) {
        Some("migrate-rc") => {
            iter.next();
            Subcommand::MigrateRc(iter.collect())
        }
        Some("setup") => {
            iter.next();
            Subcommand::Setup(iter.collect())
        }
        Some("bridge") => {
            iter.next();
            Subcommand::Bridge(iter.collect())
        }
        Some("mcp-bridge") => {
            iter.next();
            Subcommand::McpBridge
        }
        Some("mcp-pipe") => {
            iter.next();
            Subcommand::McpPipe
        }
        Some("pty-daemon") => {
            iter.next();
            Subcommand::PtyDaemon
        }
        Some("pty-daemon-stop") => {
            iter.next();
            Subcommand::PtyDaemonStop
        }
        Some("ps") => {
            iter.next();
            Subcommand::Ps(iter.collect())
        }
        Some("attach") => {
            iter.next();
            Subcommand::Attach(iter.collect())
        }
        Some("tell") => {
            iter.next();
            Subcommand::Tell(iter.collect())
        }
        Some("kill") => {
            iter.next();
            Subcommand::Kill(iter.collect())
        }
        Some("stop") => {
            iter.next();
            Subcommand::Stop(iter.collect())
        }
        Some("wait") => {
            iter.next();
            Subcommand::Wait(iter.collect())
        }
        Some("inspect") => {
            iter.next();
            Subcommand::Inspect(iter.collect())
        }
        Some("logs") => {
            iter.next();
            Subcommand::Logs(iter.collect())
        }
        Some("daemon") => {
            iter.next();
            Subcommand::Daemon(iter.collect())
        }
        Some("journal") => {
            iter.next();
            Subcommand::Journal(iter.collect())
        }
        Some("every") => {
            iter.next();
            Subcommand::Every(iter.collect())
        }
        Some("login") => {
            iter.next();
            Subcommand::Login(iter.collect())
        }
        Some("logout") => {
            iter.next();
            Subcommand::Logout(iter.collect())
        }
        Some("whoami") => {
            iter.next();
            Subcommand::Whoami(iter.collect())
        }
        Some("reasoning") => {
            iter.next();
            Subcommand::Reasoning(iter.collect())
        }
        Some("update") => {
            iter.next();
            Subcommand::Update(iter.collect())
        }
        _ => return None,
    };
    Some(sub)
}

/// Parse the remaining flags/options into `out`.
fn parse_flags(iter: &mut impl Iterator<Item = String>, out: &mut Args) -> Result<(), String> {
    while let Some(a) = iter.next() {
        match a.as_str() {
            "-c" => {
                let cmd = iter
                    .next()
                    .ok_or_else(|| "missing argument to -c".to_string())?;
                out.command = Some(cmd);
            }
            "--tui" => out.tui = true,
            "--no-tui" => out.no_tui = true,
            "--login" | "-l" => out.login = true,
            "-V" | "--version" => out.version = true,
            "-h" | "--help" => out.help = true,
            "--timeout" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| "missing argument to --timeout".to_string())?;
                let secs: u64 = raw
                    .parse()
                    .map_err(|_| format!("--timeout: expected integer seconds, got `{raw}`"))?;
                out.timeout_secs = Some(secs);
            }
            "--audit" => out.audit = true,
            "--detach" => out.detach = true,
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if out.tui && out.no_tui {
        return Err("--tui and --no-tui are mutually exclusive".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Args {
        let argv = std::iter::once("orkia").chain(words.iter().copied());
        parse_args(argv.map(str::to_string)).expect("parse")
    }

    #[test]
    fn every_listed_subcommand_parses() {
        for name in SUBCOMMAND_NAMES {
            assert!(
                parse(&[name]).subcommand.is_some(),
                "`{name}` is in SUBCOMMAND_NAMES but doesn't parse as a subcommand"
            );
        }
        // Flags are not subcommands.
        assert!(parse(&["-V"]).subcommand.is_none());
    }

    #[test]
    fn every_subcommand_is_reachable_from_the_repl() {
        // errors in-process for unknown names. Every CLI verb must
        // therefore be either a REPL builtin (table entry) or a
        // CLI_ONLY bridge name (the classifier routes `orkia <verb>` to
        // brush → this binary). Adding a subcommand without covering it
        // breaks this test, not the user.
        use orkia_shell::builtin_table::{CLI_ONLY, spec_for};
        for name in SUBCOMMAND_NAMES {
            assert!(
                spec_for(name).is_some() || CLI_ONLY.contains(name),
                "CLI subcommand `{name}` is unreachable from the REPL: add it to \
                 builtin_table::CLI_ONLY (or give it a REPL dispatch arm)"
            );
        }
        // Reverse direction: a CLI_ONLY name must be a real CLI verb.
        for name in CLI_ONLY {
            assert!(
                SUBCOMMAND_NAMES.contains(name),
                "builtin_table::CLI_ONLY lists `{name}` but the CLI doesn't parse it"
            );
        }
    }
}
