// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Interactive shell-mode renderer.
//!
//! Default at launch (no `--tui`, no `--no-tui`). Behaves like zsh/bash:
//! - Terminal emulator keeps the screen (no alt screen, no ratatui).
//! - Diagnostics (welcome, routing info, errors) go to **stderr**;
//!   command stdout stays on **stdout** so pipes work.
//! - Line input uses `rustyline` for arrow-up/down history, Ctrl-R
//!   reverse search, left/right cursor, Ctrl-W word delete. History is
//!   persisted to `~/.orkia/history` between sessions.
//!
//! Robustness notes:
//! - The rustyline `Editor` is built **lazily** on the first `read_line`
//!   call, so the welcome banner has a chance to fully flush before
//!   rustyline asks the terminal about its capabilities. (An earlier
//!   eager-construction attempt caused boot-time regressions on some
//!   iTerm profiles.)
//! - If `Editor::with_config` ever fails (no TTY, capability detection
//!   error, etc.), we transparently fall back to `stdin.read_line` so
//!   the shell stays usable.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use orkia_shell_types::decision::BlockContent;
use orkia_shell_types::renderer::{PromptContext, RenderEvent, ShellRenderer};
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::{
    Cmd, CompletionType, ConditionalEventHandler, Config, Editor, Event, EventContext,
    EventHandler, KeyEvent,
};

use crate::completion::{HelperShared, NullProvider, OrkiaHelper};
use arc_swap::ArcSwap;
use std::sync::Arc;

use super::{no_color_env, write_block};
use std::sync::atomic::{AtomicBool, Ordering};

/// Match bash's `HISTFILESIZE` default — keeps `~/.orkia/history`
/// bounded.
const HISTORY_MAX_ENTRIES: usize = 1000;

/// Tracks the lazy-init state of the line-editor backend. The large
/// `Editor` variant is boxed so the enum stays compact.
enum EditorState {
    /// Haven't tried yet. First `read_line` will attempt construction.
    NotYetBuilt,
    /// Built successfully; ready for `readline()` calls.
    Ready(Box<Editor<OrkiaHelper, FileHistory>>),
    /// Construction failed; permanent fallback to plain `stdin.read_line`.
    /// The error is surfaced once at construction time, then forgotten.
    Fallback,
}

pub struct ShellModeRenderer {
    editor: EditorState,
    history_path: Option<PathBuf>,
    /// Buffer used only by the fallback `read_line_plain` path.
    fallback_buf: String,
    /// Rustyline `ExternalPrinter` handle, lazily built out of the
    /// editor once it has been constructed. Lets async events
    /// (detector attentions) print above the live prompt line
    /// without corrupting whatever the user is typing. Boxed as a
    /// trait object so the concrete OS-specific `ExternalPrinter`
    /// type from `rustyline::tty::unix` / `tty::windows` stays
    /// behind the public trait surface.
    ext_printer: Option<Box<dyn rustyline::ExternalPrinter + Send>>,
    /// Rustyline helper installed at editor build time. Default helper
    /// uses [`NullProvider`] (Tab does nothing); the REPL replaces it
    /// via [`Self::set_completion_helper`] once the brush session is
    /// up.
    pending_helper: Option<OrkiaHelper>,
    /// Shared state the helper reads (agent list, history tail). Kept
    /// here so `read_line` can refresh `history_tail` before each
    /// prompt without touching the editor.
    helper_shared: Arc<ArcSwap<HelperShared>>,
    ctrl_g_attention: Arc<AtomicBool>,
    /// block colour even on a TTY. Decided once at construction.
    plain: bool,
}

impl Default for ShellModeRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellModeRenderer {
    pub fn new() -> Self {
        let history_path = std::env::var_os("HOME").map(|h| {
            let mut p = PathBuf::from(h);
            p.push(".orkia");
            p.push("history");
            p
        });
        Self {
            editor: EditorState::NotYetBuilt,
            history_path,
            fallback_buf: String::new(),
            ext_printer: None,
            pending_helper: None,
            helper_shared: HelperShared::new_arc(),
            ctrl_g_attention: Arc::new(AtomicBool::new(false)),
            plain: no_color_env(),
        }
    }

    /// Install the rustyline helper that drives tab-completion and
    /// inline hints. If the editor is already built, the helper is
    /// applied immediately; otherwise it's stored and used at editor
    /// construction time.
    pub fn set_completion_helper(
        &mut self,
        helper: OrkiaHelper,
        shared: Arc<ArcSwap<HelperShared>>,
    ) {
        self.helper_shared = shared;
        if let EditorState::Ready(ed) = &mut self.editor {
            ed.set_helper(Some(helper));
        } else {
            self.pending_helper = Some(helper);
        }
    }

    /// Access the shared state the helper reads (agents, history tail)
    /// so the REPL can push updates between prompts.
    pub fn helper_shared(&self) -> Arc<ArcSwap<HelperShared>> {
        self.helper_shared.clone()
    }

    /// Lazy construction of the rustyline editor. Called on the first
    /// `read_line` so any startup output (welcome, migration notice,
    /// workspace snapshot) has already drained to the terminal.
    fn ensure_editor(&mut self) {
        if !matches!(self.editor, EditorState::NotYetBuilt) {
            return;
        }
        // Flush stderr so the welcome banner doesn't interleave with
        // rustyline's first prompt redraw.
        let _ = io::stderr().flush();

        // candidate list under the prompt (fish/bash feel) instead of the
        // default `Circular` inline cycle.
        let config = match Config::builder().max_history_size(HISTORY_MAX_ENTRIES) {
            Ok(b) => b
                .auto_add_history(true)
                .completion_type(CompletionType::List)
                .build(),
            Err(_) => Config::default(),
        };

        match Editor::<OrkiaHelper, FileHistory>::with_config(config) {
            Ok(mut ed) => {
                let helper = self.pending_helper.take().unwrap_or_else(|| {
                    OrkiaHelper::new(Box::new(NullProvider), self.helper_shared.clone())
                });
                ed.set_helper(Some(helper));
                ed.bind_sequence(
                    Event::from(KeyEvent::ctrl('G')),
                    EventHandler::Conditional(Box::new(AttentionPullBinding {
                        requested: self.ctrl_g_attention.clone(),
                    })),
                );
                if let Some(path) = self.history_path.as_ref()
                    && path.exists()
                {
                    let _ = ed.load_history(path);
                }
                // Build the external printer once now so it's
                // available even before the first `read_line` call.
                // The handle survives independent of the editor.
                match ed.create_external_printer() {
                    Ok(p) => {
                        self.ext_printer = Some(Box::new(p));
                    }
                    Err(e) => {
                        tracing::warn!("shell_mode: external_printer unavailable: {e}");
                    }
                }
                self.editor = EditorState::Ready(Box::new(ed));
            }
            Err(e) => {
                // Surface the reason once so the user knows why arrows
                // don't work; we keep running in fallback mode.
                let _ = writeln!(
                    io::stderr(),
                    "  \x1b[33m[orkia]\x1b[0m readline disabled: {e} (falling back to plain stdin)"
                );
                self.editor = EditorState::Fallback;
            }
        }
    }

    fn save_history(&mut self) {
        if let (EditorState::Ready(ed), Some(path)) = (&mut self.editor, self.history_path.as_ref())
        {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = ed.save_history(path);
        }
    }
}

impl Drop for ShellModeRenderer {
    fn drop(&mut self) {
        self.save_history();
    }
}

impl ShellRenderer for ShellModeRenderer {
    fn publish(&mut self, event: RenderEvent) {
        match event {
            RenderEvent::Block(block) => publish_block(&block, self.plain),
            RenderEvent::RoutingInfo {
                agent,
                confidence,
                reason,
            } => {
                let mut err = io::stderr().lock();
                let _ = writeln!(
                    err,
                    "  \x1b[35m▸\x1b[0m routed to \x1b[1m{agent}\x1b[0m \x1b[90m({reason}, {pct:.0}%)\x1b[0m",
                    pct = confidence * 100.0
                );
            }
            RenderEvent::Welcome(info) => {
                // Dropped the `SEAL: N records` counter. With
                // scoped chains there's no single global total
                // worth showing; the `seal` builtin lists active
                // job + project chains on demand. Welcome stays
                // version + agent count.
                let mut err = io::stderr().lock();
                let _ = writeln!(
                    err,
                    "\n  \x1b[35m⬡ orkia\x1b[0m v{} \x1b[90m·\x1b[0m {} agent{}\n",
                    info.version,
                    info.agents.len(),
                    if info.agents.len() == 1 { "" } else { "s" },
                );
            }
            RenderEvent::JobUpdate(event) => publish_job_update(&event),
            RenderEvent::Prompt(_)
            | RenderEvent::JobsSnapshot(_)
            | RenderEvent::WorkspaceSnapshot(_)
            | RenderEvent::TeamSnapshot(_)
            | RenderEvent::CurrentTeamChanged { .. } => {
                // Sidebar / team snapshots are TUI-only.
            }
        }
    }

    fn take_external_print_sender(&mut self) -> Option<std::sync::mpsc::Sender<String>> {
        self.ensure_editor();
        // Move the printer out of the renderer and into a dedicated
        // worker thread. The thread loops on an mpsc receiver and
        // calls printer.print() for each message — guaranteeing that
        // toasts go out the moment they're produced, even while
        // rustyline is blocked inside a `readline()` call.
        let printer = self.ext_printer.take()?;
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        // The worker takes ownership of `printer`; on spawn failure the
        // closure is dropped and the printer with it, so the renderer
        // simply loses its external-print channel for this session.
        // Returning `None` makes callers fall back to inline rendering
        // rather than panicking the REPL — losing a toast is preferable
        // to losing the shell.
        match std::thread::Builder::new()
            .name("orkia-ext-printer".into())
            .spawn(move || {
                let mut printer = printer;
                while let Ok(mut msg) = rx.recv() {
                    if !msg.ends_with('\n') {
                        msg.push('\n');
                    }
                    if let Err(e) = printer.print(msg) {
                        tracing::warn!("ext-printer worker: print failed: {e}");
                    }
                }
            }) {
            Ok(_handle) => Some(tx),
            Err(err) => {
                tracing::error!(?err, "ext-printer worker spawn failed; toasts fall inline");
                None
            }
        }
    }

    fn install_completion_helper(
        &mut self,
        helper: Box<dyn std::any::Any + Send>,
    ) -> Option<Box<dyn std::any::Any + Send>> {
        match helper.downcast::<OrkiaHelper>() {
            Ok(boxed) => {
                let h = *boxed;
                if let EditorState::Ready(ed) = &mut self.editor {
                    ed.set_helper(Some(h));
                } else {
                    self.pending_helper = Some(h);
                }
                None
            }
            Err(other) => Some(other),
        }
    }

    fn completion_shared(&self) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        Some(Box::new(self.helper_shared.clone()))
    }

    fn external_print(&mut self, line: &str) {
        // Build the editor lazily on first external_print just like
        // we do for read_line, so async toasts at startup also use
        // the ExternalPrinter path (avoids racing the first prompt).
        self.ensure_editor();
        if let Some(p) = self.ext_printer.as_mut() {
            // rustyline's ExternalPrinter handles all the cursor
            // saving / line clearing / redraw of the prompt-line
            // buffer for us; we just feed it the bytes.
            let mut msg = line.to_string();
            if !msg.ends_with('\n') {
                msg.push('\n');
            }
            if p.print(msg).is_ok() {
                return;
            }
        }
        // Fallback: write to stderr directly. Will look a bit messy
        // if rustyline is mid-edit but at least the user sees it.
        use std::io::Write;
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{line}");
        let _ = err.flush();
    }

    fn read_line(&mut self, ctx: &PromptContext) -> Option<String> {
        // Notification cluster goes to stderr above the prompt so it
        // doesn't pollute rustyline's line buffer or the fallback path.
        if !ctx.notifications.is_empty() {
            let mut err = io::stderr().lock();
            let _ = writeln!(err);
            for line in &ctx.notifications {
                let _ = writeln!(err, "{line}");
            }
            let _ = writeln!(err);
            let _ = err.flush();
        }

        self.ensure_editor();
        let prompt = build_prompt_string(ctx);

        match &mut self.editor {
            EditorState::Ready(ed) => match ed.readline(&prompt) {
                Ok(line) => {
                    // Persist history incrementally so a crash later
                    // doesn't lose the just-typed command.
                    self.save_history();
                    // The REPL's classifier strips a trailing \n; preserve
                    // the historical contract by appending one.
                    Some(format!("{line}\n"))
                }
                Err(ReadlineError::Interrupted) => {
                    if self.ctrl_g_attention.swap(false, Ordering::SeqCst) {
                        return Some("attention pull\n".into());
                    }
                    // Ctrl-C — discard the partial line, give the user a
                    // fresh prompt on the next iteration.
                    let _ = writeln!(io::stderr());
                    Some(String::new())
                }
                Err(ReadlineError::Eof) => {
                    let _ = writeln!(io::stderr());
                    None
                }
                Err(_) => None,
            },
            EditorState::Fallback | EditorState::NotYetBuilt => self.read_line_plain(&prompt),
        }
    }

    // Shell mode has no widget-mode attach. Foreground PTY jobs go through
    // the yield/reclaim path (raw stdin↔PTY pump), implemented by the REPL
    // when it sees `is_attach_capable() == false`. Default no-ops are correct.
}

impl ShellModeRenderer {
    fn read_line_plain(&mut self, prompt: &str) -> Option<String> {
        {
            let mut err = io::stderr().lock();
            let _ = write!(err, "{prompt}");
            let _ = err.flush();
        }
        self.fallback_buf.clear();
        let stdin = io::stdin();
        match stdin.lock().read_line(&mut self.fallback_buf) {
            Ok(0) => {
                let _ = writeln!(io::stderr());
                None
            }
            Ok(_) => Some(self.fallback_buf.clone()),
            Err(_) => None,
        }
    }
}

fn build_prompt_string(ctx: &PromptContext) -> String {
    let approvals = if ctx.pending_approvals > 0 {
        format!(
            " \x1b[33m[{n} approval{plural}]\x1b[0m",
            n = ctx.pending_approvals,
            plural = if ctx.pending_approvals == 1 { "" } else { "s" },
        )
    } else {
        String::new()
    };
    let attention = ctx
        .attention_hint
        .as_ref()
        .map(|h| format!(" \x1b[33m{}\x1b[0m", h.render()))
        .unwrap_or_default();
    let cwd = compact_cwd(&ctx.cwd);
    let rfc = match &ctx.rfc_scope {
        Some(seg) => format!(" \x1b[36m{}\x1b[0m", seg.render()),
        None => String::new(),
    };
    // `[team]` (blue) only when the effective scope is non-Private.
    // Private (and unset) get no marker, keeping the prompt clean for
    // the default-case the vast majority of users see.
    let scope = match ctx.scope {
        Some(orkia_shell_types::Scope::Public) => " \x1b[33m[public]\x1b[0m".to_string(),
        Some(orkia_shell_types::Scope::Team) => " \x1b[34m[team]\x1b[0m".to_string(),
        _ => String::new(),
    };
    format!(
        "\x1b[35m⬡\x1b[0m \x1b[90m{cwd}\x1b[0m{rfc}{scope}{approvals}{attention} \x1b[35m❯\x1b[0m "
    )
}

struct AttentionPullBinding {
    requested: Arc<AtomicBool>,
}

impl ConditionalEventHandler for AttentionPullBinding {
    fn handle(
        &self,
        _evt: &Event,
        _n: rustyline::RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if ctx.line().is_empty() {
            self.requested.store(true, Ordering::SeqCst);
            Some(Cmd::Interrupt)
        } else {
            Some(Cmd::Noop)
        }
    }
}

fn publish_block(block: &BlockContent, plain: bool) {
    match block {
        // Plain command-style output goes to stdout (pipe-friendly).
        BlockContent::Text(_)
        | BlockContent::Attention { .. }
        | BlockContent::TableRow(_)
        | BlockContent::ToolCall { .. }
        | BlockContent::SealRecord { .. } => {
            let mut out = io::stdout().lock();
            let _ = write_block(&mut out, block, plain);
        }
        // Everything else is meta — prompt-side diagnostics → stderr.
        BlockContent::AgentMessage { .. }
        | BlockContent::Approval { .. }
        | BlockContent::Notice { .. }
        | BlockContent::SystemInfo(_)
        | BlockContent::Error(_) => {
            let mut err = io::stderr().lock();
            let _ = write_block(&mut err, block, plain);
        }
    }
}

// Shared with the stdout (`-c` one-shot) renderer: job notifications must
// read identically whether the shell is interactive or one-shot, and the
// id=0 SIGCHLD sentinel must never reach either surface.
pub(super) fn publish_job_update(event: &orkia_shell_types::job::JobEvent) {
    use orkia_shell_types::job::JobEvent::*;
    let id = event.job_id();
    // Suppress synthetic-wakeup events emitted by the SIGCHLD
    // handler — they carry id=0 as a sentinel to nudge the drain
    // loop, no user-visible output expected.
    if id.0 == 0 {
        return;
    }
    let mut err = io::stderr().lock();
    let _ = match event {
        Spawned { kind, pid, .. } => {
            // Bash conventionally formats spawn as `[N] PID`. We
            // keep the kind label too for orkia visibility.
            let pid_str = pid.map_or_else(String::new, |p| format!(" \x1b[90mpid={p}\x1b[0m"));
            writeln!(err, "  \x1b[90m[{id}] spawned: {kind}{pid_str}\x1b[0m")
        }
        Completed {
            exit_code, label, ..
        } => {
            // Bash: `[N]+ Done cmd` on 0, `[N]+ Exit N cmd` otherwise.
            // The label is the original command line per
            // SIGCHLD wakeup events (which return early above).
            let suffix = if label.is_empty() {
                String::new()
            } else {
                format!("                 {label}")
            };
            if *exit_code == 0 {
                writeln!(err, "  \x1b[32m[{id}]+ Done\x1b[0m{suffix}")
            } else {
                writeln!(err, "  \x1b[31m[{id}]+ Exit {exit_code}\x1b[0m{suffix}")
            }
        }
        Stopped { label, .. } => {
            let suffix = if label.is_empty() {
                String::new()
            } else {
                format!("              {label}")
            };
            writeln!(err, "  \x1b[33m[{id}]+ Stopped\x1b[0m{suffix}")
        }
        Continued { label, .. } => {
            let suffix = if label.is_empty() {
                String::new()
            } else {
                format!("            {label}")
            };
            writeln!(err, "  \x1b[90m[{id}]+ Continued\x1b[0m{suffix}")
        }
        Attached { .. } => writeln!(err, "  \x1b[90m[{id}] attached\x1b[0m"),
        Detached { .. } => writeln!(err, "  \x1b[90m[{id}] detached\x1b[0m"),
    };
}

/// Replace $HOME prefix with `~` so the prompt stays short.
fn compact_cwd(cwd: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if cwd == home.as_ref() {
            return "~".into();
        }
        if let Some(rest) = cwd.strip_prefix(home.as_ref())
            && rest.starts_with('/')
        {
            return format!("~{rest}");
        }
    }
    cwd.to_string()
}
