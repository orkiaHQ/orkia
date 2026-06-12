// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

pub mod config;
pub mod entry;
pub mod foreground_relay;
pub mod forge_entry;
pub mod lifecycle;
pub mod native_entry;
pub mod raw_attach;
pub mod raw_termios;
pub mod spawn;
mod util;

pub use config::{Attachment, CageWrapper, JobConfig};
pub use entry::{SinkRecipe, SinkTarget};
pub use lifecycle::{JobLifecycleHook, SpawnContext};

/// Back-compat alias so the REPL and binary code that still spells
/// `foreground::run_foreground` / `foreground::attached_pid` /
/// `foreground::request_detach` keeps compiling after the
/// crossterm-based pump was replaced by the raw splice pump.
pub mod foreground {
    pub use super::raw_attach::{attached_pid, is_detach, request_detach, run_foreground};
}

pub use orkia_shell_types::job::{JobEvent, JobId, JobInfo, JobKind, JobOwner, JobState};

use orkia_terminal_core::{EngineConfig, TerminalEngine};
use tokio::sync::mpsc;

use crate::error::ShellError;
use entry::JobEntry;
use forge_entry::ForgeJobEntry;
use native_entry::NativeJobEntry;
use util::{bg_job_cap, terminal_dims};

/// Output of a successful spawn: the job id plus the hashes the SEAL
/// emitter needs to enrich the spawn record. Empty hashes when the
/// agent has no filesystem definition.
pub struct SpawnResult {
    pub job_id: JobId,
    pub system_prompt_hash: String,
    pub memory_hash: String,
    pub tools_count: usize,
}

pub struct JobController {
    jobs: Vec<JobEntry>,
    /// Bare (non-PTY) child processes — currently only the Forge viewer.
    /// Tracked separately because `JobEntry` requires a `TerminalEngine`,
    /// which is the wrong shape for a GUI process. The shared `next_id`
    /// keeps `kill <N>` / `ps` working as a single unified namespace.
    forge_jobs: Vec<ForgeJobEntry>,
    /// Native (non-PTY) agent sessions — in-process actors, no child
    /// process at all. Same shared-`next_id` rationale as `forge_jobs`.
    native_jobs: Vec<NativeJobEntry>,
    next_id: u32,
    event_tx: mpsc::UnboundedSender<JobEvent>,
}

/// Inputs for [`JobController::register_native`] (config struct — the
/// 4-arg rule).
pub struct NativeRegistration {
    pub agent_name: String,
    pub agent_id: uuid::Uuid,
    pub inbound: mpsc::UnboundedSender<crate::native::NativeSessionMsg>,
    pub exit_rx: tokio::sync::oneshot::Receiver<i32>,
}

impl JobController {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<JobEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                jobs: Vec::new(),
                forge_jobs: Vec::new(),
                native_jobs: Vec::new(),
                next_id: 1,
                event_tx: tx,
            },
            rx,
        )
    }

    /// Register a freshly-spawned Forge viewer process under
    /// [`JobKind::ForgeApp`]. The child must already be running; this
    /// method only takes ownership and assigns a job id.
    pub fn register_forge_app(
        &mut self,
        app_name: String,
        child: std::process::Child,
    ) -> Result<JobId, ShellError> {
        let id = self.alloc_id()?;
        let label = format!("app:{app_name}");
        let entry = ForgeJobEntry {
            id,
            app_name: app_name.clone(),
            child,
            started_at: std::time::Instant::now(),
            state: JobState::Running,
            label: label.clone(),
        };
        let pid = entry.pid();
        self.forge_jobs.push(entry);
        let _ = self.event_tx.send(JobEvent::Spawned {
            id,
            kind: JobKind::ForgeApp { app_name },
            pid,
        });
        Ok(id)
    }

    /// Register a freshly-spawned native (non-PTY) agent session under
    /// [`JobKind::Agent`]. The session actor must already be running;
    /// this method only takes ownership of its control handles and
    /// assigns a job id.
    pub fn register_native(&mut self, reg: NativeRegistration) -> Result<JobId, ShellError> {
        let id = self.alloc_id()?;
        let label = format!("{} (native)", reg.agent_name);
        self.native_jobs.push(NativeJobEntry {
            id,
            agent_name: reg.agent_name.clone(),
            agent_id: reg.agent_id,
            inbound: reg.inbound,
            exit_rx: reg.exit_rx,
            started_at: std::time::Instant::now(),
            state: JobState::Running,
            label,
        });
        let _ = self.event_tx.send(JobEvent::Spawned {
            id,
            kind: JobKind::Agent {
                agent_id: reg.agent_id,
                agent_name: reg.agent_name,
            },
            pid: None,
        });
        Ok(id)
    }

    /// Most recent still-running native session for `name` — the
    /// native analog of [`Self::find_live_agent_by_name`].
    pub fn find_live_native_by_name(&self, name: &str) -> Option<JobId> {
        self.native_jobs
            .iter()
            .rev()
            .find(|j| j.agent_name == name && matches!(j.state, JobState::Running))
            .map(|j| j.id)
    }

    /// Control-channel sender for a live native session, used by
    /// `tell` and dispatch reuse. `None` for non-native ids.
    pub fn native_inbound(
        &self,
        id: JobId,
    ) -> Option<&mpsc::UnboundedSender<crate::native::NativeSessionMsg>> {
        self.native_jobs
            .iter()
            .find(|j| j.id == id)
            .map(|j| &j.inbound)
    }

    /// Display label of a native job, used by the `Completed` drain to
    /// attribute the synthesized `SessionEnd` (a native id misses
    /// [`Self::get`], which only sees PTY entries).
    pub fn native_label(&self, id: JobId) -> Option<String> {
        self.native_jobs
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.label.clone())
    }

    /// Per-instance UUID of an agent job, PTY or native. `None` for
    /// shell/forge jobs and unknown ids. Used by the public-job
    /// emission paths, which must work for both agent shapes.
    pub fn agent_uuid(&self, id: JobId) -> Option<uuid::Uuid> {
        if let Some(entry) = self.get(id) {
            return match &entry.kind {
                JobKind::Agent { agent_id, .. } => Some(*agent_id),
                _ => None,
            };
        }
        self.native_jobs
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.agent_id)
    }

    /// Spawn a background shell job (`cmd &`). Parallel to
    /// `spawn_agent` minus all the agent-specific attachments
    /// (hooks, agent_context, OSC 133 / APC listeners, initial
    /// prompt). Returns the new job id.
    ///
    /// `argv` must be the already-expanded command-line — caller is
    /// responsible for running it through `BrushSession::expand_to_argv`
    /// first so brush's `$VAR` / glob / tilde semantics apply.
    ///
    /// The unified [`Self::spawn`] now exists; future BG-shell
    /// this entrypoint to a `JobConfig` builder + `spawn` call.
    /// Kept as-is here because BG shell jobs are not yet wired in
    /// the REPL and the existing callers (and their log-file tee)
    /// would otherwise need to be touched.
    pub fn spawn_shell(
        &mut self,
        argv: &[String],
        env: Vec<(String, String)>,
        cwd: Option<std::path::PathBuf>,
        label: String,
        data_dir: &std::path::Path,
    ) -> Result<JobId, ShellError> {
        let cmd = argv
            .first()
            .ok_or_else(|| ShellError::Other("spawn_shell: empty argv".into()))?
            .clone();
        // Each background spawn opens
        // ~3 fds (PTY master + slave + log file); leaving room for
        // the REPL itself, agent jobs, journal socket, etc., we
        // refuse new shell-job spawns once the count would cross
        // `RLIMIT_NOFILE / 4`.
        let live_shell_jobs = self
            .jobs
            .iter()
            .filter(|j| {
                matches!(j.kind, JobKind::Shell { .. })
                    && matches!(
                        j.state,
                        JobState::Running | JobState::Foreground | JobState::Stopped
                    )
            })
            .count();
        if let Some(cap) = bg_job_cap()
            && live_shell_jobs >= cap
        {
            return Err(ShellError::Other(format!(
                "too many background jobs ({live_shell_jobs} ≥ {cap}); \
                 wait/kill some before spawning more"
            )));
        }
        let args: Vec<String> = argv.iter().skip(1).cloned().collect();
        let id = self.alloc_id()?;
        let (cols, rows) = terminal_dims();

        // Mark the child as orkia-spawned so its own hooks /
        // protocol emissions can be attributed back to this REPL.
        // No `ORKIA_JOB_ID` for shell jobs — the bridge filters
        // toasts on `job_id` presence, and a shell job's tool calls
        // (if any) are not orkia-tracked.
        let mut env = env;
        env.push(("TERM".into(), "xterm-256color".into()));

        let config = EngineConfig {
            init_cols: cols,
            init_rows: rows,
            cmd: Some(cmd.clone()),
            args,
            env,
            cwd,
            on_osc133: None,
            on_apc: None,
            ..EngineConfig::default()
        };

        let engine = TerminalEngine::start(config)
            .map_err(|e| ShellError::Other(format!("spawn_shell: {e}")))?;
        let pid = engine.child_id();
        let kind = JobKind::Shell { cmd };

        // Tee the PTY's output to a per-job log file at
        // `<data_dir>/jobs/<id>/output.log` so the user can inspect
        // what a backgrounded command produced even after it has
        // closed. A future `log` builtin reads from here; for now
        // it's a debug-only side channel. Best-effort: creation /
        // write errors are logged but never fail the spawn.
        let log_path = data_dir
            .join("jobs")
            .join(id.to_string())
            .join("output.log");
        let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new(".")));
        let log_rx = engine.subscribe_output();
        if let Err(err) = std::thread::Builder::new()
            .name(format!("orkia-bg-log-{}", id.0))
            .spawn(move || {
                use std::io::Write;
                let mut file = match std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&log_path)
                {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(
                            path = %log_path.display(),
                            error = %e,
                            "bg job log: open failed",
                        );
                        return;
                    }
                };
                while let Ok(chunk) = log_rx.recv() {
                    if let Err(e) = file.write_all(&chunk) {
                        tracing::warn!(
                            path = %log_path.display(),
                            error = %e,
                            "bg job log: write failed",
                        );
                        break;
                    }
                }
            })
        {
            // Background log is a debug aid; if the OS refuses the
            // thread we lose the log but the job itself proceeds.
            tracing::warn!(
                job = id.0,
                ?err,
                "bg job log writer spawn failed; log file will be empty",
            );
        }

        let _ = self.event_tx.send(JobEvent::Spawned {
            id,
            kind: kind.clone(),
            pid,
        });

        self.jobs.push(JobEntry {
            id,
            kind,
            state: JobState::Running,
            engine,
            started_at: std::time::Instant::now(),
            label,
            lifecycle_hooks: Vec::new(),
            sink_recipe: None,
        });

        Ok(id)
    }

    pub fn get(&self, id: JobId) -> Option<&JobEntry> {
        self.jobs.iter().find(|j| j.id == id)
    }

    pub fn get_mut(&mut self, id: JobId) -> Option<&mut JobEntry> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    /// Find the most recent still-running agent job whose
    /// `JobKind::Agent { agent_name }` matches `name`. Used by the
    /// REPL to deliver a follow-up prompt to an already-spawned
    /// agent instead of starting a fresh session.
    pub fn find_live_agent_by_name(&self, name: &str) -> Option<JobId> {
        self.jobs
            .iter()
            .rev()
            .filter(|j| {
                matches!(
                    j.state,
                    JobState::Running | JobState::Foreground | JobState::Stopped
                )
            })
            .find_map(|j| match &j.kind {
                JobKind::Agent { agent_name, .. } if agent_name == name => Some(j.id),
                _ => None,
            })
    }

    pub fn list(&mut self) -> Vec<JobInfo> {
        self.reap();
        let mut out: Vec<JobInfo> = self
            .jobs
            .iter()
            .map(|j| JobInfo {
                id: j.id,
                kind: j.kind.clone(),
                state: j.state.clone(),
                label: j.label.clone(),
                pid: j.pid(),
                runtime: j.started_at.elapsed(),
                sink: j.sink_recipe.as_ref().map(|r| match &r.target {
                    entry::SinkTarget::Command { sink_cmd, .. } => sink_cmd.clone(),
                    entry::SinkTarget::Terminal => "<terminal>".to_string(),
                }),
            })
            .collect();
        out.extend(self.forge_jobs.iter().map(|f| JobInfo {
            id: f.id,
            kind: JobKind::ForgeApp {
                app_name: f.app_name.clone(),
            },
            state: f.state.clone(),
            label: f.label.clone(),
            pid: f.pid(),
            runtime: f.started_at.elapsed(),
            sink: None,
        }));
        out.extend(self.native_jobs.iter().map(|n| JobInfo {
            id: n.id,
            kind: JobKind::Agent {
                agent_id: n.agent_id,
                agent_name: n.agent_name.clone(),
            },
            state: n.state.clone(),
            label: n.label.clone(),
            // No process: a native session is an in-process actor.
            pid: None,
            runtime: n.started_at.elapsed(),
            sink: None,
        }));
        out.sort_by_key(|j| j.id.0);
        out
    }

    pub fn stop(&mut self, id: JobId) -> Result<(), ShellError> {
        if let Some(native) = self.native_jobs.iter_mut().find(|n| n.id == id) {
            // Kill, not SIGTERM — there is no process. The actor drops
            // its in-flight turn and fires the exit oneshot; `reap`
            // emits the terminal `Completed`.
            let _ = native.inbound.send(crate::native::NativeSessionMsg::Kill);
            native.state = JobState::Stopped;
            let label = native.label.clone();
            let _ = self.event_tx.send(JobEvent::Stopped { id, label });
            return Ok(());
        }
        if let Some(forge) = self.forge_jobs.iter_mut().find(|f| f.id == id) {
            forge.signal(libc::SIGTERM)?;
            forge.state = JobState::Stopped;
            let label = forge.label.clone();
            let _ = self.event_tx.send(JobEvent::Stopped { id, label });
            return Ok(());
        }
        let entry = self
            .get_mut(id)
            .ok_or_else(|| ShellError::Other(format!("job {id} not found")))?;
        entry.signal(libc::SIGTERM)?;
        entry.state = JobState::Stopped;
        let label = entry.label.clone();
        let _ = self.event_tx.send(JobEvent::Stopped { id, label });
        Ok(())
    }

    /// Mark a job complete with an explicit exit code and emit `Completed`.
    /// Used by the one-shot `-c` teardown: a delivered turn is the
    /// authoritative completion signal, so the terminal event is emitted
    /// directly instead of depending on the PTY-exit reap of the just-SIGTERM'd
    /// agent — that reap races the engine reader's `try_wait` and can strand the
    /// entry in `Stopped`, so a detached runtime would forward only `Stopped`
    /// and the main REPL would never render `[1] done`. Idempotent: a job
    /// already terminal is left untouched so the later reap cannot double-emit.
    pub fn complete(&mut self, id: JobId, exit_code: i32) {
        let Some(entry) = self.get_mut(id) else {
            return;
        };
        if matches!(entry.state, JobState::Done { .. } | JobState::Failed { .. }) {
            return;
        }
        entry.state = JobState::Done { exit_code };
        let label = entry.label.clone();
        let _ = self.event_tx.send(JobEvent::Completed {
            id,
            exit_code,
            label,
        });
    }

    pub fn bg(&mut self, id: JobId) -> Result<(), ShellError> {
        if let Some(forge) = self.forge_jobs.iter_mut().find(|f| f.id == id) {
            if forge.state == JobState::Stopped {
                forge.signal(libc::SIGCONT)?;
                forge.state = JobState::Running;
                let label = forge.label.clone();
                let _ = self.event_tx.send(JobEvent::Continued { id, label });
            }
            return Ok(());
        }
        let entry = self
            .get_mut(id)
            .ok_or_else(|| ShellError::Other(format!("job {id} not found")))?;
        if entry.state == JobState::Stopped {
            entry.signal(libc::SIGCONT)?;
            entry.state = JobState::Running;
            let label = entry.label.clone();
            let _ = self.event_tx.send(JobEvent::Continued { id, label });
        }
        Ok(())
    }

    /// `disown` — remove a job from the controller without
    /// killing it. The child keeps running; orkia exit no longer
    /// kills it (portable-pty's `setsid` at spawn put the child
    /// in its own session, so the parent's SIGHUP cascade does
    /// not reach it). Returns the dropped JobEntry so its PTY
    /// master can be dropped — that closes the fd, but the child
    /// already has the slave open and continues unaffected.
    pub fn disown(&mut self, id: JobId) -> Result<(), ShellError> {
        if let Some(pos) = self.forge_jobs.iter().position(|f| f.id == id) {
            // Drop the child handle. The viewer process keeps running
            // (we never asked the OS to reparent it; on macOS/Linux the
            // child is in its own group from spawn time).
            self.forge_jobs.remove(pos);
            let _ = self.event_tx.send(JobEvent::Detached { id });
            return Ok(());
        }
        let pos = self
            .jobs
            .iter()
            .position(|j| j.id == id)
            .ok_or_else(|| ShellError::Other(format!("job {id} not found")))?;
        // Pop and drop. Engine.drop closes master fd; child keeps
        // its slave open and lives on with its own session leader.
        self.jobs.remove(pos);
        let _ = self.event_tx.send(JobEvent::Detached { id });
        Ok(())
    }

    pub fn reap(&mut self) {
        for entry in &mut self.jobs {
            if matches!(
                entry.state,
                JobState::Running | JobState::Foreground | JobState::Stopped
            ) && let Some(code) = entry.try_exit_code()
            {
                entry.state = JobState::Done { exit_code: code };
                let _ = self.event_tx.send(JobEvent::Completed {
                    id: entry.id,
                    exit_code: code,
                    label: entry.label.clone(),
                });
            }
        }
        for entry in &mut self.forge_jobs {
            if matches!(entry.state, JobState::Running | JobState::Stopped)
                && let Some(code) = entry.try_exit_code()
            {
                entry.state = JobState::Done { exit_code: code };
                let _ = self.event_tx.send(JobEvent::Completed {
                    id: entry.id,
                    exit_code: code,
                    label: entry.label.clone(),
                });
            }
        }
        for entry in &mut self.native_jobs {
            if matches!(entry.state, JobState::Running | JobState::Stopped)
                && let Some(code) = entry.try_exit_code()
            {
                entry.state = JobState::Done { exit_code: code };
                let _ = self.event_tx.send(JobEvent::Completed {
                    id: entry.id,
                    exit_code: code,
                    label: entry.label.clone(),
                });
            }
        }
        self.jobs
            .retain(|j| !matches!(j.state, JobState::Done { .. } | JobState::Failed { .. }));
        self.forge_jobs
            .retain(|f| !matches!(f.state, JobState::Done { .. } | JobState::Failed { .. }));
        self.native_jobs
            .retain(|n| !matches!(n.state, JobState::Done { .. } | JobState::Failed { .. }));
        // JobIds are strictly monotonic — never recycled. Bash recycles
        // freed numbers to keep them small, but here the teardown is
        // asynchronous: a job's `JobEvent::Completed { id }` is reaped
        // here yet drained later, where it runs `remove_job(id)` +
        // `injection_executor.unregister(id)`. If `id` were recycled to
        // a freshly-spawned agent in between, that stale completion
        // would rip out the NEW agent's queued body and PTY writer
        // (observed: `@faye say byee` dropped at WaitingForBoot right
        // after a Ctrl-C'd faye reaped). A unique id per instance makes
        // a late teardown un-aliasable — durability over a cosmetic
        // small number. See `job_ids_never_recycle_*` in lifecycle.rs.
    }

    pub fn event_tx(&self) -> &mpsc::UnboundedSender<JobEvent> {
        &self.event_tx
    }

    /// Write bytes into the given job's PTY master fd. Used for:
    ///   * initial prompt injection at spawn (`<intent>\n`)
    ///   * `tell N <message>` follow-ups to a running agent
    ///   * `y\n` / `n\n` keystrokes resolving hook-driven approvals
    ///
    /// Errors when the job is unknown.
    pub fn write_to_pty(&self, id: JobId, data: &[u8]) -> Result<(), ShellError> {
        let entry = self
            .get(id)
            .ok_or_else(|| ShellError::Other(format!("job {id} not found")))?;
        entry.write_stdin(data)
    }

    /// Allocate the next strictly-monotonic [`JobId`]. Ids are never recycled
    /// (lifecycle invariant): a late `Completed{id}` aliasing a reused id would
    /// tear down a new agent's writer/queue. On u32 exhaustion we refuse to
    /// spawn rather than wrap to 0 (BUG-095).
    pub(crate) fn alloc_id(&mut self) -> Result<JobId, ShellError> {
        let id = JobId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(ShellError::JobIdExhausted)?;
        Ok(id)
    }

    /// Push a freshly-constructed [`JobEntry`] onto the active
    /// list. Used by [`spawn::spawn`] after the engine is up.
    pub(crate) fn push_entry(&mut self, entry: JobEntry) {
        self.jobs.push(entry);
    }

    /// Return the most-recently-pushed entry, used by [`spawn::spawn`]
    /// to write initial bytes to the freshly-spawned PTY.
    pub(crate) fn last_entry(&self) -> Option<&JobEntry> {
        self.jobs.last()
    }

    /// Fire each lifecycle hook's `on_complete` for `job_id`.
    /// Called from the REPL's `drain_job_events` `Completed` arm
    /// in place of the imperative per-subsystem teardown
    /// (`injection_executor.unregister`, `state_machine.remove_job`)
    /// that used to live inline. Returns silently when the job
    /// is no longer registered (already reaped).
    pub fn dispatch_on_complete(&self, job_id: JobId, exit_code: i32) {
        let Some(entry) = self.get(job_id) else {
            return;
        };
        // Clone the Arc list out so iteration doesn't hold a
        // borrow on `self` while a hook does its work — hooks
        // may call back through cheap-clone handles which is
        // fine, but a re-entrant `&mut JobController` would not
        // be (none today, but defend against future drift).
        let hooks = entry.lifecycle_hooks.clone();
        for hook in &hooks {
            hook.on_complete(job_id, exit_code);
        }
    }

    /// agent dispatch and any future plain-shell or container spawn
    /// delegate to this single path. See [`spawn::SpawnDeps`] for the
    /// surrounding-subsystem handles the controller needs.
    pub fn spawn(
        &mut self,
        config: JobConfig<'_>,
        deps: spawn::SpawnDeps<'_>,
    ) -> Result<SpawnResult, ShellError> {
        spawn::spawn(self, config, deps)
    }
}
