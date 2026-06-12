// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! brush-backed shell engine.
//!
//! Wraps a persistent `brush_core::Shell` so the orkia REPL can execute
//! POSIX/bash commands in-process: cd, export, alias, pipes, redirections,
//! globbing, and fork/exec — all without delegating to `$SHELL -c`.
//!
//! Boundary: orkia's classifier handles orkia builtins (`ps`, `seal`, ...)
//! and `@agent` syntax *before* anything reaches this engine. Everything
//! that lands here is a real shell command that brush parses and runs.

use std::path::Path;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use brush_builtins::ShellBuilderExt as _;
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::{ExecutionControlFlow, ProfileLoadBehavior, RcLoadBehavior, Shell, SourceInfo};
use orkia_pty::AdoptedPty;
use orkia_terminal_core::{AdoptMaster, RawOutputRx, TerminalEngine};

use crate::error::ShellError;

pub mod pty;

const DEFAULT_PTY_COLS: usize = 120;
const DEFAULT_PTY_ROWS: usize = 40;
const READ_BUF_BYTES: usize = 64 * 1024;
/// After a command returns from brush, allow a brief settle window for
/// any pending PTY bytes to surface on the master side before we publish
/// the captured block. 50ms is enough for slow forks on a busy machine
/// but invisible to a human at the prompt.
const PTY_DRAIN_SETTLE: Duration = Duration::from_millis(50);
const PTY_DRAIN_POLL: Duration = Duration::from_millis(10);

/// Longest alt-screen-enter sequence we scan for (`\x1b[?1049h`); used as the
/// re-scan overlap so a sequence split across PTY reads is still matched.
const ALT_SCREEN_ENTER_MAX: usize = 8;

/// True if `buf` contains an alt-screen enter (`?1049h` / `?1047h` / `?47h`) —
/// the runtime signal that a foreground command went full-screen and must own
/// the terminal live rather than be captured as a block. Detecting the program
/// *at runtime* (not by name) is the whole point: any full-screen TUI is
/// promoted; ordinary line commands never are.
fn contains_alt_screen_enter(buf: &[u8]) -> bool {
    const PATTERNS: [&[u8]; 3] = [b"\x1b[?1049h", b"\x1b[?1047h", b"\x1b[?47h"];
    PATTERNS
        .iter()
        .any(|p| buf.windows(p.len()).any(|w| w == *p))
}

/// Result of executing a single shell command.
#[derive(Debug, Clone, Copy)]
pub struct ExecuteResult {
    /// Exit code reported by brush for the command (or the last in a pipeline).
    pub exit_code: u8,
    /// True iff the command triggered the `exit` builtin and the REPL should terminate.
    pub should_exit: bool,
}

/// Knobs for RC sourcing. Defaults to a clean engine (no rc files) so
/// tests stay isolated from whatever lives in the developer's
/// `$HOME/.bashrc`. The production binary opts in by constructing
/// [`Self::production`] (or building one manually with both load flags
/// enabled).
#[derive(Debug, Clone, Copy, Default)]
pub struct ShellEngineOptions {
    /// Source `~/.bashrc` on startup (non-login shells). Default `false`.
    pub load_bashrc: bool,
    /// Source `~/.bash_profile` / `~/.bash_login` / `~/.profile` on
    /// startup (login shells). Default `false`.
    pub load_profile: bool,
    /// Treat this engine as a login shell. Login shells source the
    /// profile chain; non-login shells source `.bashrc`. Default `false`.
    pub login: bool,
}

impl ShellEngineOptions {
    /// Production defaults: load `.bashrc` and `.profile` (the latter
    /// only matters when `login` is set). Caller can flip individual
    /// flags before passing to [`ShellEngine::new_with_options`].
    pub const fn production() -> Self {
        Self {
            load_bashrc: true,
            load_profile: true,
            login: false,
        }
    }

    /// Mark this engine as a login shell. Switches RC sourcing from the
    /// `.bashrc` branch to the `.bash_profile`/`.bash_login`/`.profile`
    /// chain (bash's convention).
    pub const fn with_login(mut self, login: bool) -> Self {
        self.login = login;
        self
    }
}

/// Persistent in-process shell. One instance per orkia session.
pub struct ShellEngine {
    shell: Shell,
}

impl ShellEngine {
    /// Build a fresh shell with default options (no RC files sourced).
    /// Equivalent to `new_with_options(ShellEngineOptions::default())`.
    /// Kept for tests and embedders that want a hermetic engine.
    pub async fn new() -> Result<Self, ShellError> {
        Self::new_with_options(ShellEngineOptions::default()).await
    }

    /// Build a fresh shell. RC sourcing is **deferred** — the engine is
    /// returned without any `~/.bashrc` / `~/.profile` / `~/.orkiarc`
    /// executed. The caller (typically [`BrushSession`]) binds output
    /// FDs and then invokes [`Self::source_default_rc`] +
    /// [`Self::source_if_exists`] so rc-script output goes through the
    /// orkia-owned PTY rather than the process's real stdout.
    ///
    /// The `opts` are stored for later use by `source_default_rc`.
    pub async fn new_with_options(_opts: ShellEngineOptions) -> Result<Self, ShellError> {
        let shell = Shell::builder()
            .default_builtins(brush_builtins::BuiltinSet::BashMode)
            .interactive(false)
            .no_editing(true)
            .rc(RcLoadBehavior::Skip)
            .profile(ProfileLoadBehavior::Skip)
            .build()
            .await
            .map_err(|e| ShellError::Other(format!("brush init: {e}")))?;
        Ok(Self { shell })
    }

    /// Source the default RC chain per [`ShellEngineOptions`]:
    ///
    /// - login + load_profile: first of `~/.bash_profile`,
    ///   `~/.bash_login`, `~/.profile` that exists.
    /// - non-login + load_bashrc: `~/.bashrc`.
    ///
    /// Errors during a single rc file are returned as a list of
    /// `(path, error)` pairs so the caller can surface them without
    /// aborting startup — a broken `.bashrc` must not prevent orkia
    /// from coming up.
    pub async fn source_default_rc(
        &mut self,
        opts: ShellEngineOptions,
    ) -> Vec<(std::path::PathBuf, ShellError)> {
        let mut warnings = Vec::new();
        let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
            return warnings;
        };

        if opts.login && opts.load_profile {
            for candidate in [".bash_profile", ".bash_login", ".profile"] {
                let path = home.join(candidate);
                match self.source_if_exists(&path).await {
                    Ok(true) => break, // bash convention: only the first.
                    Ok(false) => continue,
                    Err(e) => {
                        warnings.push((path, e));
                        break;
                    }
                }
            }
        } else if !opts.login && opts.load_bashrc {
            let path = home.join(".bashrc");
            if let Err(e) = self.source_if_exists(&path).await {
                warnings.push((path, e));
            }
        }
        warnings
    }

    /// Execute one command line. Output goes wherever the shell's
    /// `open_files` point (PTY slave in the live REPL; pipes in tests).
    pub async fn execute(&mut self, line: &str) -> Result<ExecuteResult, ShellError> {
        let params = self.shell.default_exec_params();
        let src = SourceInfo::from("<orkia>");
        let result = self
            .shell
            .run_string(line.to_owned(), &src, &params)
            .await
            .map_err(|e| ShellError::Other(format!("brush exec: {e}")))?;

        let exit_code: u8 = (&result.exit_code).into();
        let should_exit = matches!(result.next_control_flow, ExecutionControlFlow::ExitShell);
        Ok(ExecuteResult {
            exit_code,
            should_exit,
        })
    }

    /// Source a script file if it exists. Used for `~/.bashrc`,
    /// `~/.profile`, and `~/.orkiarc`.
    ///
    /// Returns `Ok(true)` if the file existed and was sourced (regardless
    /// of its internal exit status), `Ok(false)` if missing.
    ///
    /// **stdin is replaced with `/dev/null` for the duration of the
    /// source.** Some real-world rc files (notably `nvm.sh` + its bash
    /// completion, or any rc that calls `read` defensively at the top
    /// level) read fd 0 at source time. If the caller's stdin is an
    /// interactive TTY with no input pending, the read blocks forever
    /// and orkia never finishes booting. /dev/null returns EOF instantly
    /// and any such reads complete with no data.
    ///
    /// **Parse errors are NOT propagated as `Err`.** brush prints a
    /// diagnostic to the shell's stderr and continues — that matches the
    /// not prevent startup. The caller can detect that something went
    /// wrong by inspecting [`Shell::last_exit_status`] (non-zero after a
    /// parse failure).
    pub async fn source_if_exists(&mut self, path: &Path) -> Result<bool, ShellError> {
        if !path.exists() {
            return Ok(false);
        }
        // Swap stdin → /dev/null around the source. Restore the previous
        // fd 0 binding afterwards (or remove it if there was none) so
        // subsequent calls — including `execute` in `-c` mode — see the
        // caller's intended stdin.
        let dev_null = std::fs::File::open("/dev/null")
            .map_err(|e| ShellError::Other(format!("open /dev/null: {e}")))?;
        let files = self.shell.open_files_mut();
        let saved_stdin = files.set_fd(OpenFiles::STDIN_FD, OpenFile::File(dev_null));

        let params = self.shell.default_exec_params();
        let result = self
            .shell
            .source_script(path, std::iter::empty::<String>(), &params)
            .await
            .map_err(|e| ShellError::Other(format!("brush source {}: {e}", path.display())));

        // Restore stdin regardless of success/failure.
        let files = self.shell.open_files_mut();
        match saved_stdin {
            Some(prev) => {
                files.set_fd(OpenFiles::STDIN_FD, prev);
            }
            None => {
                files.remove_fd(OpenFiles::STDIN_FD);
            }
        }

        result?;
        Ok(true)
    }

    /// Current working directory. brush owns cwd; orkia reads it for the prompt.
    pub fn cwd(&self) -> &Path {
        self.shell.working_dir()
    }

    /// Exit code of the last completed command.
    pub fn last_exit(&self) -> u8 {
        self.shell.last_exit_status()
    }

    /// Snapshot of exported variables, for propagation into spawned agents.
    pub fn exported_env(&self) -> Vec<(String, String)> {
        let shell = &self.shell;
        shell
            .env()
            .iter_exported()
            .filter_map(|(name, var)| {
                if !var.value().is_set() {
                    return None;
                }
                let value = var.value().to_cow_str(shell).into_owned();
                Some((name.clone(), value))
            })
            .collect()
    }

    /// Mutable access to the inner shell — used only for PTY binding via
    /// [`pty::bind_pty_to_shell`]. Not for general use.
    pub fn shell_mut(&mut self) -> &mut Shell {
        &mut self.shell
    }

    /// Defined alias names, read straight from brush's in-memory table
    /// (the single source of truth for aliases). Used by the line editor to
    /// A cheap hashmap key read, not a scan. Function names are not yet
    /// exposed by `brush_core::Shell` (deferred — needs a fork accessor).
    pub fn alias_names(&self) -> Vec<String> {
        self.shell.aliases().keys().cloned().collect()
    }
}

/// REPL-side bundle: a [`ShellEngine`] plus the orkia-owned PTY that
/// receives all of brush's (and its children's) output.
///
/// A single `BrushSession` lives for the whole orkia session. Each call
/// to [`Self::execute`] emits an OSC-133 `C` (command start) sequence,
/// runs the command via brush, then drains and returns the bytes that
/// flowed back over the PTY master, finishing with OSC-133 `D` (done).
pub struct BrushSession {
    engine: ShellEngine,
    /// Adopted-master engine wraps the orkia-side PTY. Held so the master
    /// fd stays alive for the whole session and so resize ioctls have a
    /// place to land.
    #[allow(
        dead_code,
        reason = "kept alive for fd ownership + future resize plumbing"
    )]
    pty: TerminalEngine,
    raw_output: RawOutputRx,
    /// RC-loading errors collected during startup. Caller drains via
    /// [`Self::take_rc_warnings`].
    rc_warnings: Vec<(std::path::PathBuf, ShellError)>,
}

impl BrushSession {
    /// Build a fresh brush session with default options (no rc files
    /// sourced). Equivalent to
    /// `new_with_options(ShellEngineOptions::default())`.
    pub async fn new() -> Result<Self, ShellError> {
        Self::new_with_options(ShellEngineOptions::default()).await
    }

    /// Build a fresh brush session: open a PTY pair, bind the slave to
    /// brush, adopt the master under a [`TerminalEngine`], and then
    /// — with output now flowing through the PTY — source the default RC
    /// chain per `opts`. Any rc-file errors are returned in the
    /// `rc_warnings` field of the result so the caller can surface them
    /// without blocking startup.
    pub async fn new_with_options(opts: ShellEngineOptions) -> Result<Self, ShellError> {
        let pair = orkia_pty::open_pair(DEFAULT_PTY_COLS, DEFAULT_PTY_ROWS)
            .map_err(|e| ShellError::Other(format!("open pty pair: {e}")))?;
        let AdoptedPty {
            reader,
            writer,
            master_fd,
            slave,
            dims,
            screen,
        } = pair;

        // Build the engine without rc sourcing; we'll do it ourselves
        // *after* the PTY is bound so banner/echo output lands in the
        // orkia-managed PTY rather than the parent process's stdout.
        let mut engine = ShellEngine::new_with_options(opts).await?;
        pty::bind_pty_to_shell(engine.shell_mut(), slave)?;

        let pty = TerminalEngine::adopt_master(AdoptMaster {
            reader,
            writer,
            master_fd,
            dims,
            screen,
            buf_bytes: READ_BUF_BYTES,
            // The brush PTY does not surface OSC 133 / APC to the
            // protocol layer (it's the orkia internal shell, not an
            // external agent). Agents get their listeners installed
            // in `job::spawn_agent` via the EngineConfig path.
            on_osc133: None,
            on_apc: None,
        })
        .map_err(|e| ShellError::Other(format!("adopt master: {e}")))?;

        let raw_output = pty
            .take_raw_output_rx()
            .ok_or_else(|| ShellError::Other("raw_output_rx already taken".into()))?;

        let mut session = Self {
            engine,
            pty,
            raw_output,
            rc_warnings: Vec::new(),
        };

        // Source ~/.bashrc or ~/.profile depending on opts. Errors are
        // collected, not propagated — a syntactically broken rc file
        // must not prevent orkia from coming up.
        session.rc_warnings = session.engine.source_default_rc(opts).await;
        // Discard whatever those rc files wrote to the PTY (banners,
        // motd, etc.) so they don't appear above the orkia welcome.
        session.drain_nonblocking();
        Ok(session)
    }

    /// Drain and return any RC-loading warnings collected during
    /// startup. Caller (REPL) typically surfaces these as
    /// `BlockContent::Error` blocks so the user sees what broke.
    pub fn take_rc_warnings(&mut self) -> Vec<(std::path::PathBuf, ShellError)> {
        std::mem::take(&mut self.rc_warnings)
    }

    /// Run one command line. Returns the brush exit code, the
    /// `exit`-builtin flag, and the bytes captured from the PTY master
    /// during the command (already drained for downstream block emission).
    ///
    /// We do **not** inject OSC-133 markers here: writes to the PTY master
    /// land in the slave's input buffer and are echoed back by the kernel
    /// tty line discipline, surfacing as `^[]133;C^G` literals in the
    /// captured output. Block segmentation, if needed, must use a
    /// different channel (the engine knows exactly when execution starts
    /// and ends — boundaries can be tracked here, not in-band).
    pub async fn execute(&mut self, line: &str) -> Result<CommandOutput, ShellError> {
        // Drop any output that arrived between calls (e.g. async writes
        // by previously backgrounded children).
        self.drain_nonblocking();

        // Size brush's PTY to the host terminal so a full-screen program draws
        // at the right dimensions from its first frame.
        if let Some((cols, rows)) = crate::job::raw_attach::read_terminal_size_via_tiocgwinsz() {
            let _ = self.pty.resize(cols, rows);
        }

        // Disjoint field borrows: the `exec` future holds `&mut engine` while
        // we read `raw_output` and (on promote) resize/write via `pty`.
        let Self {
            engine,
            raw_output,
            pty,
            ..
        } = self;
        let exec = engine.execute(line);
        tokio::pin!(exec);

        // alt-screen enter. Ordinary line commands finish here on the
        // unchanged capture path; a full-screen program trips the detector and
        // so the promoted program's first frame isn't lost.
        let mut captured = Vec::<u8>::new();
        let mut scan_from = 0usize;
        loop {
            tokio::select! {
                biased;
                r = &mut exec => {
                    let res = r?;
                    captured.extend(drain_settled(raw_output).await);
                    return Ok(CommandOutput {
                        exit_code: res.exit_code,
                        should_exit: res.should_exit,
                        bytes: captured,
                    });
                }
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    while let Ok(b) = raw_output.try_recv() {
                        captured.extend_from_slice(&b);
                    }
                    let from = scan_from.saturating_sub(ALT_SCREEN_ENTER_MAX);
                    if contains_alt_screen_enter(&captured[from..]) {
                        break;
                    }
                    scan_from = captured.len();
                }
            }
        }

        // the rest of its life. `raw_output` is moved into the splice and
        // returned afterwards so the next command can read the PTY again.
        let writer = pty.writer();
        let placeholder = {
            let (_tx, rx) = std::sync::mpsc::channel();
            rx
        };
        let raw_rx = std::mem::replace(raw_output, placeholder);
        let (res, returned) = crate::job::raw_attach::splice_brush_foreground(
            writer,
            raw_rx,
            pty,
            &captured,
            exec.as_mut(),
        )
        .await;
        *raw_output = returned;
        let res = res?;
        Ok(CommandOutput {
            exit_code: res.exit_code,
            should_exit: res.should_exit,
            bytes: Vec::new(),
        })
    }

    /// Source a script if it exists. Returns `Ok(true)` if the file
    /// existed (regardless of its internal exit status), `Ok(false)` if
    /// missing, `Err` on a parse/source failure (e.g. `.orkiarc` syntax
    /// error). The REPL surfaces `Err` as a non-fatal warning block.
    pub async fn source_if_exists(&mut self, path: &Path) -> Result<bool, ShellError> {
        let sourced = self.engine.source_if_exists(path).await?;
        // Discard banner / motd output the rc script may have produced.
        self.drain_nonblocking();
        Ok(sourced)
    }

    pub fn cwd(&self) -> &Path {
        self.engine.cwd()
    }

    pub fn exported_env(&self) -> Vec<(String, String)> {
        self.engine.exported_env()
    }

    /// prompt to keep the line-editor snapshot's dynamic command set fresh.
    pub fn alias_names(&self) -> Vec<String> {
        self.engine.alias_names()
    }

    /// Set the bash special variable `$!` (last-background-pid).
    /// orkia spawns background commands via `JobController::spawn_shell`
    /// — that path doesn't go through brush's own interpreter, so
    /// brush never updates `$!` on its own. We poke it here so the
    /// next foreground command (often `echo $!`) sees the pid.
    /// Best-effort: any error from the brush env layer is logged
    /// and swallowed.
    pub fn set_last_bg_pid(&mut self, pid: u32) {
        use brush_core::env::{EnvironmentLookup, EnvironmentScope};
        use brush_core::variables::ShellValueLiteral;
        let res = self.engine.shell_mut().env_mut().update_or_add(
            "!",
            ShellValueLiteral::Scalar(pid.to_string()),
            |_| Ok(()),
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        );
        if let Err(e) = res {
            tracing::warn!(error = %e, "brush: set $! failed");
        }
    }

    /// Set the bash special variable `$?` (last exit status). Builtins
    /// and typed commands resolve outside brush's interpreter, so brush
    /// never sees their codes on its own — the REPL pokes the tracked
    /// status in before the next shell line runs, making `echo $?` and
    /// `[ $? -eq 0 ]` truthful across the builtin/brush boundary
    pub fn set_last_status(&mut self, status: u8) {
        self.engine.shell_mut().set_last_exit_status(status);
    }

    /// Expand `line` into an argv via brush's tokeniser + word expander.
    /// Resolves `$VAR`, `~`, `*.rs`, command substitution, arithmetic,
    /// then field-splits on `IFS`. Does NOT execute. Does NOT expand
    /// aliases or invoke functions (those only fire on `run_string`).
    ///
    /// Used by `dispatch_shell(cmd &)` so backgrounded commands honour
    /// the same expansion semantics as foreground ones. Returns the
    /// final argv (`argv[0] = command`, rest = args). Empty input or
    /// pure-whitespace produces an empty vec — caller should treat
    /// that as a parse error.
    pub async fn expand_to_argv(&mut self, line: &str) -> Result<Vec<String>, ShellError> {
        let params = brush_core::ExecutionParameters::default();
        self.engine
            .shell_mut()
            .full_expand_and_split_string(&params, line)
            .await
            .map_err(|e| ShellError::Other(format!("expand: {e}")))
    }

    /// Run brush's completion engine on `(line, pos)` and return the raw
    /// `Completions` result. Errors are surfaced to the caller; the
    /// rustyline bridge maps them to "no candidates".
    pub async fn complete(
        &mut self,
        line: &str,
        pos: usize,
    ) -> Result<brush_core::completion::Completions, ShellError> {
        self.engine
            .shell_mut()
            .complete(line, pos)
            .await
            .map_err(|e| ShellError::Other(format!("complete: {e}")))
    }

    fn drain_nonblocking(&mut self) {
        while self.raw_output.try_recv().is_ok() {}
    }
}

/// Collect every byte already queued on the PTY master plus anything that
/// arrives within the settle window. ANSI escapes (`ls --color`, OSC-133
/// markers) are preserved so the outer terminal renders them. Free function so
/// `execute` can call it while holding a destructured `&mut raw_output`.
async fn drain_settled(raw_output: &mut RawOutputRx) -> Vec<u8> {
    let mut out = Vec::<u8>::new();
    loop {
        match raw_output.try_recv() {
            Ok(b) => out.extend_from_slice(&b),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => return out,
        }
    }
    // Settle window: poll for late bytes, yielding to the runtime between polls
    // instead of blocking the tokio worker on `recv_timeout` (BUG-066).
    let deadline = Instant::now() + PTY_DRAIN_SETTLE;
    while Instant::now() < deadline {
        match raw_output.try_recv() {
            Ok(b) => out.extend_from_slice(&b),
            Err(TryRecvError::Empty) => tokio::time::sleep(PTY_DRAIN_POLL).await,
            Err(TryRecvError::Disconnected) => break,
        }
    }
    out
}

/// Captured result of one `BrushSession::execute` call.
pub struct CommandOutput {
    pub exit_code: u8,
    pub should_exit: bool,
    /// Raw bytes drained from the PTY master, in order. Includes any
    /// ANSI escape sequences emitted by the command or by the OSC-133
    /// boundary markers.
    pub bytes: Vec<u8>,
}
