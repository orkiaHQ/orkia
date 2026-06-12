// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use crate::agent::AgentInfo;
use crate::attached::{AttachedHandle, AttachedOutcome};
use crate::attention::AttentionHint;
use crate::decision::BlockContent;
use crate::job::{JobEvent, JobInfo};
use crate::scope::Scope;
use crate::workspace::Workspace;

#[derive(Debug, Clone)]
pub struct PromptContext {
    pub cwd: String,
    pub agent_count: usize,
    pub seal_active: bool,
    pub connected: bool,
    pub pending_approvals: usize,
    pub attention_hint: Option<AttentionHint>,
    /// Pre-rendered notification lines (ANSI-coloured) to print right
    /// before the prompt. Filled by the REPL from journal envelopes
    /// drained since the last tick — agent tool use, completions,
    /// approvals. Empty in the common case.
    pub notifications: Vec<String>,
    /// When set (`rfc cd <id>` has been issued), the prompt should render an
    /// `rfc:<id>(<state>, v<n>) [N ask, M review]`.
    pub rfc_scope: Option<RfcScopeSegment>,
    /// Effective visibility scope of the artifact currently in scope
    /// (resolved cwd → project → workspace default). When `Some` and
    /// non-`Private`, the prompt renders a `[public]` / `[team]`
    /// marker. `Private` and `None` render nothing — keeps the prompt
    pub scope: Option<Scope>,
}

/// Pre-rendered RFC scope segment for the prompt. The REPL fills this from
/// its session-scoped `rfc_scope` field; renderers stringify it directly.
#[derive(Debug, Clone)]
pub struct RfcScopeSegment {
    pub id: String,
    pub state: String,
    pub version: u32,
    pub open_clarifications: u32,
    pub unreviewed_decisions: u32,
}

impl RfcScopeSegment {
    /// `rfc:<id>(<state>, v<n>) [N ask, M review]` — but the bracketed
    /// counter is elided when both counts are zero.
    pub fn render(&self) -> String {
        let base = format!("rfc:{}({}, v{})", self.id, self.state, self.version);
        if self.open_clarifications == 0 && self.unreviewed_decisions == 0 {
            base
        } else {
            format!(
                "{base} [{} ask, {} review]",
                self.open_clarifications, self.unreviewed_decisions
            )
        }
    }
}

#[derive(Debug, Clone)]
pub enum RenderEvent {
    Block(BlockContent),
    RoutingInfo {
        agent: String,
        confidence: f32,
        reason: String,
    },
    Prompt(PromptContext),
    Welcome(WelcomeInfo),
    JobUpdate(JobEvent),
    /// Snapshot of current job list — emitted whenever the job table changes.
    JobsSnapshot(Vec<JobInfo>),
    /// Snapshot of current workspace (projects/rfcs/issues) — emitted after mutations.
    WorkspaceSnapshot(Workspace),
    /// invites, shared projects, caller's `team_scope`). REPL emits
    /// after each [`crate::team::TeamSnapshot`] refresh so the TUI
    /// widgets (`TeamPane`, `TeamDetail`, `ShareDialog`) redraw
    /// without a separate cache subscription channel.
    TeamSnapshot(crate::team::TeamSnapshot),
    /// renderer's team-color-bar prefix.
    CurrentTeamChanged {
        team_id: Option<uuid::Uuid>,
        color: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct WelcomeInfo {
    pub version: String,
    pub agents: Vec<AgentInfo>,
    pub seal_chain_length: u64,
    pub last_seal_hash: Option<String>,
}

pub trait ShellRenderer: Send + 'static {
    fn publish(&mut self, event: RenderEvent);
    fn read_line(&mut self, ctx: &PromptContext) -> Option<String>;

    /// Report the exit code of the command that just finished, so a
    /// block-oriented renderer can mark its success/failure. The default
    /// is a no-op — line-by-line renderers (shell mode, stdout) don't
    /// need it; only the TUI's card view overrides this.
    fn note_exit(&mut self, _exit_code: i32) {}

    /// Permanently silence ALL output from this renderer. Used by a detached
    /// auto-foreground PTY relay owns the runtime's controlling terminal, any
    /// renderer byte — on stdout OR stderr, which share the one captured PTY —
    /// would interleave with the agent's live TUI. Default is a no-op; only the
    /// non-interactive `StdoutRenderer` (the renderer a detached runtime uses)
    /// overrides it. Irreversible by design: the relay runs for the rest of the
    /// runtime's life.
    fn mute(&mut self) {}

    /// Show a blocking selection prompt (`title` + `detail` + a numbered
    /// option list) and return the chosen 0-based index, or `None` if
    /// cancelled. `default` is the initially-highlighted option. A
    /// TUI renderer should override this with an arrow-key menu
    /// (↑/↓ to move, Enter to confirm). The default impl is a line-based
    /// fallback: it prints the menu and reads one line from stdin —
    /// Enter/`y`/`1` picks the default, a digit picks that option,
    /// anything else cancels.
    fn select_prompt(
        &mut self,
        title: &str,
        detail: &str,
        options: &[&str],
        default: usize,
    ) -> Option<usize> {
        // Fail-closed in a detached runtime. A daemon-hosted agent runs inside
        // an `orkia -c "@agent &"` process that re-enters this dispatch path but
        // has NO interactive stdin — `ORKIA_DETACHED_JOB_ID` marks it. Blocking
        // on `stdin.read_line` here would hang the runtime forever (the
        // documented landmine). Per non-negotiable #8 (fail-closed by default),
        // an unanswerable modal in a detached runtime denies (returns `None`),
        // never blocks. The TUI renderer (the only interactive surface) overrides
        // this method, so this only ever fires on the non-interactive fallback.
        if std::env::var_os("ORKIA_DETACHED_JOB_ID").is_some() {
            tracing::warn!(
                title,
                "select_prompt: detached runtime has no stdin — failing closed (deny)"
            );
            return None;
        }
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{title}");
        if !detail.is_empty() {
            let _ = writeln!(out, "  {detail}");
        }
        for (i, opt) in options.iter().enumerate() {
            let _ = writeln!(out, "  {}. {opt}", i + 1);
        }
        let _ = write!(out, "> ");
        let _ = out.flush();
        drop(out);
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).ok()? == 0 {
            return None;
        }
        let t = line.trim().to_ascii_lowercase();
        if t.is_empty() || t == "y" || t == "yes" {
            return Some(default.min(options.len().saturating_sub(1)));
        }
        t.parse::<usize>()
            .ok()
            .filter(|n| *n >= 1 && *n <= options.len())
            .map(|n| n - 1)
    }

    /// Print a one-shot toast above the live prompt line WITHOUT
    /// disturbing whatever the user is currently typing. Used for
    /// async detector events (agent prompts, injections, dropped
    /// bodies) that need to surface in real time. The default impl
    /// writes to stderr — fine when no line editor is active or as a
    /// fallback; renderers backed by a line editor (rustyline) should
    /// override to use the editor's `ExternalPrinter` so the prompt
    /// redraws cleanly.
    fn external_print(&mut self, line: &str) {
        use std::io::Write;
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{line}");
        let _ = err.flush();
    }

    /// Hand the caller an `mpsc::Sender<String>` that prints to the
    /// terminal from a background thread without disturbing the
    /// prompt. Pairing with a worker thread that consumes
    /// `DetectorEvent` from `terminal_state` allows truly real-time
    /// toasts even while `read_line` is blocking. Returning `None`
    /// means the renderer has no equivalent of `ExternalPrinter`;
    /// the caller falls back to between-prompt `external_print`.
    fn take_external_print_sender(&mut self) -> Option<std::sync::mpsc::Sender<String>> {
        None
    }

    /// Called before the REPL grabs raw stdin/stdout for a foreground PTY job
    /// (shell command or `$EDITOR`). TUI renderers should leave the alternate
    /// screen and disable raw mode so the child has a clean terminal.
    fn yield_terminal(&mut self) {}

    /// Called after the foreground job exits or detaches. TUI renderers should
    /// re-enter the alternate screen and redraw.
    fn reclaim_terminal(&mut self) {}

    /// Whether this renderer supports the widget-mode attach flow (PTY rendered
    /// inside the ratatui buffer, sidebar still visible). `false` means the
    /// REPL should fall back to the tmux-style yield/reclaim path.
    fn is_attach_capable(&self) -> bool {
        false
    }

    /// Hand the renderer an attached-job handle. The renderer keeps it until
    /// `drive_attached` returns. No-op for non-capable renderers.
    fn attach_job(&mut self, _handle: AttachedHandle) {}

    /// Run the attached-mode event loop: poll input, route keys to the PTY,
    /// redraw on engine wake or input, return when the user detaches (Ctrl-Z)
    /// or the child exits. Pushes a SystemInfo block on exit.
    fn drive_attached(&mut self) -> AttachedOutcome {
        AttachedOutcome::Unsupported
    }

    /// Install rustyline tab-completion / inline-hint support. The
    /// concrete helper type lives in `orkia-shell` (which depends on
    /// this crate); pass it as `Box<dyn Any>` to avoid a circular dep.
    /// Renderers that don't use rustyline ignore the call. Returns
    /// `Some(box)` if the helper was not consumed so the caller can
    /// drop it explicitly.
    fn install_completion_helper(
        &mut self,
        helper: Box<dyn std::any::Any + Send>,
    ) -> Option<Box<dyn std::any::Any + Send>> {
        Some(helper)
    }

    /// Returns the helper-shared state box (an `Arc<ArcSwap<HelperShared>>`
    /// from `orkia-shell`) wrapped as `Any`, if this renderer hosts one.
    /// Allows the REPL to push fresh agent / history snapshots without
    /// rebuilding the helper.
    fn completion_shared(&self) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        None
    }
}

#[cfg(test)]
mod detached_fail_closed_tests {
    use super::*;

    /// Serializes the process-wide `ORKIA_DETACHED_JOB_ID` mutation so this
    /// test never races other env-touching tests in the binary.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Bare renderer that implements only the two required methods, so
    /// `select_prompt` resolves to the trait default under test.
    struct BareRenderer;
    impl ShellRenderer for BareRenderer {
        fn publish(&mut self, _event: RenderEvent) {}
        fn read_line(&mut self, _ctx: &PromptContext) -> Option<String> {
            None
        }
    }

    #[test]
    fn select_prompt_fails_closed_in_detached_runtime() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var_os("ORKIA_DETACHED_JOB_ID");

        // SAFETY: process-wide env mutation serialized on `ENV_LOCK`.
        unsafe {
            std::env::set_var("ORKIA_DETACHED_JOB_ID", "7");
        }
        // Must return `None` (deny) WITHOUT touching stdin — if it blocked on
        // `read_line` this test would hang forever, which is the failure we
        // are guarding against.
        let choice = BareRenderer.select_prompt(
            "Trust this directory?",
            "detail",
            &["Yes, trust this folder", "No, cancel"],
            0,
        );
        assert_eq!(choice, None);

        // SAFETY: restore prior state under the same guard.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("ORKIA_DETACHED_JOB_ID", v),
                None => std::env::remove_var("ORKIA_DETACHED_JOB_ID"),
            }
        }
    }
}
