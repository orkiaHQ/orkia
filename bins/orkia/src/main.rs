// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `orkia` binary entry point.
//!
//! Three runtime shapes:
//!   * `orkia -c "<cmd>"`  — non-interactive. Boots the brush engine,
//!     sources `~/.orkiarc`, runs the command, exits with its status.
//!     This is the `chsh`/`ssh user@host 'cmd'`/cron safety net.
//!   * `orkia --tui`       — launches directly in the ratatui alternate
//!     screen (sidebar, blocks, PTY widget).
//!   * `orkia` (default)   — stdin/stdout shell mode, like zsh. Native
//!     terminal scrollback + selection preserved. If stdin isn't a TTY
//!     (piped input), falls through to the minimal `StdoutRenderer`.

use std::io::IsTerminal;
use std::path::PathBuf;

use orkia_shell::{ShellConfig, ShellModeRenderer, StdoutRenderer};
use orkia_shell_tui::TuiRenderer;
use orkia_shell_types::Workspace;

mod args;
mod auth_cli;
mod bridge;
mod daemon_cli;
mod dash_c;
mod dispatch_wiring;
mod forge_wiring;
mod journal_cli;
mod mcp_bridge;
mod mcp_pipe;
mod pipeline_wiring;
mod pty_daemon;
mod repl_helpers;
mod seal_wiring;
mod setup;
mod signals;
mod tracing_setup;
mod update_cli;

use args::{Args, Subcommand, parse_args};
use repl_helpers::{ReplWiring, build_capability_wiring, build_repl, make_tui_factory, run_repl};
use signals::install_sigint_swallow;
use tracing_setup::init_tracing;

#[tokio::main]
async fn main() {
    init_tracing();
    install_sigint_swallow();
    let argv: Vec<String> = std::env::args().collect();
    let args = match parse_args(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("orkia: {e}");
            print_usage();
            std::process::exit(2);
        }
    };
    if args.help {
        print_usage();
        return;
    }
    if args.version {
        println!("orkia {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // CLI subcommands: run before ShellConfig::load so `orkia setup` works
    // on an unconfigured box.
    if let Some(sub) = args.subcommand {
        std::process::exit(dispatch_subcommand(sub).await);
    }
    // -c "cmd" → non-interactive, single-shot.
    let config = ShellConfig::load();
    if let Some(cmd) = args.command {
        std::process::exit(
            dash_c::run_dash_c(
                &cmd,
                &config,
                args.login,
                args.timeout_secs,
                args.audit,
                args.detach,
            )
            .await,
        );
    }
    // First-launch auto-prompt (interactive shells only).
    if std::io::stdin().is_terminal()
        && let Err(e) = setup::auto_prompt_if_missing()
    {
        eprintln!("orkia: setup: {e}");
    }
    // Retrofit `--once` onto pre-existing agent crontab entries
    // concurrent cron firings could race the `crontab -` write.
    match orkia_builtin::every::migrate::migrate_missing_once() {
        Ok(0) => {}
        Ok(n) => eprintln!("orkia: every: migrated {n} schedule(s) to --once (one-shot cron runs)"),
        Err(e) => tracing::debug!("every migration skipped: {e}"),
    }
    let config = ShellConfig::load();
    run_with(choose_renderer(&args, &config), config, args.login).await;
}

/// Dispatch CLI subcommands that short-circuit the REPL. Returns the exit code.
async fn dispatch_subcommand(sub: Subcommand) -> i32 {
    match sub {
        Subcommand::Bridge(a) => run_bridge_cli(&a).await,
        Subcommand::McpBridge => mcp_bridge::run().await,
        Subcommand::McpPipe => mcp_pipe::run().await,
        Subcommand::PtyDaemon => pty_daemon::run_server(ShellConfig::load()),
        Subcommand::PtyDaemonStop => daemon_cli::run_stop(),
        Subcommand::Login(a) => auth_cli::run_login(&a).await,
        Subcommand::Logout(a) => auth_cli::run_logout(&a).await,
        Subcommand::Whoami(a) => auth_cli::run_whoami(&a).await,
        Subcommand::Reasoning(a) => run_reasoning_cli(&a).await,
        Subcommand::Update(a) => update_cli::run(&a).await,
        other => run_subcommand(other),
    }
}

enum RendererKind {
    ShellMode,
    Tui,
    Stdout,
}

fn choose_renderer(args: &Args, config: &ShellConfig) -> RendererKind {
    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdout_is_tty = std::io::stdout().is_terminal();
    let interactive = stdin_is_tty && stdout_is_tty;
    let cfg_mode = config.default_mode.as_deref().map(str::to_ascii_lowercase);
    let want_tui = matches!(
        (args.tui, args.no_tui, cfg_mode.as_deref()),
        (true, _, _) | (_, _, Some("tui"))
    ) && !args.no_tui;
    if !interactive {
        RendererKind::Stdout
    } else if want_tui {
        RendererKind::Tui
    } else {
        RendererKind::ShellMode
    }
}

async fn run_with(kind: RendererKind, config: ShellConfig, login: bool) {
    match kind {
        RendererKind::ShellMode => {
            let (classifier, handle, resolver, auth) = build_capability_wiring();
            let repl = build_repl(
                ShellModeRenderer::new(),
                ReplWiring {
                    classifier,
                    config,
                    login,
                    resolver,
                    handle,
                    auth,
                    tui_factory: Some(make_tui_factory()),
                },
            );
            run_repl(repl).await;
        }
        RendererKind::Stdout => {
            let (classifier, handle, resolver, auth) = build_capability_wiring();
            let repl = build_repl(
                StdoutRenderer::new(),
                ReplWiring {
                    classifier,
                    config,
                    login,
                    resolver,
                    handle,
                    auth,
                    tui_factory: None,
                },
            );
            run_repl(repl).await;
        }
        RendererKind::Tui => {
            let workspace = Workspace::load(&config.data_dir);
            let agents = config.agents.clone();
            let renderer = match TuiRenderer::new(agents, workspace) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("orkia: failed to init TUI ({e}); falling back to shell mode");
                    let (classifier, handle, resolver, auth) = build_capability_wiring();
                    let repl = build_repl(
                        ShellModeRenderer::new(),
                        ReplWiring {
                            classifier,
                            config,
                            login,
                            resolver,
                            handle,
                            auth,
                            tui_factory: Some(make_tui_factory()),
                        },
                    );
                    run_repl(repl).await;
                    return;
                }
            };
            let (classifier, handle, resolver, auth) = build_capability_wiring();
            let repl = build_repl(
                renderer,
                ReplWiring {
                    classifier,
                    config,
                    login,
                    resolver,
                    handle,
                    auth,
                    tui_factory: None,
                },
            );
            run_repl(repl).await;
        }
    }
}

/// Drive a CLI subcommand (`orkia migrate-rc ...`) without touching
/// the REPL machinery. Returns the process exit code.
fn run_subcommand(sub: Subcommand) -> i32 {
    match sub {
        Subcommand::MigrateRc(a) => run_migrate_rc_cli(&a),
        Subcommand::Setup(a) => run_setup_cli(&a),
        Subcommand::Journal(a) => journal_cli::run(&a),
        Subcommand::Every(a) => run_every_cli(&a),
        Subcommand::Ps(a) => daemon_cli::run_ps(&a),
        Subcommand::Attach(a) => daemon_cli::run_attach(&a),
        Subcommand::Tell(a) => daemon_cli::run_tell(&a),
        Subcommand::Kill(a) => daemon_cli::run_kill(&a),
        Subcommand::Stop(a) => daemon_cli::run_stop_job(&a),
        Subcommand::Wait(a) => daemon_cli::run_wait(&a),
        Subcommand::Inspect(a) => daemon_cli::run_inspect(&a),
        Subcommand::Logs(a) => daemon_cli::run_logs(&a),
        Subcommand::Daemon(a) => daemon_cli::run_daemon(&a),
        // The async-runtime subcommands run from `main` directly so they
        // share the existing tokio runtime — unreachable here, kept
        // exhaustive so adding a new variant fails to compile until
        // wired in both places.
        Subcommand::Bridge(_)
        | Subcommand::McpBridge
        | Subcommand::McpPipe
        | Subcommand::PtyDaemon
        | Subcommand::PtyDaemonStop
        | Subcommand::Login(_)
        | Subcommand::Logout(_)
        | Subcommand::Whoami(_)
        | Subcommand::Reasoning(_)
        | Subcommand::Update(_) => {
            // These run from `main` on the shared tokio runtime; reaching here
            // means a routing bug. Exit non-zero instead of panicking (BUG-080).
            eprintln!("internal error: async subcommand routed to sync dispatcher");
            2
        }
    }
}

async fn run_bridge_cli(args: &[String]) -> i32 {
    let parsed = match bridge::BridgeArgs::parse(args) {
        Ok(a) => a,
        Err(e) if e == "__help__" => {
            bridge::print_help();
            return 0;
        }
        Err(e) => {
            eprintln!("orkia bridge: {e}");
            bridge::print_help();
            // Even on a parse error, exit 0 so a misconfigured hook
            // does not block the spawning agent.
            return 0;
        }
    };
    bridge::run(&parsed).await
}

/// `orkia reasoning <subcommand>` — currently only `backfill`. Routes to the
/// shell-side orchestrator so the consumer/encoding contract is owned in one
/// place. Returns the process exit code.
async fn run_reasoning_cli(args: &[String]) -> i32 {
    match args.split_first() {
        Some((cmd, rest)) if cmd == "backfill" => orkia_shell::reasoning_backfill::run(rest).await,
        _ => {
            eprintln!("usage: orkia reasoning backfill <envelopes.jsonl> [--no-push] [--dry-run]");
            2
        }
    }
}

fn run_setup_cli(args: &[String]) -> i32 {
    let parsed = match setup::SetupArgs::parse(args.iter().cloned()) {
        Ok(a) => a,
        Err(e) if e == "__help__" => {
            setup::print_help();
            return 0;
        }
        Err(e) => {
            eprintln!("orkia setup: {e}");
            setup::print_help();
            return 2;
        }
    };
    match setup::run_setup(&parsed) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("orkia setup: {e}");
            1
        }
    }
}

/// `orkia every ...` from the bare CLI (no REPL).
fn run_every_cli(args: &[String]) -> i32 {
    use orkia_shell_types::BlockContent;
    let data_dir = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".orkia"))
        .unwrap_or_else(|| PathBuf::from(".orkia"));
    let blocks = orkia_builtin::every::every(args, &data_dir);
    let mut exit = 0;
    for b in blocks {
        match b {
            BlockContent::Error(msg) => {
                eprintln!("orkia every: {msg}");
                exit = 1;
            }
            BlockContent::SystemInfo(msg) | BlockContent::Text(msg) => {
                println!("{msg}");
            }
            other => println!("{other:?}"),
        }
    }
    exit
}

fn run_migrate_rc_cli(args: &[String]) -> i32 {
    let opts = match orkia_builtin::migrate_rc::MigrateRcOpts::parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("orkia migrate-rc: {e}");
            return 2;
        }
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        eprintln!("orkia migrate-rc: HOME not set");
        return 2;
    };
    let dest = home.join(".orkiarc");
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let report = match orkia_builtin::migrate_rc::run_migration(&opts, &home, &dest, &today) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("orkia migrate-rc: {e}");
            return 1;
        }
    };

    eprintln!(
        "\n  source: {} \x1b[90m({})\x1b[0m\n",
        report.source_path.display(),
        report.kind.name(),
    );
    if report.counts.migrated > 0 {
        eprintln!("  \x1b[32m✓\x1b[0m {} migrated", report.counts.migrated);
    }
    if report.counts.translated > 0 {
        eprintln!("  \x1b[32m✓\x1b[0m {} translated", report.counts.translated);
    }
    if report.counts.comments > 0 {
        eprintln!("    {} comments preserved", report.counts.comments);
    }
    if report.counts.skipped > 0 {
        eprintln!("  \x1b[33m⚠\x1b[0m {} skipped:", report.counts.skipped);
        for (orig, reason) in &report.skipped {
            eprintln!("      {} \x1b[90m→ {reason:?}\x1b[0m", orig.trim());
        }
    }
    eprintln!();

    if opts.dry_run {
        eprintln!("  \x1b[90mdry-run: nothing written\x1b[0m");
        print!("{}", report.orkiarc_body);
        return 0;
    }

    if let Some(err) = &report.write_error {
        eprintln!("  \x1b[31merror:\x1b[0m {err}");
        return 1;
    }
    if let Some(p) = &report.written_to {
        eprintln!("  \x1b[32m✓\x1b[0m written to {}", p.display());
    }
    0
}

fn print_usage() {
    eprintln!(
        "Usage: orkia [OPTIONS]
       orkia -c <COMMAND>
       orkia --detach -c <AGENT_COMMAND>
       orkia setup [--minimal] [--force] [--offline]
       orkia migrate-rc [--from PATH] [--dry-run] [--append]
       orkia every \"<frequency>\" [@agent] <command> [--timeout DUR]
       orkia every (list | remove N | pause N | resume N)

OPTIONS:
    -c <COMMAND>    Execute COMMAND and exit (non-interactive, for ssh/scripts).
                    POSIX-first: the payload runs in the shell engine like
                    `bash -c` (`-c \"ps\"` is the system ps). Only an explicit
                    `orkia <cmd>` namespace or an agent command (@agent, pipes
                    to agents) routes through the Orkia pipeline.
    --timeout SECS  Hard cap on the -c run (used by `orkia every` for
                    scheduled jobs; exits 124 on timeout).
    --audit         Audit plain shell -c commands. Agentic -c commands are
                    already routed through the REPL journal/SEAL lifecycle.
    --detach        Run an agentic -c command under the local PTY daemon and
                    return immediately with a daemon job id.
    --tui           Launch in the ratatui TUI (alternate screen).
    --no-tui        Launch in the minimal stdout renderer (CI/scripting).
    -l, --login     Behave as a login shell (source .profile chain).
    -V, --version   Print version and exit.
    -h, --help      Print this help and exit.

SUBCOMMANDS:
    setup           First-time setup wizard (also relaunchable to add
                    agents/projects). Flags: --minimal, --force, --offline.
    migrate-rc      Convert ~/.zshrc | ~/.bashrc | fish config to ~/.orkiarc.
                    Flags: --from PATH, --dry-run, --append,
                           --zsh | --bash | --fish (explicit source kind).
    bridge          Agent-hook shim to the journal socket. Reads a JSON
                    payload on stdin and forwards it to ~/.orkia/run/orkia.sock.
                    Flags: --source <claude|codex|gemini|generic>. Used by
                    agent hook configs; not for direct invocation.
    mcp-bridge      stdio MCP bridge to Orkia socket tools. Used by generated
                    agent mcp-config.json; not for direct invocation.
    mcp-pipe        stdio MCP server exposing submit_pipeline_output. Spawned
                    per pipeline stage as the output safety net; env-driven.
    ps              List detached daemon-owned PTY jobs. Use `ps --json` for
                    machine-readable daemon/stage metrics and recovery state.
                    Use `ps --gc` to remove terminal/stale daemon job caches.
    attach          Attach to a detached job. `attach 1` attaches to the
                    runtime PTY; `attach 1:@sage` attaches to that agent
                    stage through the runtime control socket. Stage ids from
                    `ps --json` also work: `attach 1:2`. A bare agent name
                    (`attach sage` / `attach @sage`) attaches to that
                    agent's most recent live job.
    tell            Send a message to a detached stage:
                    `tell 1:@sage \"focus on security issues\"` or `tell 1:2`.
    kill            Terminate a detached job or stage: `kill 1` or
                    `kill 1:@sage` / `kill 1:2`.
    stop            Gracefully stop a detached job and keep its cache/logs
                    inspectable: `stop 1`.
    wait            Wait for a detached job to reach a terminal state:
                    `wait 1 --timeout 30`.
    inspect         Show detailed daemon diagnostics for one job:
                    `inspect 1`.
    logs            Print daemon SEAL/job log lines: `logs 1 --last 50`.
    daemon          Daemon lifecycle diagnostics: `daemon status`.
    journal         Query the unified event journal at ~/.orkia/journal.jsonl.
                    Flags: --agent NAME, --job ID, --type TYPE, --source SRC,
                           --last N, --since (RFC3339 | NN[smhd]).
    reasoning       Reasoning-graph maintenance. `reasoning backfill
                    <envelopes.jsonl>` stages historical agent sessions into
                    the graph and flushes them to the cloud (premium).
                    Flags: --no-push, --dry-run.
    update          Self-update the shell binaries from the signed `latest`
                    release. Flags: --check (report only), --kernel (update
                    the premium kernel daemon instead; requires login).

With no flags and an interactive terminal, orkia launches in shell mode:
stdin/stdout, no alternate screen, native terminal scrollback preserved.
This is the mode that makes `chsh -s /usr/bin/orkia` behave like a shell.

ENV:
    RUST_LOG=<filter>   tracing filter (e.g. `debug`, `orkia_shell=trace`).
    ORKIA_LOG=<path>    route tracing output to <path> instead of stderr.
                        Recommended for interactive sessions:
                          ORKIA_LOG=/tmp/orkia.log RUST_LOG=debug orkia
                        then in another tab: tail -f /tmp/orkia.log

CONFIG:
    [daemon]
    ipc_timeout_ms = 250
    startup_timeout_ms = 1000"
    );
}
