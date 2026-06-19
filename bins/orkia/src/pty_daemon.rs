// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
use orkia_shell::ShellConfig;
use orkia_terminal_core::TerminalEngine;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;
mod attach;
mod audit;
mod client_api;
pub(crate) mod daemon_jobs;
pub(crate) mod detached_spawner;
mod ipc_security;
pub(crate) mod job_forward;
mod journal_host;
mod journal_pump;
mod lock;
mod protocol;
mod recovery;
mod runtime_control;
mod runtime_spawn;
mod task_host;
pub(crate) use client_api::{
    SpawnDetachedRequest, attach, gc, inspect, is_terminal_state, kill, kill_target, list, logs,
    shutdown, spawn_detached_request, status, stop, tell, wait,
};
use journal_host::JournalHost;
use orkia_shell::journal::McpReply;
pub(crate) use protocol::{DaemonJobInfo, DaemonStatus};
use protocol::{
    DaemonStageInfo, PROTOCOL_VERSION, Request, Response, WireRequest, agent_labels, socket_path,
    write_error, write_ok, write_response,
};
/// journal hub. On success returns a pump starter the REPL invokes from
/// `boot_journal` (handing it the relay/emit/mcp handles); `None` ⇒ the REPL
/// boots its own local hub (daemon-less fallback). Starting the daemon if
/// absent is intentional — it must be the sole `orkia.sock` owner.
pub(crate) fn journal_pump_starter(
    config: &ShellConfig,
) -> Option<orkia_shell::repl::JournalPumpStarter> {
    // must NOT subscribe to the daemon-hosted hub — that single subscriber slot
    // belongs to the main REPL. It hosts its own per-job local hub instead
    // (`boot_journal` takes the local branch when no starter is installed) and
    // forwards events up to the daemon. Detect via `ORKIA_DETACHED_JOB_ID`.
    if orkia_shell::detached_control::detached_runtime_hub_socket(&config.data_dir).is_some() {
        return None;
    }
    let reader = client_api::subscribe_journal(config)?;
    Some(Box::new(move |handles| {
        journal_pump::spawn(reader, handles)
    }))
}

struct DaemonJob {
    id: u32,
    agent: String,
    label: String,
    started_at: Instant,
    engine: TerminalEngine,
    control_sock: PathBuf,
}

#[derive(Default)]
struct DaemonState {
    next_id: u32,
    jobs: HashMap<u32, DaemonJob>,
    data_dir: PathBuf,
}

struct DaemonCommand {
    stream: UnixStream,
    request: Request,
}

impl DaemonState {
    fn alloc_id(&mut self) -> Result<u32, String> {
        if self.next_id == 0 {
            self.next_id = 1;
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| "job id exhausted".to_string())?;
        Ok(id)
    }
}

pub(crate) fn run_server(config: ShellConfig) -> i32 {
    // Build the tokio runtime early so its lifetime spans the whole daemon.
    //
    // Runtime choice: multi_thread with 2 worker threads.
    //   - multi_thread gets its own OS threads, so its reactor never competes
    //     with the synchronous accept loop below.
    //   - current_thread would require explicit block_on driving; the sync
    //     accept loop never calls block_on, so tasks would never poll.
    //     MCP proxy call parks a worker in `block_in_place` awaiting the REPL
    //     reply (RFC asks are interactive). 4 gives headroom for concurrent
    //     asks plus the hub fanout + accept reactor.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("orkia-daemon-async")
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("orkia pty-daemon: build tokio runtime: {err}");
            return 1;
        }
    };
    let mut _task_host = task_host::TaskHost::new(runtime.handle().clone());

    let _lock = match lock::DaemonLock::acquire(&config.data_dir) {
        Ok(lock) => lock,
        Err(err) => {
            eprintln!("orkia pty-daemon: {err}");
            return 1;
        }
    };

    // hooks, FRS capture, and the disk mirror survive a REPL restart. A bind
    // failure degrades journaling but must not kill PTY control, so we log and
    // run host-less (journal control requests then fail-closed in the actor).
    let journal_host = match JournalHost::start(&config, runtime.handle().clone()) {
        Ok(host) => Some(Arc::new(host)),
        Err(err) => {
            eprintln!("orkia pty-daemon: journal host disabled: {err}");
            None
        }
    };
    let path = socket_path(&config.data_dir);
    let Some(parent) = path.parent() else {
        eprintln!("orkia pty-daemon: invalid socket path {}", path.display());
        return 1;
    };
    if let Err(err) = std::fs::create_dir_all(parent) {
        eprintln!("orkia pty-daemon: create {}: {err}", parent.display());
        return 1;
    }
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("orkia pty-daemon: bind {}: {err}", path.display());
            return 1;
        }
    };
    if let Err(err) = ipc_security::set_private_socket_permissions(&path) {
        eprintln!("orkia pty-daemon: {err}");
        return 1;
    }
    if let Err(err) = listener.set_nonblocking(true) {
        eprintln!("orkia pty-daemon: nonblocking listener: {err}");
        return 1;
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    let (cmd_tx, cmd_rx) = mpsc::channel::<DaemonCommand>();
    let actor_config = config.clone();
    let actor_shutdown = Arc::clone(&shutdown);
    let actor = match std::thread::Builder::new()
        .name("orkia-pty-daemon-actor".to_string())
        .spawn(move || run_actor(cmd_rx, actor_config, actor_shutdown))
    {
        Ok(actor) => actor,
        Err(err) => {
            eprintln!("orkia pty-daemon: spawn actor: {err}");
            return 1;
        }
    };
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                // The listener is non-blocking so the accept loop can poll for
                // shutdown. On macOS/BSD the accepted socket INHERITS O_NONBLOCK,
                // so a blocking `read_line` of the request would instead return
                // EAGAIN ("Resource temporarily unavailable") whenever the
                // client's bytes have not landed yet — a startup race. Each
                // client runs on its own thread and should block on its socket,
                // so put it back into blocking mode explicitly.
                if let Err(err) = stream.set_nonblocking(false) {
                    eprintln!("orkia pty-daemon: client set_blocking: {err}");
                    continue;
                }
                let cmd_tx = cmd_tx.clone();
                let host = journal_host.clone();
                let _ = std::thread::Builder::new()
                    .name("orkia-pty-daemon-client".to_string())
                    .spawn(move || handle_client(stream, cmd_tx, host));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(err) => eprintln!("orkia pty-daemon: accept: {err}"),
        }
    }
    // Abort async tasks before the runtime drops. This mirrors how the sync
    // actor is joined and the socket file is removed: tasks first, then sync
    // teardown, then runtime.
    _task_host.shutdown();
    drop(cmd_tx);
    let _ = actor.join();
    let _ = std::fs::remove_file(&path);
    // Shut the runtime down BEFORE dropping the JournalHost. The hub's bus
    // tasks hold disk-tee sender clones, and `JournalStore::drop` joins the
    // writer thread, which only exits once every sender is gone — dropping
    // the host first deadlocks the teardown (daemon survives `pty-daemon-stop`
    // with the lock and pid intact). The timeout bounds MCP dispatchers
    // parked in `block_in_place`.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    drop(journal_host);
    0
}

fn handle_client(
    mut stream: UnixStream,
    cmd_tx: mpsc::Sender<DaemonCommand>,
    host: Option<Arc<JournalHost>>,
) {
    if let Err(err) = ipc_security::verify_peer_owner(&stream) {
        let _ = write_error(&mut stream, format!("unauthorized client: {err}"));
        return;
    }
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            let _ = write_error(&mut stream, format!("clone stream: {err}"));
            return;
        }
    };
    let mut reader = BufReader::new(reader_stream);
    let mut line = String::new();
    if let Err(err) = reader.read_line(&mut line) {
        let _ = write_error(&mut stream, format!("read request: {err}"));
        return;
    }
    let wire: WireRequest = match serde_json::from_str(&line) {
        Ok(wire) => wire,
        Err(err) => {
            let _ = write_error(&mut stream, format!("parse request: {err}"));
            return;
        }
    };
    if wire.version != PROTOCOL_VERSION {
        let _ = write_error(
            &mut stream,
            format!(
                "daemon protocol mismatch: client={} daemon={}",
                wire.version, PROTOCOL_VERSION
            ),
        );
        return;
    }
    // `JournalHost` (no actor round-trip). `Subscribe` upgrades this
    // connection to the long-lived, full-duplex REPL stream; the in-process
    // control frames are processed and the connection closes. Host-less
    // (bind failed) ⇒ fall through to the actor's fail-closed arm.
    if let Some(host) = host.as_ref() {
        match wire.request {
            Request::Subscribe => {
                serve_subscriber(stream, reader, host);
                return;
            }
            Request::JournalEmit { envelope } => {
                host.emit(envelope);
                return;
            }
            Request::McpProxyReply {
                corr_id,
                response,
                accessed_node_ids,
            } => {
                host.resolve_mcp(
                    corr_id,
                    McpReply {
                        response,
                        accessed_node_ids,
                    },
                );
                return;
            }
            // `JobScope` is reserved under Option A (SEAL stays REPL-resident,
            // so the daemon needs no job→project map); ack by closing.
            Request::JobScope { .. } => return,
            // the REPL subscriber and close; fire-and-return like `JournalEmit`.
            Request::JobEventEmit { event } => {
                host.push_job_event(event);
                return;
            }
            _ => {}
        }
        let cmd = DaemonCommand {
            stream,
            request: wire.request,
        };
        let _ = cmd_tx.send(cmd);
        return;
    }
    let cmd = DaemonCommand {
        stream,
        request: wire.request,
    };
    if cmd_tx.send(cmd).is_err() {
        // Actor is gone; dropping the stream closes the client request.
    }
}

/// Drive the long-lived REPL subscriber connection. Sends the `ok` handshake
/// (so the REPL can tell a hub-hosting daemon from a stale one that rejects
/// `Subscribe`), hands the write half to the hub's stream writer, then loops
/// reading REPL→daemon control frames (`JournalEmit`, `McpProxyReply`) until
/// the REPL disconnects. Per-frame parse failures are skipped, never fatal
/// (every byte untrusted, #7).
fn serve_subscriber(
    mut stream: UnixStream,
    mut reader: BufReader<UnixStream>,
    host: &Arc<JournalHost>,
) {
    if write_ok(&mut stream).is_err() {
        return;
    }
    host.attach_subscriber(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        if line.trim().is_empty() {
            continue;
        }
        let wire: WireRequest = match serde_json::from_str(&line) {
            Ok(wire) => wire,
            Err(_) => continue,
        };
        match wire.request {
            Request::JournalEmit { envelope } => host.emit(envelope),
            Request::McpProxyReply {
                corr_id,
                response,
                accessed_node_ids,
            } => host.resolve_mcp(
                corr_id,
                McpReply {
                    response,
                    accessed_node_ids,
                },
            ),
            Request::JobScope { .. } => {}
            _ => {}
        }
    }
}

fn run_actor(rx: mpsc::Receiver<DaemonCommand>, config: ShellConfig, shutdown: Arc<AtomicBool>) {
    let mut state = DaemonState {
        next_id: audit::next_cached_job_id(&config.data_dir),
        data_dir: config.data_dir.clone(),
        ..DaemonState::default()
    };
    while let Ok(cmd) = rx.recv() {
        match cmd.request {
            Request::Status => handle_status(cmd.stream, &mut state, &config),
            Request::Spawn {
                command,
                args,
                working_dir,
                agent_name,
                extra_env,
                cage_wrapper,
                stdin_mode,
                initial_stdin_bytes,
                process_group,
                terminal_cols,
                terminal_rows,
                osc133,
            } => handle_spawn(
                cmd.stream,
                &mut state,
                &config,
                command,
                runtime_spawn::SpawnOptions {
                    args,
                    working_dir: working_dir.map(Into::into),
                    agent_name,
                    extra_env,
                    cage_wrapper,
                    stdin_mode,
                    initial_stdin_bytes,
                    process_group,
                    terminal_cols,
                    terminal_rows,
                    osc133,
                },
            ),
            Request::List => handle_list(cmd.stream, &mut state),
            Request::Gc => handle_gc(cmd.stream, &mut state),
            Request::Inspect { id } => handle_inspect(cmd.stream, &mut state, id),
            Request::Logs { id, limit } => handle_logs(cmd.stream, &state, id, limit),
            Request::Tell {
                id,
                target,
                message,
            } => handle_tell(cmd.stream, &state, id, target, message),
            Request::Stop { id } => handle_stop(cmd.stream, &mut state, id),
            Request::Kill { id, target } => handle_kill(cmd.stream, &mut state, id, target),
            Request::Attach {
                id,
                target,
                cols,
                rows,
            } => handle_attach(
                cmd.stream,
                &state,
                AttachArgs {
                    id,
                    target,
                    winsize: cols.zip(rows),
                },
            ),
            // Journal-stream frames only reach the actor when the daemon
            // failed to start its hub (host-less). The REPL treats this
            // error as "stale/host-less daemon" and falls back to owning
            // `orkia.sock` itself. When the hub IS hosted, `handle_client`
            // intercepts these before they reach the actor.
            Request::Subscribe
            | Request::JournalEmit { .. }
            | Request::JobScope { .. }
            | Request::JobEventEmit { .. }
            | Request::McpProxyReply { .. } => {
                let mut stream = cmd.stream;
                let _ = write_error(
                    &mut stream,
                    "journal hub not hosted by this daemon".to_string(),
                );
            }
            Request::Shutdown => {
                let mut stream = cmd.stream;
                let _ = write_ok(&mut stream);
                shutdown.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
}

fn base_response() -> Response {
    Response {
        version: PROTOCOL_VERSION,
        ok: true,
        error: None,
        status: None,
        job: None,
        jobs: Vec::new(),
        logs: Vec::new(),
    }
}

fn handle_status(mut stream: UnixStream, state: &mut DaemonState, config: &ShellConfig) {
    reap_dead(state);
    let mut resp = base_response();
    resp.status = Some(DaemonStatus {
        state: "running".to_string(),
        protocol_version: PROTOCOL_VERSION,
        pid: Some(std::process::id()),
        socket: socket_path(&config.data_dir).display().to_string(),
        jobs: state.jobs.len(),
    });
    let _ = write_response(&mut stream, resp);
}

fn handle_gc(mut stream: UnixStream, state: &mut DaemonState) {
    reap_dead(state);
    let removed = recovery::gc_cached_jobs(state);
    let mut resp = base_response();
    resp.jobs = removed;
    let _ = write_response(&mut stream, resp);
}

fn handle_spawn(
    mut stream: UnixStream,
    state: &mut DaemonState,
    config: &ShellConfig,
    command: String,
    mut opts: runtime_spawn::SpawnOptions,
) {
    // Fallback cage resolution for detached paths that do not thread it on the
    // request themselves (`--detach -c`, RFC dispatch). The REPL sets it
    // explicitly; here we resolve from the daemon's `[cage]` config + the spawn's
    // agent (the request's `agent_name`, else parsed from the command) so EVERY
    // detached spawn is caged identically — never silently uncaged when the user
    // enabled the cage.
    if opts.cage_wrapper.is_none() {
        let agent = opts
            .agent_name
            .clone()
            .unwrap_or_else(|| job_name_for_command(&command));
        if let Some((cage_bin, policy_path)) = orkia_shell::resolve_detached_cage(config, &agent) {
            opts.cage_wrapper = Some(protocol::CageWrapperProto {
                cage_bin,
                policy_path,
            });
        }
    }
    let id = match state.alloc_id() {
        Ok(id) => id,
        Err(err) => {
            let _ = write_error(&mut stream, err);
            return;
        }
    };
    let control_sock = runtime_control::control_socket_path(&config.data_dir, id);
    if let Err(err) = runtime_spawn::trust_detached_cwd(config, opts.working_dir.as_deref()) {
        let _ = write_error(&mut stream, err);
        return;
    }
    let engine = match runtime_spawn::start(config, &command, id, &control_sock, opts) {
        Ok(engine) => engine,
        Err(err) => {
            let _ = write_error(&mut stream, err);
            return;
        }
    };
    let job = DaemonJob {
        id,
        agent: job_name_for_command(&command),
        label: command.clone(),
        started_at: Instant::now(),
        engine,
        control_sock,
    };
    let info = job_info(&job);
    audit::write_job_cache(&state.data_dir, &info);
    state.jobs.insert(id, job);
    audit::emit_event(
        &state.data_dir,
        "detached.spawn",
        id,
        None,
        Some(command.as_str()),
    );
    let mut resp = base_response();
    resp.job = Some(info);
    let _ = write_response(&mut stream, resp);
}

fn handle_list(mut stream: UnixStream, state: &mut DaemonState) {
    reap_dead(state);
    let mut jobs: Vec<_> = state.jobs.values().map(job_info).collect();
    for job in &jobs {
        audit::write_job_cache(&state.data_dir, job);
    }
    recovery::merge_cached_jobs(&mut jobs, state);
    let mut resp = base_response();
    resp.jobs = jobs;
    let _ = write_response(&mut stream, resp);
}

fn handle_inspect(mut stream: UnixStream, state: &mut DaemonState, id: u32) {
    reap_dead(state);
    let job = state
        .jobs
        .get(&id)
        .map(job_info)
        .or_else(|| audit::read_job_cache(&state.data_dir, id));
    let Some(job) = job else {
        let _ = write_error(&mut stream, format!("job {id} not found"));
        return;
    };
    let mut resp = base_response();
    resp.job = Some(job);
    let _ = write_response(&mut stream, resp);
}

fn handle_logs(mut stream: UnixStream, state: &DaemonState, id: u32, limit: Option<usize>) {
    let Some(job) = state
        .jobs
        .get(&id)
        .map(job_info)
        .or_else(|| audit::read_job_cache(&state.data_dir, id))
    else {
        let _ = write_error(&mut stream, format!("job {id} not found"));
        return;
    };
    let mut resp = base_response();
    resp.logs = audit::read_job_logs(&state.data_dir, &job, limit.unwrap_or(100));
    let _ = write_response(&mut stream, resp);
}

fn handle_tell(
    mut stream: UnixStream,
    state: &DaemonState,
    id: u32,
    target: String,
    message: String,
) {
    let Some(job) = state.jobs.get(&id) else {
        recovery::handle_tell(stream, state, id, target, message);
        return;
    };
    if !job_accepts_target(job, &target) {
        let _ = write_error(
            &mut stream,
            format!("job {id} has no target {target}; available: {}", job.agent),
        );
        return;
    }
    match runtime_control::tell_with_retry(&job.control_sock, &target, &message) {
        Ok(()) => {
            audit::emit_event(
                &state.data_dir,
                "detached.tell",
                id,
                Some(target.as_str()),
                None,
            );
            let _ = write_ok(&mut stream);
        }
        Err(err) => {
            let _ = write_error(&mut stream, err);
        }
    }
}

fn handle_kill(mut stream: UnixStream, state: &mut DaemonState, id: u32, target: Option<String>) {
    if let Some(target) = target {
        let Some(job) = state.jobs.get(&id) else {
            recovery::handle_kill_target(stream, state, id, target);
            return;
        };
        if !job_accepts_target(job, &target) {
            let _ = write_error(
                &mut stream,
                format!("job {id} has no target {target}; available: {}", job.agent),
            );
            return;
        }
        match runtime_control::kill_with_retry(&job.control_sock, &target) {
            Ok(()) => {
                audit::emit_event(
                    &state.data_dir,
                    "detached.kill_stage",
                    id,
                    Some(target.as_str()),
                    None,
                );
                let _ = write_ok(&mut stream);
            }
            Err(err) => {
                let _ = write_error(&mut stream, err);
            }
        }
        return;
    }
    let Some(job) = state.jobs.remove(&id) else {
        recovery::handle_kill(stream, state, id);
        return;
    };
    // Kill requests the end-state "job dead". A child that already
    // exited (e.g. a prior targeted `kill <id>:@stage` reaped the only
    // stage) satisfies that goal, so it is success — fall through to
    // clean up the cache and emit the audit event idempotently, just as
    // the missing-job branch above does.
    if let Err(err) = job.engine.signal(libc::SIGTERM)
        && !err.is_child_already_exited()
    {
        let _ = write_error(&mut stream, format!("kill job {id}: {err}"));
        return;
    }
    audit::remove_job_cache(&state.data_dir, id);
    audit::emit_event(&state.data_dir, "detached.kill", id, None, None);
    let _ = write_ok(&mut stream);
}

fn handle_stop(mut stream: UnixStream, state: &mut DaemonState, id: u32) {
    let Some(job) = state.jobs.remove(&id) else {
        recovery::handle_kill(stream, state, id);
        return;
    };
    if let Err(err) = job.engine.signal(libc::SIGTERM) {
        let _ = write_error(&mut stream, format!("stop job {id}: {err}"));
        return;
    }
    let mut info = job_info(&job);
    info.state = "stopped".to_string();
    info.attachable = false;
    for stage in &mut info.stages {
        stage.state = "stopped".to_string();
        stage.attachable = false;
    }
    audit::write_job_cache(&state.data_dir, &info);
    audit::emit_event(&state.data_dir, "detached.stop", id, None, None);
    let _ = write_ok(&mut stream);
}

/// Attach parameters bundled to respect the 4-argument limit.
/// `winsize` is the attaching terminal's `(cols, rows)` — forwarded so
/// the target PTY resizes BEFORE the catch-up paint; `None` keeps the
/// agent's current size (non-tty clients).
struct AttachArgs {
    id: u32,
    target: Option<String>,
    winsize: Option<(u16, u16)>,
}

/// Validate the attach on the actor, then hand the BLOCKING splice/pump to a
/// dedicated thread. The actor must never park inside an attach session: it
/// is the single dispatcher for every daemon request, so an inline splice
/// locked out `kill`/`ps`/`spawn` (and a `kill` of the attached job itself)
/// until the client detached (#1 — never block the heartbeat).
fn handle_attach(mut stream: UnixStream, state: &DaemonState, args: AttachArgs) {
    let AttachArgs {
        id,
        target,
        winsize,
    } = args;
    if let Some(target) = target {
        let Some(job) = state.jobs.get(&id) else {
            let _ = write_error(&mut stream, attach_unavailable(state, id));
            return;
        };
        if !job_accepts_target(job, &target) {
            let _ = write_error(
                &mut stream,
                format!("job {id} has no target {target}; available: {}", job.agent),
            );
            return;
        }
        let control_sock = job.control_sock.clone();
        let data_dir = state.data_dir.clone();
        spawn_attach_thread(id, move || {
            match runtime_control::attach_proxy(&control_sock, &target, winsize, stream) {
                Ok(()) => {
                    audit::emit_event(&data_dir, "detached.attach_stage", id, Some(&target), None);
                }
                Err(err) => {
                    eprintln!("orkia pty-daemon: attach stage {id}:{target}: {err}");
                }
            }
        });
        return;
    }
    let (history, snapshot, rx, writer) = {
        let Some(job) = state.jobs.get(&id) else {
            let _ = write_error(&mut stream, attach_unavailable(state, id));
            return;
        };
        (
            job.engine.history_snapshot(),
            job.engine.render_visible_snapshot(),
            job.engine.subscribe_output(),
            job.engine.writer(),
        )
    };
    if write_ok(&mut stream).is_err() {
        return;
    }
    audit::emit_event(&state.data_dir, "detached.attach", id, None, None);
    spawn_attach_thread(id, move || {
        attach::pump(stream, history, snapshot, rx, writer);
    });
}

/// Error text when a job is absent from the live roster. A finished job is
/// "done", not "lost its PTY" — consult the cache so the client gets the
/// truth instead of an alarming lost-PTY message for a normal exit.
fn attach_unavailable(state: &DaemonState, id: u32) -> String {
    match audit::read_job_cache(&state.data_dir, id) {
        Some(job) if matches!(job.state.as_str(), "done" | "stopped" | "failed") => {
            format!("job {id} is {}; the agent already exited", job.state)
        }
        Some(_) => format!("job {id} is not attachable; daemon no longer owns its PTY"),
        None => format!("job {id} not found"),
    }
}

/// Run an attach splice off-actor. A spawn failure only loses this attach —
/// the client sees its stream close and exits cleanly.
fn spawn_attach_thread(id: u32, splice: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name(format!("orkia-daemon-attach-{id}"))
        .spawn(splice);
}

fn reap_dead(state: &mut DaemonState) {
    let done: Vec<u32> = state
        .jobs
        .iter()
        .filter_map(|(id, job)| match job.engine.try_wait() {
            Ok(Some(_)) => Some(*id),
            _ => None,
        })
        .collect();
    for id in done {
        if let Some(job) = state.jobs.remove(&id) {
            let mut info = job_info(&job);
            info.state = "done".to_string();
            for stage in &mut info.stages {
                stage.state = "done".to_string();
            }
            audit::write_job_cache(&state.data_dir, &info);
            audit::emit_event(&state.data_dir, "detached.complete", id, None, None);
        }
    }
}

fn job_info(job: &DaemonJob) -> DaemonJobInfo {
    let state = if job.engine.is_alive() {
        "detached".to_string()
    } else {
        "done".to_string()
    };
    DaemonJobInfo {
        id: job.id,
        agent: job.agent.clone(),
        state: state.clone(),
        pid: job.engine.child_id(),
        label: job.label.clone(),
        runtime_secs: job.started_at.elapsed().as_secs(),
        control_socket: Some(job.control_sock.display().to_string()),
        pty_owner_pid: Some(std::process::id()),
        lost_reason: None,
        // engine caches the child's wait status, so a done job reports what
        // the agent actually exited with, not a hardcoded 0 (kept only as
        // the fallback for an unobservable status).
        exit_code: if state == "done" {
            Some(job.engine.try_wait().ok().flatten().unwrap_or(0))
        } else {
            None
        },
        seal_path: Some(seal_path(job.id)),
        attachable: state == "detached",
        stages: runtime_control::list(&job.control_sock).unwrap_or_else(|| fallback_stages(job)),
    }
}

fn fallback_stages(job: &DaemonJob) -> Vec<DaemonStageInfo> {
    job.agent
        .split('|')
        .filter(|target| !target.is_empty() && *target != "runtime" && *target != "orkia")
        .map(|target| DaemonStageInfo {
            id: 0,
            target: format!("@{target}"),
            state: if job.engine.is_alive() {
                "unknown".to_string()
            } else {
                "done".to_string()
            },
            pid: None,
            runtime_secs: job.started_at.elapsed().as_secs(),
            lost_reason: Some("runtime_control_unavailable".to_string()),
            exit_code: None,
            attachable: false,
        })
        .collect()
}

fn seal_path(id: u32) -> String {
    format!("agents/daemon/jobs/{id}/seal.jsonl")
}

fn job_name_for_command(command: &str) -> String {
    let labels = agent_labels(command);
    match labels.as_slice() {
        [] if command.trim_start().starts_with("orkia") => "orkia".to_string(),
        [] => "runtime".to_string(),
        [one] => one.clone(),
        many => many.join("|"),
    }
}

fn job_accepts_target(job: &DaemonJob, target: &str) -> bool {
    if let Ok(stage_id) = target.parse::<u32>() {
        return runtime_control::list(&job.control_sock)
            .unwrap_or_default()
            .iter()
            .any(|stage| stage.id == stage_id);
    }
    job.agent
        .split('|')
        .any(|name| name.strip_prefix('@').unwrap_or(name) == target)
}
