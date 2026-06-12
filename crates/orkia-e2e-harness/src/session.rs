// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! `OrkiaSession` — unified driver API.
//!
//! After Part B/C wiring + F001 prep:
//!   * `start_compose` boots the backend pool AND a PTY-hosted orkia
//!     shell (if the `orkia` binary is resolvable).
//!   * `reset_backend` calls the server-side `reset_e2e_test_state()`.
//!   * `type_line` / `wait_for` drive the PTY.
//!   * The shell side is `Option<ShellSession>` so backend-only smoke
//!     paths (orkia-check 0-flow runs) still work without the binary.

use std::time::Duration;

use std::path::PathBuf;

use orkia_test_harness::{JournalTail, OrkiaProcess, OrkiaSandbox};
use sqlx::PgPool;

use crate::assert::{BackendAssert, FileAssert, JournalAssert, OutputAssert};
use crate::boot::{crontab_spool_path, restore_default_faye_script, try_start_shell};
use crate::error::HarnessError;
use crate::mode::Mode;

/// Internal construction payload — produced by mode booters.
pub struct SessionInner {
    pub backend_url: String,
    pub db_pool: PgPool,
}

/// Output captured from a single command run inside the shell.
#[derive(Debug, Clone, Default)]
pub struct RenderedOutput {
    pub raw: String,
    pub stripped: String,
    pub lines: Vec<String>,
    pub exit_status: Option<i32>,
    pub duration: Duration,
}

impl RenderedOutput {
    pub fn contains(&self, needle: &str) -> bool {
        self.stripped.contains(needle)
    }
}

/// PTY-hosted orkia shell bound to a hermetic ORKIA_HOME.
pub struct ShellSession {
    pub sandbox: OrkiaSandbox,
    pub process: OrkiaProcess,
    pub journal: JournalTail,
    /// Cached `sandbox.data_dir()` — that method returns by value, so
    /// we materialize it once for cheap borrowing from `files()`.
    pub data_dir: PathBuf,
}

/// Unified handle on a booted Orkia shell + backend.
pub struct OrkiaSession {
    mode: Mode,
    backend_url: String,
    db_pool: PgPool,
    shell: Option<ShellSession>,
    last_output: RenderedOutput,
    /// Offset into `JournalTail::all()` that journal assertions treat
    /// as the "start" of the current flow. Bumped by
    /// [`Self::reset_for_next_flow`] so a flow only sees envelopes
    /// emitted after it began, even though all flows share one
    /// JournalTail instance (orkia-test-harness has no truncate API).
    journal_cursor: usize,
}

impl OrkiaSession {
    pub async fn start_local() -> crate::Result<Self> {
        crate::mode::local::start_local().await
    }

    pub async fn start_compose() -> crate::Result<Self> {
        Self::start_compose_with_env(crate::env::FlowEnv::free()).await
    }

    /// Boot a compose session under a specific [`FlowEnv`]. The env's plan
    /// selects the fixture account the harness logs in as (real backend
    /// login → signed JWT); everything else is identical to
    /// [`Self::start_compose`].
    pub async fn start_compose_with_env(env: crate::env::FlowEnv) -> crate::Result<Self> {
        crate::mode::compose::start_compose_with_env(env).await
    }

    /// Rebrand a compose-booted session as Local. Used by
    /// [`crate::mode::local::start_local`] which currently reuses the
    pub(crate) fn override_mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    pub(crate) fn from_compose(inner: SessionInner, env: crate::env::FlowEnv) -> Self {
        let shell = try_start_shell(&env);
        Self {
            mode: Mode::Compose,
            backend_url: inner.backend_url,
            db_pool: inner.db_pool,
            shell,
            last_output: RenderedOutput::default(),
            journal_cursor: 0,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn backend_url(&self) -> &str {
        &self.backend_url
    }
    pub fn db_pool(&self) -> &PgPool {
        &self.db_pool
    }
    pub fn shell(&self) -> Option<&ShellSession> {
        self.shell.as_ref()
    }
    pub fn has_shell(&self) -> bool {
        self.shell.is_some()
    }

    /// Write raw bytes directly to the PTY master. Use this for control
    /// bytes that aren't normal input (Ctrl-Z = `\x1a` for attach detach,
    /// Ctrl-C = `\x03`, etc.). Bypasses the per-character drip-feed in
    /// [`Self::type_line`].
    pub fn send_bytes(&mut self, bytes: &[u8]) -> crate::Result<()> {
        let s = self.shell_mut()?;
        s.process
            .pty
            .write(bytes)
            .map_err(|e| HarnessError::Infra(format!("pty.write: {e}")))
    }

    /// Type a line into the shell. Does NOT wait for completion — pair
    /// with [`Self::wait_for`] on a command-specific marker.
    pub async fn type_line(&mut self, line: &str) -> crate::Result<()> {
        let s = self.shell_mut()?;
        s.process
            .pty
            .type_line(line)
            .map_err(|e| HarnessError::Infra(format!("type_line: {e}")))?;
        Ok(())
    }

    /// Wait for `marker` to appear on the rendered screen, then refresh
    /// the cached [`RenderedOutput`] so output assertions see the
    /// latest state.
    pub async fn wait_for(&mut self, marker: &str, timeout: Duration) -> crate::Result<()> {
        let s = self.shell_mut()?;
        s.process
            .pty
            .wait_for_text(marker, timeout)
            .await
            .map_err(|e| HarnessError::Timeout(e.to_string()))?;
        self.refresh_output();
        Ok(())
    }

    /// Convenience: `type_line` + `wait_for` in a single call.
    ///
    /// OSC 133;D (command-end) so subsequent flow code that snapshots
    /// the 133;D count doesn't race against the in-flight command.
    /// Without the second wait, `count_command_end_marks` taken
    /// immediately after `run()` returns can miss this command's own
    /// 133;D — the next flow stage will then see it as a false positive
    /// for some later command finishing (S1 F103 wait-blocks bug).
    pub async fn run(&mut self, line: &str, marker: &str, timeout: Duration) -> crate::Result<()> {
        let pre_d_count = self.command_end_count();
        self.type_line(line).await?;
        self.wait_for(marker, timeout).await?;
        // Brief wait for the trailing 133;D. Some marker hits coincide
        // with the command's natural end (e.g. ps that prints "faye"
        // then OSC 133;D); some happen mid-output. 1.5s is plenty for
        // a builtin to flush.
        let d_deadline = std::time::Instant::now() + Duration::from_millis(1500);
        while self.command_end_count() <= pre_d_count {
            if std::time::Instant::now() >= d_deadline {
                // Don't fail the run — the marker matched, which is
                // the contract. Just log so anyone investigating sees
                // the partial state.
                tracing::debug!(
                    "run({line:?}, {marker:?}): marker hit but 133;D not seen in 1500ms"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok(())
    }

    /// Public count of OSC 133;D (command-end) marks in the raw PTY
    /// buffer. Used by flows that need to detect when a command — like
    /// `wait` — finishes without emitting any text output (S1 F103
    /// finding). Reading 133;A would false-trigger on every keystroke
    /// during prompt input.
    pub fn command_end_count(&self) -> usize {
        self.shell
            .as_ref()
            .map(|s| s.process.pty.raw_text().matches("\x1b]133;D").count())
            .unwrap_or(0)
    }

    /// Force orkia's `JobController::reap()` to run by issuing the
    /// `jobs` builtin (which internally calls `list()` → `reap()`).
    ///
    /// **Why this exists** (S1 retro finding #1): orkia's `reap()` —
    /// the path that emits `lifecycle:completed`/`lifecycle:stopped`
    /// envelopes when child processes exit — is ONLY called from
    /// `JobController::list()`. `list()` runs from `ps`, `wait`,
    /// `kill`, `fg`, `bg`, `disown`, etc. AND from `emit_jobs_snapshot`
    /// at the top of the REPL loop — but the REPL loop blocks in
    /// `read_line` between user inputs, so SIGCHLD events between
    /// prompts never trigger reap until the user types something.
    /// Orkia ships a SIGCHLD waker (repl.rs:4233) that nudges the
    /// JobEvent channel, but the nudge doesn't interrupt `read_line`.
    ///
    /// Consequence for flows: any assertion that depends on a
    /// `lifecycle:completed` or `lifecycle:stopped` envelope for a
    /// child that exits between user inputs MUST be preceded by
    /// either a `wait <name>` (polls list) or this `force_reap()`.
    ///
    /// Filed as TODO in the harness; the proper fix in orkia-shell
    /// is to make `read_line` interruptible by SIGCHLD (rustyline
    /// supports signal-driven refresh via `ExternalPrinter` but the
    /// reap call also needs to happen — needs a dedicated wakeup
    /// channel into the main loop).
    pub async fn force_reap(&mut self) -> crate::Result<()> {
        // `jobs` is a builtin that always returns quickly and emits
        // a 133;D. We wait for 133;D since the output text varies
        // (could be empty if no jobs, or a table if some are alive).
        let pre = self.command_end_count();
        self.type_line("jobs").await?;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while self.command_end_count() <= pre {
            if std::time::Instant::now() >= deadline {
                return Err(HarnessError::Timeout(
                    "force_reap: `jobs` builtin did not complete in 3s".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.refresh_output();
        Ok(())
    }

    pub async fn reset_backend(&self) -> crate::Result<()> {
        sqlx::query("SELECT reset_e2e_test_state()")
            .execute(&self.db_pool)
            .await?;
        Ok(())
    }

    /// Per-flow current journal cursor. Assertions returned by
    /// [`Self::journal`] only consider envelopes at this index or later.
    pub fn journal_cursor(&self) -> usize {
        self.journal_cursor
    }

    /// Bring the session back to a known baseline before the next flow
    /// runs. Called by `orkia-check`'s runner between flows so 30+ flows
    /// in a single suite stay isolated despite sharing one boot.
    ///
    /// Resets, in order:
    ///   1. **Backend DB** — `SELECT reset_e2e_test_state()` (preserves
    ///      seeded fixtures, deletes everything else scoped to the test
    ///      workspace).
    ///   2. **Journal cursor** — moves the assertion floor to the end of
    ///      the current event log. Earlier flows' lifecycle envelopes
    ///      are now invisible to assertions.
    ///   3. **Faye agent script** — restores the default
    ///      `<sandbox>/.orkia/agents/faye/script.yaml`. Flows that
    ///      rewrite it (F004 / F005) get a clean baseline.
    ///
    /// Best-effort: a failure on any individual step logs a warning but
    /// does not abort the run (we'd rather see a flaky-due-to-leak fail
    /// than a hard error that hides the root cause).
    pub async fn reset_for_next_flow(&mut self) -> crate::Result<()> {
        if let Err(e) = self.reset_backend().await {
            tracing::warn!("reset_for_next_flow: reset_backend failed: {e}");
        }
        if let Some(shell) = self.shell.as_ref() {
            self.journal_cursor = shell.journal.all().await.len();
            if let Err(e) = restore_default_faye_script(&shell.data_dir) {
                tracing::warn!("reset_for_next_flow: restore faye script failed: {e}");
            }
            // Truncate the crontab spool so a flow's `every` entries don't
            // leak into the next flow.
            let _ = std::fs::remove_file(crontab_spool_path(&shell.data_dir));
        }
        self.last_output = RenderedOutput::default();
        Ok(())
    }

    /// Seed a Forge app manifest at `<HOME>/.orkia/forge/<name>/manifest.toml`
    /// — the path `app inspect`/`list`/`perms` read (`default_app_root()`).
    pub fn seed_forge_app(&self, name: &str, manifest_toml: &str) -> crate::Result<()> {
        let shell = self
            .shell
            .as_ref()
            .ok_or_else(|| HarnessError::Infra("shell not booted".into()))?;
        let app_dir = shell.data_dir.join("forge").join(name);
        std::fs::create_dir_all(&app_dir)
            .map_err(|e| HarnessError::Infra(format!("seed_forge_app mkdir: {e}")))?;
        std::fs::write(app_dir.join("manifest.toml"), manifest_toml)
            .map_err(|e| HarnessError::Infra(format!("seed_forge_app write: {e}")))?;
        Ok(())
    }

    /// Seed a valid Forge App Provenance chain (ledger #3) for `name` using
    /// `orkia_forge_seal::SealWriter` (standalone: auto-generates the per-app
    /// signing key). Returns the events.jsonl path so F504 can tamper it.
    pub fn seed_forge_seal_chain(&self, name: &str) -> crate::Result<PathBuf> {
        let shell = self
            .shell
            .as_ref()
            .ok_or_else(|| HarnessError::Infra("shell not booted".into()))?;
        let seal_dir = shell.data_dir.join("forge").join(name).join("seal");
        let writer = orkia_forge_seal::SealWriter::open(&seal_dir)
            .map_err(|e| HarnessError::Infra(format!("SealWriter::open: {e}")))?;
        writer
            .append("test.event", serde_json::json!({ "marker": "alpha" }))
            .map_err(|e| HarnessError::Infra(format!("seal append 1: {e}")))?;
        writer
            .append("test.event", serde_json::json!({ "marker": "bravo" }))
            .map_err(|e| HarnessError::Infra(format!("seal append 2: {e}")))?;
        Ok(seal_dir.join("events.jsonl"))
    }

    pub async fn shutdown(mut self) -> crate::Result<()> {
        if let Some(mut s) = self.shell.take() {
            let _ = s.process.pty.kill();
        }
        self.db_pool.close().await;
        Ok(())
    }

    pub fn output(&self) -> OutputAssert<'_> {
        OutputAssert::new(&self.last_output)
    }
    pub fn backend(&self) -> BackendAssert<'_> {
        BackendAssert::new(&self.db_pool)
    }
    pub fn files(&self) -> FileAssert<'_> {
        match self.shell.as_ref() {
            // `sandbox.data_dir()` returns by value; stash it on the
            // session in the shell branch and lend it here.
            Some(s) => FileAssert::with_data_dir(&s.data_dir),
            None => FileAssert::detached(),
        }
    }
    pub fn journal(&self) -> JournalAssert<'_> {
        match self.shell.as_ref() {
            Some(s) => JournalAssert::with_tail(&s.journal, self.journal_cursor),
            None => JournalAssert::detached(),
        }
    }

    fn refresh_output(&mut self) {
        if let Some(s) = self.shell.as_ref() {
            let raw = s.process.pty.raw_text();
            // `stripped` is the full alacritty screen (last 24 lines).
            // S1 retro flagged this as flaky for `not_contains` because
            // `kill faye` leaves a `[1]+ Stopped faye` line on screen
            // that's caught by subsequent `not_contains("faye")`.
            // A windowed-to-last-command variant was attempted (via
            // OSC 133;C/D parsing) but broke `contains` in flows where
            // the prior 133;C was already in the buffer and the new
            // command's output hadn't fully populated yet.
            //
            // Workaround for now: callers should prefer positive
            // assertions (contains / has_line) over not_contains.
            // The full raw buffer is exposed via `last_output.raw`
            // for any caller that needs windowing logic of its own.
            let stripped = s.process.pty.screen_text();
            let lines = stripped.lines().map(|l| l.to_string()).collect();
            self.last_output = RenderedOutput {
                raw,
                stripped,
                lines,
                exit_status: None,
                duration: Duration::ZERO,
            };
        }
    }

    fn shell_mut(&mut self) -> crate::Result<&mut ShellSession> {
        self.shell.as_mut().ok_or(HarnessError::NotImplemented {
            what: "shell not booted (set ORKIA_TEST_BIN or build `orkia-cli`)",
        })
    }
}

impl OrkiaSession {
    /// Rewrite the script file for a pre-seeded agent. The agent's
    /// `agent.toml` (command + script path) is fixed at session boot
    /// because orkia loads agent definitions only at startup via
    /// `hydrate_agents_from_dir`. The script content can still change
    /// mid-flow because `orkia-fake-agent` re-reads `script.yaml` on
    /// every spawn.
    ///
    /// Valid agent names: those pre-seeded in `try_start_shell`. F101+
    /// uses `faye` and `sage`; add more there if needed by future flows.
    /// Returns an error if the agent directory doesn't exist (caller
    /// likely typo'd or named an unseeded agent).
    pub fn seed_agent_with_script(
        &self,
        name: &str,
        script: &orkia_test_harness::script::AgentScript,
    ) -> crate::Result<()> {
        let shell = self.shell.as_ref().ok_or(HarnessError::NotImplemented {
            what: "seed_agent_with_script: shell not booted",
        })?;
        let agent_dir = shell.data_dir.join("agents").join(name);
        if !agent_dir.exists() {
            return Err(HarnessError::Infra(format!(
                "agent '{name}' not pre-seeded at session boot. Add it to the loop in `try_start_shell`."
            )));
        }
        let yaml = script
            .to_yaml()
            .map_err(|e| HarnessError::Infra(format!("script to_yaml: {e}")))?;
        std::fs::write(agent_dir.join("script.yaml"), yaml)?;
        Ok(())
    }
}
