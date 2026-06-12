// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use orkia_shell::ShellConfig;
use orkia_shell_types::{BlockContent, PsFlags};

use crate::pty_daemon;

pub(crate) fn run_ps(args: &[String]) -> i32 {
    let json_output = args.iter().any(|a| a == "--json");
    let config = ShellConfig::load();
    let jobs = if args.iter().any(|a| a == "--gc") {
        match pty_daemon::gc(&config) {
            Ok(jobs) => jobs,
            Err(err) => {
                eprintln!("ps --gc: {err}");
                return 1;
            }
        }
    } else {
        pty_daemon::list(&config)
    };
    // build the render model, let the one core renderer format. No agent
    // roster from a plain CLI process (`agents: None` omits the JSON key);
    // no system-process section — that is the REPL `ps` enrichment.
    let model = orkia_builtin::ps::PsModel {
        agents: None,
        jobs: jobs.iter().map(daemon_job_row).collect(),
    };
    let flags = PsFlags {
        show_agents: true,
        show_system: false,
        full: false,
        json: json_output,
    };
    for block in orkia_builtin::ps::render(&model, &flags) {
        match block {
            BlockContent::Text(line) | BlockContent::SystemInfo(line) => println!("{line}"),
            _ => {}
        }
    }
    0
}

/// Protocol row → render row. Lives here because `DaemonJobInfo` is
/// binary-local; the core model is the shared shape.
fn daemon_job_row(job: &pty_daemon::DaemonJobInfo) -> orkia_builtin::ps::PsRow {
    orkia_builtin::ps::PsRow {
        id: job.id,
        owner: orkia_shell_types::JobOwner::Daemon,
        agent: job.agent.clone(),
        state: job.state.clone(),
        state_typed: orkia_builtin::ps::job_state(&job.state, job.exit_code),
        pid: job.pid,
        label: job.label.clone(),
        runtime_secs: job.runtime_secs,
        // The daemon protocol does not carry sink bindings — unknown here,
        // so the row (not a hardcoded JSON pin) decides what renders
        sink: None,
        exit_code: job.exit_code,
        attachable: job.attachable,
        is_app: false,
        control_socket: job.control_socket.clone(),
        pty_owner_pid: job.pty_owner_pid,
        lost_reason: job.lost_reason.clone(),
        seal_path: job.seal_path.clone(),
        stages: job
            .stages
            .iter()
            .map(|s| orkia_builtin::ps::PsStageRow {
                id: s.id,
                target: s.target.clone(),
                state: s.state.clone(),
                pid: s.pid,
                runtime_secs: s.runtime_secs,
                exit_code: s.exit_code,
                attachable: s.attachable,
                lost_reason: s.lost_reason.clone(),
            })
            .collect(),
    }
}

pub(crate) fn run_attach(args: &[String]) -> i32 {
    let Some(raw) = args.first() else {
        eprintln!("usage: orkia attach <daemon_job_id>[:@agent|:stage_id] | <agent_name>");
        return 2;
    };
    let config = ShellConfig::load();
    let (id, target) = if let Some((id, target)) = parse_target(raw) {
        (id, Some(target))
    } else if let Ok(id) = raw.parse::<u32>() {
        (id, None)
    } else {
        match resolve_attach_agent(&config, raw) {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("attach: {err}");
                return 2;
            }
        }
    };
    match pty_daemon::attach(&config, id, target) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("attach: {err}");
            1
        }
    }
}

/// Bare agent name (`attach faye`, `tell sage …`, `kill sage`) from a
/// normal shell: resolve the name against the daemon roster — most-recent
/// LIVE job first, mirroring the in-REPL resolver. For `attach`/`tell` the
/// returned name targets that agent's stage (the nested agent PTY, not the
/// wrapper); pipeline jobs resolve too: naming one of the stages targets
/// that stage.
fn resolve_attach_agent(config: &ShellConfig, raw: &str) -> Result<(u32, Option<String>), String> {
    let name = raw.strip_prefix('@').unwrap_or(raw);
    let jobs = pty_daemon::list(config);
    let named: Vec<_> = jobs
        .iter()
        .filter(|j| {
            j.agent
                .split('|')
                .any(|a| a.trim_start_matches('@') == name)
        })
        .collect();
    let job = named
        .iter()
        .rev()
        .find(|j| daemon_job_is_live(&j.state))
        .copied()
        .or_else(|| named.last().copied())
        .ok_or_else(|| format!("no daemon job for agent `{name}`"))?;
    if !daemon_job_is_live(&job.state) {
        return Err(format!(
            "job {} is {}; the agent already exited",
            job.id, job.state
        ));
    }
    Ok((job.id, Some(name.to_string())))
}

/// Same liveness predicate as the REPL bridge's `daemon_view_is_live`.
fn daemon_job_is_live(state: &str) -> bool {
    !state.starts_with("done")
        && !state.starts_with("fail")
        && state != "pid_dead"
        && state != "control_unavailable"
}

pub(crate) fn run_tell(args: &[String]) -> i32 {
    let Some(raw) = args.first() else {
        eprintln!("usage: orkia tell <daemon_job_id>:(@agent|stage_id) | <agent_name> <message>");
        return 2;
    };
    let config = ShellConfig::load();
    let (id, target) = if let Some(pair) = parse_tell_target(raw) {
        pair
    } else if raw.parse::<u32>().is_ok() {
        // A bare job id is ambiguous for tell: the message must reach an
        // agent or stage, never the runtime wrapper (`kill 1` / `attach 1`
        // have a whole-job meaning; tell does not). Refuse with the form
        // instead of mis-resolving the digits as an agent name.
        eprintln!(
            "tell: `{raw}` is a daemon job id — name the agent or stage, e.g. `{raw}:@sage` or `{raw}:2`"
        );
        return 2;
    } else {
        // Bare agent name (`tell sage …` / `tell @sage …`): resolve against
        // the daemon roster like `attach`, then target that agent's stage.
        match resolve_attach_agent(&config, raw) {
            Ok((id, name)) => (
                id,
                name.unwrap_or_else(|| raw.trim_start_matches('@').to_string()),
            ),
            Err(err) => {
                eprintln!("tell: {err}");
                return 2;
            }
        }
    };
    let message = args.get(1..).unwrap_or_default().join(" ");
    if message.trim().is_empty() {
        eprintln!("tell: missing message");
        return 2;
    }
    match pty_daemon::tell(&config, id, &target, &message) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("tell: {err}");
            1
        }
    }
}

fn parse_tell_target(raw: &str) -> Option<(u32, String)> {
    parse_target(raw)
}

fn parse_target(raw: &str) -> Option<(u32, String)> {
    let (id_raw, target_raw) = raw.split_once(':')?;
    let id = id_raw.parse::<u32>().ok()?;
    if target_raw.chars().all(|ch| ch.is_ascii_digit()) {
        return Some((id, target_raw.to_string()));
    }
    let target = target_raw.strip_prefix('@')?;
    if target.is_empty()
        || !target
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    Some((id, target.to_string()))
}

pub(crate) fn run_kill(args: &[String]) -> i32 {
    let Some(raw) = args.first() else {
        eprintln!("usage: orkia kill <daemon_job_id>[:@agent|:stage_id] | <agent_name>");
        return 2;
    };
    let config = ShellConfig::load();
    if let Some((id, target)) = parse_target(raw) {
        return match pty_daemon::kill_target(&config, id, &target) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("kill: {err}");
                1
            }
        };
    }
    let id = if let Ok(id) = raw.parse::<u32>() {
        id
    } else {
        // Bare agent name (`kill sage` / `kill @sage`): resolve against the
        // daemon roster like `attach`, then kill that agent's whole job.
        match resolve_attach_agent(&config, raw) {
            Ok((id, _)) => id,
            Err(err) => {
                eprintln!("kill: {err}");
                return 2;
            }
        }
    };
    match pty_daemon::kill(&config, id) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("kill: {err}");
            1
        }
    }
}

pub(crate) fn run_stop_job(args: &[String]) -> i32 {
    let Some(id) = parse_job_id(args, "stop") else {
        return 2;
    };
    match pty_daemon::stop(&ShellConfig::load(), id) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("stop: {err}");
            1
        }
    }
}

pub(crate) fn run_wait(args: &[String]) -> i32 {
    let Some(id) = parse_job_id(args, "wait") else {
        return 2;
    };
    let timeout_ms = parse_timeout_ms(args).unwrap_or(30_000);
    let config = ShellConfig::load();
    // Mirror of the REPL builtin's `daemon_wait_refusal_for` (orkia-shell
    // repl/job_control.rs): a live persistent session (no standalone `--once`
    // token in its verbatim command line) never terminates on its own, so a
    // blocking wait would only ever time out. Refuse with the same actionable
    // hint instead. `list` (not `inspect`) on purpose: only the list path
    // probes cached corpses to `pid_dead`, and it is the same source the wait
    // loop polls. An absent job (daemon down, unknown id) falls through to the
    // normal wait, whose existing error path reports it.
    if let Some(job) = pty_daemon::list(&config).into_iter().find(|j| j.id == id)
        && !pty_daemon::is_terminal_state(&job.state)
        && !job.label.split_whitespace().any(|w| w == "--once")
    {
        eprintln!(
            "wait: '@{agent}' is a persistent agent session; it ends only when killed. \
             Use `tell {agent} <msg>` to message it, `attach @{agent}` to attach, \
             `kill {id}` to end it — or dispatch with `--once` for a one-shot `wait` can block on",
            agent = job.agent,
        );
        return 1;
    }
    match pty_daemon::wait(&config, id, timeout_ms) {
        Ok(job) => {
            println!("{} {}", job.id, job.state);
            job.exit_code.unwrap_or(0)
        }
        Err(err) => {
            eprintln!("wait: {err}");
            1
        }
    }
}

pub(crate) fn run_inspect(args: &[String]) -> i32 {
    let Some(id) = parse_job_id(args, "inspect") else {
        return 2;
    };
    match pty_daemon::inspect(&ShellConfig::load(), id) {
        Ok(job) => {
            print_inspect(&job);
            0
        }
        Err(err) => {
            eprintln!("inspect: {err}");
            1
        }
    }
}

pub(crate) fn run_logs(args: &[String]) -> i32 {
    let Some(id) = parse_job_id(args, "logs") else {
        return 2;
    };
    let limit = parse_last(args).unwrap_or(100);
    match pty_daemon::logs(&ShellConfig::load(), id, limit) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            0
        }
        Err(err) => {
            eprintln!("logs: {err}");
            1
        }
    }
}

pub(crate) fn run_daemon(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("status") | None => {
            print_daemon_status(&pty_daemon::status(&ShellConfig::load()));
            0
        }
        Some(other) => {
            eprintln!("usage: orkia daemon status");
            eprintln!("daemon: unknown command `{other}`");
            2
        }
    }
}

pub(crate) fn run_stop() -> i32 {
    match pty_daemon::shutdown(&ShellConfig::load()) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("pty-daemon-stop: {err}");
            1
        }
    }
}

fn parse_job_id(args: &[String], command: &str) -> Option<u32> {
    let Some(raw) = args.first() else {
        eprintln!("usage: orkia {command} <daemon_job_id>");
        return None;
    };
    match raw.parse::<u32>() {
        Ok(id) => Some(id),
        Err(_) => {
            eprintln!("{command}: expected daemon job id, got `{raw}`");
            None
        }
    }
}

fn parse_timeout_ms(args: &[String]) -> Option<u64> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--timeout" {
            return iter
                .next()
                .and_then(|raw| raw.parse::<u64>().ok())
                .map(|secs| secs.saturating_mul(1000));
        }
    }
    None
}

fn parse_last(args: &[String]) -> Option<usize> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--last" {
            return iter.next().and_then(|raw| raw.parse::<usize>().ok());
        }
    }
    None
}

fn print_inspect(job: &pty_daemon::DaemonJobInfo) {
    println!("JOB {}", job.id);
    println!("  agent: {}", job.agent);
    println!("  status: {}", job.state);
    println!("  pid: {}", fmt_opt(job.pid));
    println!("  runtime_secs: {}", job.runtime_secs);
    println!("  attachable: {}", job.attachable);
    println!("  cmd: {}", job.label);
    println!("  control_socket: {}", fmt_opt_ref(&job.control_socket));
    println!("  pty_owner_pid: {}", fmt_opt(job.pty_owner_pid));
    println!("  seal_path: {}", fmt_opt_ref(&job.seal_path));
    println!("  lost_reason: {}", fmt_opt_ref(&job.lost_reason));
    println!("  exit_code: {}", fmt_opt_i32(job.exit_code));
    if job.stages.is_empty() {
        println!("  stages: none");
        return;
    }
    println!("  stages:");
    for stage in &job.stages {
        println!(
            "    {} {} status={} pid={} attachable={} runtime_secs={} exit_code={} lost_reason={}",
            stage.id,
            stage.target,
            stage.state,
            fmt_opt(stage.pid),
            stage.attachable,
            stage.runtime_secs,
            fmt_opt_i32(stage.exit_code),
            fmt_opt_ref(&stage.lost_reason)
        );
    }
}

fn print_daemon_status(status: &pty_daemon::DaemonStatus) {
    println!("state: {}", status.state);
    println!("protocol_version: {}", status.protocol_version);
    println!("pid: {}", fmt_opt(status.pid));
    println!("socket: {}", status.socket);
    println!("jobs: {}", status.jobs);
}

fn fmt_opt(value: Option<u32>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn fmt_opt_i32(value: Option<i32>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn fmt_opt_ref(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("-")
}
