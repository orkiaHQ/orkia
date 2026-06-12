// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0

use std::io::Write as _;
use std::path::{Path, PathBuf};

use orkia_shell::ShellConfig;
use orkia_terminal_core::{EngineConfig, TerminalEngine};

use super::protocol::CageWrapperProto;

/// Pre-trust the directory the detached runtime will actually run in, so the
/// agent's in-runtime trust gate (`dispatch_agent` → `select_prompt`) does not
/// fire. That gate blocks on `stdin.read_line`, which never returns in a
/// detached runtime (its stdin is the daemon's PTY with no user typing), so a
/// missing pre-trust hangs the job forever. `working_dir` is the same dir
/// passed to `start`; fall back to the daemon's cwd only when the caller did
/// not specify one (legacy parity).
pub(super) fn trust_detached_cwd(
    config: &ShellConfig,
    working_dir: Option<&Path>,
) -> Result<(), String> {
    let cwd = match working_dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().map_err(|e| format!("detached cwd: {e}"))?,
    };
    let mut trust =
        orkia_shell::trust::TrustRegistry::load(config.data_dir.join("trusted_dirs.json"));
    trust
        .trust(&cwd)
        .map_err(|e| format!("detached trust registry: {e}"))
}

/// Optional parity fields for a daemon-spawned agent. Every field
/// defaults to `None`/empty; absent fields reproduce today's
/// hardcoded behaviour exactly (backward-compatible).
pub(super) struct SpawnOptions {
    /// Explicit argv for the child. When empty the daemon uses
    /// `["-c", detached_runtime_command(command)]` as before.
    pub args: Vec<String>,
    /// Working directory for the child. `None` → daemon's cwd.
    pub working_dir: Option<PathBuf>,
    /// Value written to `ORKIA_AGENT_NAME`. `None` → not set.
    pub agent_name: Option<String>,
    /// Extra env vars merged on top of the daemon's hardcoded block.
    pub extra_env: Vec<(String, String)>,
    /// Cage wrapper. `None` → spawn `command` directly.
    pub cage_wrapper: Option<CageWrapperProto>,
    /// Stdin mode string: `"pty"`, `"inherit"`, `"null"`, or
    /// `"initial_bytes"`. `None` → PTY (today's behaviour).
    pub stdin_mode: Option<String>,
    /// Bytes written into the PTY master after spawn when
    /// `stdin_mode = "initial_bytes"`.
    pub initial_stdin_bytes: Vec<u8>,
    /// Process group mode string: `"new_session"` or `"inherit"`.
    /// `None` → new session (today's behaviour). Stored for future
    /// use; `EngineConfig` does not expose this knob yet so the field
    /// is intentionally unread for now.
    #[allow(dead_code)]
    pub process_group: Option<String>,
    /// Terminal width hint. `None` → query daemon's terminal.
    pub terminal_cols: Option<usize>,
    /// Terminal height hint. `None` → query daemon's terminal.
    pub terminal_rows: Option<usize>,
    /// When `true`, set OSC-133 capability env vars
    /// (`ORKIA=1`, `ORKIA_PROTOCOL_VERSION=1`, `TERM_PROGRAM=orkia`)
    /// instead of `TERM=xterm-256color`.
    pub osc133: bool,
}

pub(super) fn start(
    config: &ShellConfig,
    command: &str,
    id: u32,
    control_sock: &Path,
    opts: SpawnOptions,
) -> Result<TerminalEngine, String> {
    let (cols, rows) = resolve_dims(opts.terminal_cols, opts.terminal_rows);
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let (exec_cmd, exec_args) = build_argv(
        &exe.display().to_string(),
        command,
        opts.args,
        opts.cage_wrapper,
    );
    let env = build_env(
        config,
        id,
        control_sock,
        opts.agent_name,
        opts.extra_env,
        opts.osc133,
    );
    let cwd = opts.working_dir.or_else(|| std::env::current_dir().ok());

    let engine = TerminalEngine::start(EngineConfig {
        init_cols: cols,
        init_rows: rows,
        cmd: Some(exec_cmd),
        args: exec_args,
        env,
        cwd,
        persistent_program: true,
        ..EngineConfig::default()
    })
    .map_err(|e| format!("start detached orkia runtime PTY: {e}"))?;

    maybe_write_initial_bytes(&engine, id, opts.stdin_mode, opts.initial_stdin_bytes);

    Ok(engine)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_dims(cols: Option<usize>, rows: Option<usize>) -> (usize, usize) {
    match (cols, rows) {
        (Some(c), Some(r)) => (c, r),
        (c, r) => {
            let (dc, dr) = terminal_dims();
            (c.unwrap_or(dc), r.unwrap_or(dr))
        }
    }
}

/// Build `(exec_cmd, argv)`. Without a cage wrapper this is the
/// orkia binary with `["-c", detached_runtime_command(command)]`
/// (today's hardcoded path). With explicit args (non-empty) those
/// replace the default `["-c", …]` but are still optionally wrapped.
fn build_argv(
    exe: &str,
    command: &str,
    args: Vec<String>,
    cage_wrapper: Option<CageWrapperProto>,
) -> (String, Vec<String>) {
    let base_cmd = exe.to_string();
    let base_args = if args.is_empty() {
        vec!["-c".to_string(), detached_runtime_command(command)]
    } else {
        args
    };

    match cage_wrapper {
        None => (base_cmd, base_args),
        Some(w) => {
            let mut wrapped = Vec::with_capacity(base_args.len() + 4);
            wrapped.push("--policy".to_string());
            wrapped.push(w.policy_path);
            wrapped.push("--".to_string());
            wrapped.push(base_cmd);
            wrapped.extend(base_args);
            (w.cage_bin, wrapped)
        }
    }
}

fn build_env(
    config: &ShellConfig,
    id: u32,
    control_sock: &Path,
    agent_name: Option<String>,
    extra_env: Vec<(String, String)>,
    osc133: bool,
) -> Vec<(String, String)> {
    // Hardcoded block — always present (today's behaviour).
    let mut env = vec![
        ("ORKIA_DETACHED_JOB_ID".to_string(), id.to_string()),
        (
            "ORKIA_DETACHED_CONTROL_SOCK".to_string(),
            control_sock.display().to_string(),
        ),
        (
            "ORKIA_RUNTIME_CONTROL_TIMEOUT_MS".to_string(),
            config.daemon.ipc_timeout_ms.max(50).to_string(),
        ),
    ];

    // OSC-133 capability env OR the baseline TERM.
    if osc133 {
        env.push(("ORKIA".to_string(), "1".to_string()));
        env.push(("ORKIA_PROTOCOL_VERSION".to_string(), "1".to_string()));
        env.push(("TERM_PROGRAM".to_string(), "orkia".to_string()));
    } else {
        env.push(("TERM".to_string(), "xterm-256color".to_string()));
    }

    if let Some(name) = agent_name {
        env.push(("ORKIA_AGENT_NAME".to_string(), name));
    }

    // Caller-supplied extras last so they can override anything above.
    env.extend(extra_env);
    env
}

/// Write initial bytes into the PTY master when `stdin_mode` is
/// `"initial_bytes"` and the byte slice is non-empty. Non-fatal:
/// a race between exec and the PTY being readable can swallow the
/// first bytes for some agents (mirrors the in-process spawn path).
fn maybe_write_initial_bytes(
    engine: &TerminalEngine,
    id: u32,
    stdin_mode: Option<String>,
    mut initial_bytes: Vec<u8>,
) {
    if stdin_mode.as_deref() != Some("initial_bytes") || initial_bytes.is_empty() {
        return;
    }
    if !initial_bytes.ends_with(b"\n") {
        initial_bytes.push(b'\n');
    }
    let writer = engine.writer();
    let mut w = writer.lock();
    if let Err(e) = w.write_all(&initial_bytes).and_then(|()| w.flush()) {
        tracing::warn!("daemon job {id}: initial bytes write failed: {e}");
    }
}

fn detached_runtime_command(command: &str) -> String {
    let trimmed = command.trim_end();
    if trimmed.ends_with('&') {
        trimmed.to_string()
    } else {
        format!("{trimmed} &")
    }
}

fn terminal_dims() -> (usize, usize) {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    if rc == 0 && size.ws_col > 0 && size.ws_row > 0 {
        (size.ws_col as usize, size.ws_row as usize)
    } else {
        (120, 42)
    }
}
