// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;

use orkia_shell::ShellConfig;

use super::attach as attach_mod;
use super::protocol::{
    CageWrapperProto, DaemonJobEvent, DaemonJobInfo, DaemonStatus, PROTOCOL_VERSION, Request,
    Response, send_request, socket_path,
};

/// All optional fields for a daemon-spawn request. Build with
/// `SpawnDetachedRequest::new(command)` then set fields as needed.
///
/// The `Default` impl sets every optional field to its absent value so
/// that old call sites using only `command` continue to behave exactly
/// as they do today.
pub(crate) struct SpawnDetachedRequest {
    pub command: String,
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    pub agent_name: Option<String>,
    pub extra_env: Vec<(String, String)>,
    pub cage_wrapper: Option<CageWrapperProto>,
    pub stdin_mode: Option<String>,
    pub initial_stdin_bytes: Vec<u8>,
    pub process_group: Option<String>,
    pub terminal_cols: Option<usize>,
    pub terminal_rows: Option<usize>,
    pub osc133: bool,
}

impl SpawnDetachedRequest {
    pub(crate) fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            working_dir: None,
            agent_name: None,
            extra_env: Vec::new(),
            cage_wrapper: None,
            stdin_mode: None,
            initial_stdin_bytes: Vec::new(),
            process_group: None,
            terminal_cols: None,
            terminal_rows: None,
            osc133: false,
        }
    }
}

/// Full-featured spawn: send the request with all optional fields.
pub(crate) fn spawn_detached_request(req: SpawnDetachedRequest, config: &ShellConfig) -> i32 {
    if !crate::dash_c::needs_repl_pipeline(&req.command) {
        eprintln!("orkia: --detach requires an agentic command or orkia builtin");
        return 2;
    }
    let wire_req = Request::Spawn {
        command: req.command,
        args: req.args,
        working_dir: req.working_dir,
        agent_name: req.agent_name,
        extra_env: req.extra_env,
        cage_wrapper: req.cage_wrapper,
        stdin_mode: req.stdin_mode,
        initial_stdin_bytes: req.initial_stdin_bytes,
        process_group: req.process_group,
        terminal_cols: req.terminal_cols,
        terminal_rows: req.terminal_rows,
        osc133: req.osc133,
    };
    match request_with_retry(config, &wire_req) {
        Ok(resp) if resp.ok => print_spawn(resp),
        Ok(resp) => {
            eprintln!(
                "orkia: {}",
                resp.error
                    .unwrap_or_else(|| "daemon spawn failed".to_string())
            );
            1
        }
        Err(err) => {
            eprintln!("orkia: {err}");
            1
        }
    }
}

/// Like [`spawn_detached_request`] but returns the daemon job id instead of an
/// exit code, and prints NOTHING. The REPL's `DetachedSpawner` impl uses this
/// its own side effects, so it needs the id back, not a printed line + exit code.
pub(crate) fn spawn_detached_request_id(
    req: SpawnDetachedRequest,
    config: &ShellConfig,
) -> Result<u32, String> {
    if !crate::dash_c::needs_repl_pipeline(&req.command) {
        return Err("--detach requires an agentic command or orkia builtin".to_string());
    }
    let wire_req = Request::Spawn {
        command: req.command,
        args: req.args,
        working_dir: req.working_dir,
        agent_name: req.agent_name,
        extra_env: req.extra_env,
        cage_wrapper: req.cage_wrapper,
        stdin_mode: req.stdin_mode,
        initial_stdin_bytes: req.initial_stdin_bytes,
        process_group: req.process_group,
        terminal_cols: req.terminal_cols,
        terminal_rows: req.terminal_rows,
        osc133: req.osc133,
    };
    let resp = request_with_retry(config, &wire_req)?;
    if !resp.ok {
        return Err(resp
            .error
            .unwrap_or_else(|| "daemon spawn failed".to_string()));
    }
    resp.job
        .map(|job| job.id)
        .ok_or_else(|| "daemon returned no job".to_string())
}

pub(crate) fn list(config: &ShellConfig) -> Vec<DaemonJobInfo> {
    match request_with_retry(config, &Request::List) {
        Ok(resp) if resp.ok => resp.jobs,
        _ => Vec::new(),
    }
}

pub(crate) fn gc(config: &ShellConfig) -> Result<Vec<DaemonJobInfo>, String> {
    let resp = request_with_retry(config, &Request::Gc)?;
    if resp.ok {
        Ok(resp.jobs)
    } else {
        Err(resp.error.unwrap_or_else(|| "daemon gc failed".to_string()))
    }
}

pub(crate) fn inspect(config: &ShellConfig, id: u32) -> Result<DaemonJobInfo, String> {
    let resp = request_with_retry(config, &Request::Inspect { id })?;
    if resp.ok {
        resp.job
            .ok_or_else(|| "daemon inspect returned no job".to_string())
    } else {
        Err(resp
            .error
            .unwrap_or_else(|| "daemon inspect failed".to_string()))
    }
}

pub(crate) fn logs(config: &ShellConfig, id: u32, limit: usize) -> Result<Vec<String>, String> {
    let resp = request_with_retry(
        config,
        &Request::Logs {
            id,
            limit: Some(limit),
        },
    )?;
    if resp.ok {
        Ok(resp.logs)
    } else {
        Err(resp
            .error
            .unwrap_or_else(|| "daemon logs failed".to_string()))
    }
}

pub(crate) fn status(config: &ShellConfig) -> DaemonStatus {
    match request(config, &Request::Status) {
        Ok(resp) if resp.ok => resp.status.unwrap_or_else(|| stopped_status(config)),
        Ok(resp) => DaemonStatus {
            state: resp
                .error
                .unwrap_or_else(|| "daemon status unavailable".to_string()),
            ..stopped_status(config)
        },
        Err(_) => stopped_status(config),
    }
}

pub(crate) fn tell(
    config: &ShellConfig,
    id: u32,
    target: &str,
    message: &str,
) -> Result<(), String> {
    ok_response(
        request(
            config,
            &Request::Tell {
                id,
                target: target.to_string(),
                message: message.to_string(),
            },
        )?,
        "daemon tell failed",
    )
}

pub(crate) fn kill(config: &ShellConfig, id: u32) -> Result<(), String> {
    ok_response(
        request(config, &Request::Kill { id, target: None })?,
        "daemon kill failed",
    )
}

pub(crate) fn stop(config: &ShellConfig, id: u32) -> Result<(), String> {
    ok_response(
        request(config, &Request::Stop { id })?,
        "daemon stop failed",
    )
}

pub(crate) fn wait(
    config: &ShellConfig,
    id: u32,
    timeout_ms: u64,
) -> Result<DaemonJobInfo, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    // The roster auto-reaps "done" jobs after reporting them once; another
    // client's list can consume that report between our polls. A job we saw
    // alive that has vanished IS finished — synthesize its terminal state
    // instead of spinning until timeout.
    let mut last_seen: Option<DaemonJobInfo> = None;
    loop {
        match list(config).into_iter().find(|job| job.id == id) {
            Some(job) if is_terminal_state(&job.state) => return Ok(job),
            Some(job) => last_seen = Some(job),
            None => {
                let Some(mut job) = last_seen else {
                    return Err(format!("no such job: {id}"));
                };
                job.state = "done".to_string();
                return Ok(job);
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("wait timeout for job {id}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub(crate) fn kill_target(config: &ShellConfig, id: u32, target: &str) -> Result<(), String> {
    ok_response(
        request(
            config,
            &Request::Kill {
                id,
                target: Some(target.to_string()),
            },
        )?,
        "daemon kill failed",
    )
}

/// Forward one detached-runtime `JobEvent` up to the daemon (MIGRATE-AGENT-
/// to the REPL subscriber and closes WITHOUT a response, so we send and drop.
/// Best-effort by design — no daemon, an older daemon, or a transient connect
/// failure is non-fatal (#1: never block the runtime's drain on this; the main
/// REPL re-derives job state from `List` regardless).
pub(crate) fn forward_job_event(config: &ShellConfig, event: DaemonJobEvent) {
    let path = socket_path(&config.data_dir);
    if let Ok(mut stream) = UnixStream::connect(&path) {
        let _ = install_timeout(config, &stream);
        let _ = send_request(&mut stream, &Request::JobEventEmit { event });
    }
}

/// Forward a detached runtime's journal envelope up to the daemon (MIGRATE-AGENT-
/// main REPL's projection/sink sees a daemon-owned agent's turn. Reuses the same
/// `JournalEmit` request the subscribed REPL uses to push its in-process emits;
/// the daemon hub relays it to the main REPL subscriber as a `StreamFrame::Envelope`.
/// Fire-and-forget, best-effort (#1) — same rationale as [`forward_job_event`].
pub(crate) fn forward_journal_envelope(
    config: &ShellConfig,
    envelope: orkia_shell::journal::JournalEnvelope,
) {
    let path = socket_path(&config.data_dir);
    if let Ok(mut stream) = UnixStream::connect(&path) {
        let _ = install_timeout(config, &stream);
        let _ = send_request(&mut stream, &Request::JournalEmit { envelope });
    }
}

pub(crate) fn shutdown(config: &ShellConfig) -> Result<(), String> {
    ok_response(
        request(config, &Request::Shutdown)?,
        "daemon shutdown failed",
    )
}

pub(crate) fn attach(config: &ShellConfig, id: u32, target: Option<String>) -> Result<(), String> {
    attach_mod::client(config, id, target)
}

fn print_spawn(resp: Response) -> i32 {
    if let Some(job) = resp.job {
        println!(
            "{} detached: {} pid={}",
            orkia_shell_types::render_job_id(orkia_shell_types::JobOwner::Daemon, job.id, None),
            job.agent,
            job.pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string())
        );
        0
    } else {
        eprintln!("orkia: daemon returned no job");
        1
    }
}

fn ok_response(resp: Response, default_error: &str) -> Result<(), String> {
    if resp.ok {
        Ok(())
    } else {
        Err(resp.error.unwrap_or_else(|| default_error.to_string()))
    }
}

fn request_with_retry(config: &ShellConfig, req: &Request) -> Result<Response, String> {
    match request(config, req) {
        Ok(resp) => Ok(resp),
        Err(_) => {
            start_daemon_process()?;
            let wait = config.daemon.startup_timeout_ms.max(50);
            let attempts = wait.div_ceil(50);
            for _ in 0..attempts {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if let Ok(resp) = request(config, req) {
                    return Ok(resp);
                }
            }
            Err("daemon did not become ready".to_string())
        }
    }
}

/// a long-lived `Subscribe` stream. Ensures the daemon is running (starts it
/// if not — the daemon is the sole `orkia.sock` owner, so a REPL that booted
/// its own local hub would otherwise have its socket silently stolen the
/// moment any detached spawn starts the daemon). Performs the `ok`
/// handshake, then returns the full-duplex reader with the streaming read
/// timeout cleared. `None` ⇒ subscription unavailable; the caller falls back
/// to the REPL-owned local hub (degraded, daemon-less — no steal hazard).
pub(crate) fn subscribe_journal(config: &ShellConfig) -> Option<BufReader<UnixStream>> {
    if let Some(reader) = try_subscribe_once(config) {
        return Some(reader);
    }
    // No (or stale) daemon — start one and retry until it is ready.
    if start_daemon_process().is_err() {
        return None;
    }
    let wait = config.daemon.startup_timeout_ms.max(50);
    let attempts = wait.div_ceil(50);
    for _ in 0..attempts {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if let Some(reader) = try_subscribe_once(config) {
            return Some(reader);
        }
    }
    None
}

fn try_subscribe_once(config: &ShellConfig) -> Option<BufReader<UnixStream>> {
    let path = socket_path(&config.data_dir);
    let mut stream = UnixStream::connect(&path).ok()?;
    // Short timeout for the handshake only; cleared below for streaming.
    let _ = install_timeout(config, &stream);
    if send_request(&mut stream, &Request::Subscribe).is_err() {
        return None;
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return None;
    }
    let resp: Response = serde_json::from_str(&line).ok()?;
    // A stale daemon (predating the subscribe frames) rejects `Subscribe` as an
    // unknown method (ok=false); a version skew is likewise a hard no.
    if !resp.ok || resp.version != PROTOCOL_VERSION {
        return None;
    }
    // Streaming connection: clear the per-call timeouts so the reader can
    // block indefinitely on the next frame and the writer never times out.
    let _ = reader.get_ref().set_read_timeout(None);
    let _ = reader.get_ref().set_write_timeout(None);
    Some(reader)
}

fn request(config: &ShellConfig, req: &Request) -> Result<Response, String> {
    let path = socket_path(&config.data_dir);
    let mut stream =
        UnixStream::connect(&path).map_err(|e| format!("connect {}: {e}", path.display()))?;
    install_timeout(config, &stream)?;
    send_request(&mut stream, req)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read response: {e}"))?;
    let resp: Response = serde_json::from_str(&line).map_err(|e| format!("parse response: {e}"))?;
    if resp.version != PROTOCOL_VERSION {
        return Err(format!(
            "daemon protocol mismatch: daemon={} client={}",
            resp.version, PROTOCOL_VERSION
        ));
    }
    Ok(resp)
}

fn install_timeout(config: &ShellConfig, stream: &UnixStream) -> Result<(), String> {
    let timeout = Some(std::time::Duration::from_millis(
        config.daemon.ipc_timeout_ms.max(50),
    ));
    stream
        .set_read_timeout(timeout)
        .map_err(|e| format!("set daemon read timeout: {e}"))?;
    stream
        .set_write_timeout(timeout)
        .map_err(|e| format!("set daemon write timeout: {e}"))
}

fn start_daemon_process() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    std::process::Command::new(exe)
        .arg("pty-daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("spawn daemon: {e}"))
}

fn stopped_status(config: &ShellConfig) -> DaemonStatus {
    DaemonStatus {
        state: "stopped".to_string(),
        protocol_version: PROTOCOL_VERSION,
        pid: None,
        socket: socket_path(&config.data_dir).display().to_string(),
        jobs: 0,
    }
}

pub(crate) fn is_terminal_state(state: &str) -> bool {
    matches!(
        state,
        "done" | "failed" | "stopped" | "pid_dead" | "lost_pty" | "control_unavailable"
    )
}
