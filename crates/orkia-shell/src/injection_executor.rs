// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Dedicated executor for queued-prompt byte injection.
//!
//! Why it exists: the prompt detector decides "the agent is ready,
//! flush the queued body" on its own thread (see
//! `terminal_state::detector_loop`). Previously the actual PTY byte
//! write was done on the REPL main loop inside
//! `Repl::emit_injection`, which only runs when
//! `drain_state_machine_events` is called — between prompts, before a
//! tick, or right before `attach`. When the REPL was parked in
//! `read_line`, that drain never happened: the "▸ prompt injected"
//! toast fired immediately but the bytes sat queued for minutes,
//! arriving only when the user finally attached.
//!
//! This executor owns its own thread and a per-job [`SharedWriter`]
//! map, so detector-driven injections write straight to the PTY the
//! instant the detector decides — independent of REPL state. The
//! journal `Tell` envelope is still emitted on the REPL drain path
//! (it needs `&mut self`), but the user-visible byte arrival no
//! longer lags it.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use orkia_pty::SharedWriter;
use orkia_shell_types::JobId;
use parking_lot::Mutex;

use crate::terminal_state::DetectorEvent;

/// Renders the agent's current visible grid to ANSI bytes. The executor
/// uses it to confirm a typed body actually populated the agent's input
/// box before sending the submit (`\r`). Built from
/// [`orkia_terminal_core::TerminalEngine::grid_probe`].
pub type GridProbe = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;

/// Build a confirmation probe from a passive PTY output subscriber.
/// Codex's alacritty grid snapshot can be blank while its TUI composer
/// is visible in the byte stream; this keeps confirmation tied to the
/// same real TUI output the user sees, without reading screen text from
/// the PTY as an architectural state source.
pub fn output_transcript_probe(rx: mpsc::Receiver<Vec<u8>>) -> GridProbe {
    let transcript = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = Arc::clone(&transcript);
    thread::spawn(move || {
        while let Ok(chunk) = rx.recv() {
            let mut buf = writer.lock();
            buf.extend_from_slice(&chunk);
            let keep_from = buf.len().saturating_sub(64 * 1024);
            if keep_from > 0 {
                buf.drain(..keep_from);
            }
        }
    });
    Arc::new(move || transcript.lock().clone())
}

/// One inter-byte delay used both here and in the legacy
/// REPL-drain inject path. 5 ms is small enough to feel
/// instantaneous and large enough that Bun-based agents (claude)
/// run their stdin handler per char, the same as for a human typist.
const BYTE_GAP: Duration = Duration::from_millis(5);

/// How long, per attempt, to wait for a typed body to appear in the
/// agent's grid before re-typing. Long enough to absorb a slow boot
/// where the input box only renders several seconds in (claude buffers
/// early input and echoes it once the box exists), short enough that a
/// genuinely-lost body is re-typed promptly.
const CONFIRM_TIMEOUT: Duration = Duration::from_millis(6000);

/// Poll cadence while waiting for the body to show up in the grid.
const CONFIRM_POLL: Duration = Duration::from_millis(50);

/// Once the grid echoes the typed body, give the TUI event loop a full
/// turn before sending Submit. A rendered cell proves the bytes reached
/// the process, but Codex can update its input model later than its
/// render; a fast Enter is ignored while the text remains visible.
const SUBMIT_AFTER_CONFIRM_DELAY: Duration = Duration::from_millis(1000);

/// How many times to (re-)type the body, waiting for it to land in the
/// box each time. The detector can fire during a boot lull — before the
/// input box exists — so the first attempt may be typed into a not-yet-
/// ready agent and lost; a retype once the box is up lands it.
const MAX_ATTEMPTS: usize = 2;

/// Ctrl-U (kill-line) — wipes any partial/buffered input before a retype
/// so retries can't accumulate into a doubled prompt.
const CLEAR_LINE: u8 = 0x15;

/// How many boot modals (trust dialog, onboarding) to auto-accept
/// between spawn and the input box before giving up.
const MAX_MODALS: usize = 3;

/// Bracketed-paste markers (DEC mode 2004). An injected body is a
/// MESSAGE, never keystrokes: typed raw, the agent's TUI interprets a
/// leading `#` / `/` / `!` as a shortcut (claude opens its memory
/// dialog on `#`, swallowing the body) and treats `\n` as input
/// boundaries. Inside these markers the TUI inserts the content
/// literally into its input box — the same path a human paste takes.
const PASTE_BEGIN: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Commands the executor consumes. All async w.r.t. the caller — the
/// executor processes them on its own thread in FIFO order.
enum Command {
    Register {
        job_id: JobId,
        writer: SharedWriter,
        probe: Option<GridProbe>,
    },
    Unregister {
        job_id: JobId,
    },
    Inject {
        job_id: JobId,
        agent_name: String,
        body: String,
    },
    Hold {
        job_id: JobId,
    },
    Release {
        job_id: JobId,
    },
    /// Raw keystroke(s) to the agent PTY — no confirm, no trailing CR.
    /// Used to auto-answer a boot trust modal (Enter) off the REPL loop.
    SendKeys {
        job_id: JobId,
        bytes: Vec<u8>,
    },
}

/// Handle to the injection executor thread. Cheap to clone — wraps
/// a single mpsc sender. Dropping every clone causes the thread to
/// exit on next `recv`.
#[derive(Clone)]
pub struct InjectionExecutor {
    tx: Sender<Command>,
}

impl InjectionExecutor {
    /// Spawn the executor thread and return a handle. The thread is
    /// named `orkia-injection-exec` for log readability.
    ///
    /// Thread-spawn failure at REPL boot indicates the OS is refusing
    /// new threads (RLIMIT_NPROC / ENOMEM), which leaves the shell
    /// unable to dispatch any agent input — there is no useful
    /// recovery. We surface that as a logged panic rather than
    /// papering over it with a silently-broken executor.
    pub fn spawn() -> Self {
        Self::spawn_inner(None)
    }

    /// Like [`Self::spawn`] but wires a `DetectorEvent` sender so the
    /// executor emits [`DetectorEvent::Delivered`] the moment a body
    /// lands. The production REPL path uses this so the
    /// "▸ prompt injected" toast and journal `Tell` fire on the actual
    /// landing, not on the detector's earlier decision (which can lead
    /// the landing by several seconds on a slow agent boot).
    pub fn spawn_with_delivery(delivered_tx: Sender<DetectorEvent>) -> Self {
        Self::spawn_inner(Some(delivered_tx))
    }

    #[allow(clippy::expect_used)]
    fn spawn_inner(delivered_tx: Option<Sender<DetectorEvent>>) -> Self {
        let (tx, rx) = mpsc::channel::<Command>();
        thread::Builder::new()
            .name("orkia-injection-exec".into())
            .spawn(move || run_loop(rx, delivered_tx))
            .expect("orkia: failed to spawn injection executor thread (OS refused; shell cannot proceed)");
        Self { tx }
    }

    /// Associate `job_id` with the PTY writer obtained from the
    /// agent's `TerminalEngine` at spawn time, plus an optional grid
    /// probe used to confirm a typed body landed before submitting.
    /// `probe = None` keeps the legacy behaviour (type body + submit
    /// with no confirmation). Safe to call from any thread.
    pub fn register(&self, job_id: JobId, writer: SharedWriter, probe: Option<GridProbe>) {
        let _ = self.tx.send(Command::Register {
            job_id,
            writer,
            probe,
        });
    }

    /// Drop the writer for `job_id`. Called when a job exits so the
    /// executor stops holding the agent's PTY master alive.
    pub fn unregister(&self, job_id: JobId) {
        let _ = self.tx.send(Command::Unregister { job_id });
    }

    /// Queue a byte-by-byte injection of `body` (with a trailing
    /// `\r` for Enter) for `job_id`. Returns immediately; the write
    /// happens on the executor thread.
    pub fn inject(&self, job_id: JobId, agent_name: &str, body: &str) {
        let _ = self.tx.send(Command::Inject {
            job_id,
            agent_name: agent_name.to_string(),
            body: body.to_string(),
        });
    }

    pub fn hold(&self, job_id: JobId) {
        let _ = self.tx.send(Command::Hold { job_id });
    }

    pub fn release(&self, job_id: JobId) {
        let _ = self.tx.send(Command::Release { job_id });
    }

    /// Send raw `bytes` to the agent PTY (no confirm/no CR). Used to
    /// auto-answer a boot trust modal. Returns immediately.
    pub fn send_keys(&self, job_id: JobId, bytes: Vec<u8>) {
        let _ = self.tx.send(Command::SendKeys { job_id, bytes });
    }
}

fn run_loop(rx: mpsc::Receiver<Command>, delivered_tx: Option<Sender<DetectorEvent>>) {
    let mut writers: HashMap<JobId, SharedWriter> = HashMap::new();
    let mut probes: HashMap<JobId, GridProbe> = HashMap::new();
    let mut held: HashSet<JobId> = HashSet::new();
    let mut deferred: HashMap<JobId, Vec<DeferredInject>> = HashMap::new();
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Register {
                job_id,
                writer,
                probe,
            } => {
                writers.insert(job_id, writer);
                if let Some(p) = probe {
                    probes.insert(job_id, p);
                }
                tracing::debug!(job = job_id.0, "injection-exec: registered");
            }
            Command::Unregister { job_id } => {
                writers.remove(&job_id);
                probes.remove(&job_id);
                held.remove(&job_id);
                deferred.remove(&job_id);
                tracing::debug!(job = job_id.0, "injection-exec: unregistered");
            }
            Command::Inject {
                job_id,
                agent_name,
                body,
            } => {
                let inject = DeferredInject {
                    job_id,
                    agent_name,
                    body,
                };
                if held.contains(&job_id) {
                    deferred.entry(job_id).or_default().push(inject);
                    tracing::info!(job = job_id.0, "injection-exec: held, deferring prompt");
                } else {
                    deliver_deferred(inject, &writers, &probes, delivered_tx.as_ref());
                }
            }
            Command::Hold { job_id } => {
                held.insert(job_id);
                tracing::info!(job = job_id.0, "injection-exec: hold enabled");
            }
            Command::Release { job_id } => {
                held.remove(&job_id);
                let pending = deferred.remove(&job_id).unwrap_or_default();
                tracing::info!(
                    job = job_id.0,
                    count = pending.len(),
                    "injection-exec: hold released"
                );
                for inject in pending {
                    deliver_deferred(inject, &writers, &probes, delivered_tx.as_ref());
                }
            }
            Command::SendKeys { job_id, bytes } => match writers.get(&job_id).cloned() {
                Some(writer) => {
                    let _ = type_bytes(&writer, &bytes);
                    tracing::debug!(job = job_id.0, n = bytes.len(), "injection-exec: sent keys");
                }
                None => tracing::warn!(job = job_id.0, "injection-exec: send_keys no writer"),
            },
        }
    }
    tracing::debug!("injection-exec: channel closed, exiting");
}

struct DeferredInject {
    job_id: JobId,
    agent_name: String,
    body: String,
}

fn deliver_deferred(
    inject: DeferredInject,
    writers: &HashMap<JobId, SharedWriter>,
    probes: &HashMap<JobId, GridProbe>,
    delivered_tx: Option<&Sender<DetectorEvent>>,
) {
    match writers.get(&inject.job_id).cloned() {
        Some(writer) => do_inject(
            &InjectJob {
                job_id: inject.job_id,
                agent_name: &inject.agent_name,
                body: &inject.body,
                writer: &writer,
                delivered_tx,
            },
            probes.get(&inject.job_id),
        ),
        None => tracing::warn!(
            job = inject.job_id.0,
            agent = %inject.agent_name,
            "injection-exec: no writer registered (job exited?)",
        ),
    }
}

/// The pieces of one injection, bundled so the delivery helpers stay
/// under the argument limit.
struct InjectJob<'a> {
    job_id: JobId,
    agent_name: &'a str,
    body: &'a str,
    writer: &'a SharedWriter,
    /// Channel to announce a confirmed landing. `None` for test /
    /// non-REPL executors that don't drive a toast or journal.
    delivered_tx: Option<&'a Sender<DetectorEvent>>,
}

/// Deliver `body` to the agent. With a grid probe we type it (no
/// submit), confirm it landed in the input box, then send `\r` — only a
/// confirmed body gets the committing Enter. Without a probe (tests /
/// non-agent jobs) we keep the legacy type+submit behaviour.
fn do_inject(job: &InjectJob<'_>, probe: Option<&GridProbe>) {
    tracing::info!(
        job = job.job_id.0,
        agent = %job.agent_name,
        bytes = job.body.len() + 1,
        body = %job.body,
        "injection-exec: START typing payload",
    );
    match probe {
        Some(p) => deliver_confirmed(job, p),
        None => {
            let _ = type_body_pasted(job.writer, job.body)
                .and_then(|()| submit_input(job.writer, job.agent_name));
            done_log(job, false, 1);
        }
    }
}

/// Type the body, wait for it to appear in the grid, and submit. The
/// detector can fire during a boot lull — before the input box exists —
/// so the first type may be lost; we clear and re-type until it shows,
/// up to [`MAX_ATTEMPTS`]. If the body is never confirmed in the grid we
/// do NOT submit (fail-closed): an unverified `\r` on an unknown agent
/// state could validate an unintended dialog. The operator should attach
/// to the job to resolve it manually.
fn deliver_confirmed(job: &InjectJob<'_>, probe: &GridProbe) {
    // Clear any boot modal (trust dialog / onboarding) standing between
    // spawn and the input box. Every job reaching here passed the
    // dispatch trust gate, so the directory is consented — accepting the
    // agent's OWN boot trust dialog (Enter) honours that. Bounded to a
    // few modals and to this initial-delivery (boot) window only, so a
    // post-ready tool-permission prompt is never auto-accepted here.
    for _ in 0..MAX_MODALS {
        if !accept_modal(job, probe) {
            break;
        }
    }
    for attempt in 1..=MAX_ATTEMPTS {
        if attempt > 1 {
            // Wipe any partial input from a not-yet-ready earlier try so
            // the retype can't double the prompt.
            let _ = type_bytes(job.writer, &[CLEAR_LINE]);
        }
        if type_body_pasted(job.writer, job.body).is_err() {
            return;
        }
        if confirm_in_grid(probe, job.body) {
            thread::sleep(SUBMIT_AFTER_CONFIRM_DELAY);
            if submit_input(job.writer, job.agent_name).is_ok() {
                done_log(job, true, attempt);
            }
            return;
        }
        // A modal re-appeared (claude redraws its prompt) — accept it and
        // retry rather than typing the body into it.
        accept_modal(job, probe);
    }
    // Never submit while a confirmation menu is still up.
    if grid_shows_confirm_menu(probe) {
        tracing::warn!(
            job = job.job_id.0,
            "injection-exec: modal still up after retries; not submitting",
        );
        return;
    }
    tracing::warn!(
        job = job.job_id.0,
        agent = %job.agent_name,
        attempts = MAX_ATTEMPTS,
        "injection-exec: body NOT confirmed after retries; NOT submitting (fail-closed). \
         Attach to the job to deliver the prompt manually.",
    );
    // Fail-closed: do not submit an unverified prompt. A spurious \r on an
    // unknown agent state could accept an unintended dialog or corrupt the
    // session. The operator must attach to resolve.
}

fn submit_sequence(_agent_name: &str) -> &'static [u8] {
    b"\r"
}

fn submit_input(writer: &SharedWriter, agent_name: &str) -> std::io::Result<()> {
    let seq = submit_sequence(agent_name);
    type_bytes(writer, seq)
}

/// If a boot confirmation/trust modal is on screen, accept it (Enter —
/// the highlighted "Yes") and wait for it to clear. Returns whether one
/// was accepted. Safe because `deliver_confirmed` only runs for the
/// initial body of an agent that passed the trust gate (directory
/// consented) and only in the boot window — never for a post-ready
/// tool-permission prompt.
fn accept_modal(job: &InjectJob<'_>, probe: &GridProbe) -> bool {
    if !grid_shows_confirm_menu(probe) {
        return false;
    }
    let _ = type_bytes(job.writer, b"\r");
    tracing::info!(
        job = job.job_id.0,
        "injection-exec: auto-accepted boot trust/confirm modal (consented dir)",
    );
    let start = Instant::now();
    while grid_shows_confirm_menu(probe) && start.elapsed() < CONFIRM_TIMEOUT {
        thread::sleep(CONFIRM_POLL);
    }
    true
}

/// Conservative, **provider-agnostic** check: does the agent's grid show
/// a yes/no or trust confirmation menu? Used to refuse submitting an
/// injected prompt INTO such a menu, where `\r` would auto-select the
/// highlighted option (e.g. accept a trust dialog without consent). The
/// markers cover the confirmation language common to claude / codex /
/// gemini / kimi trust + permission prompts (modal footers, numbered
/// yes/no, inline `[y/n]`), so a normal input box never trips it. The
/// definitive per-provider detection lives in the trust flow.
fn grid_shows_confirm_menu(probe: &GridProbe) -> bool {
    const MARKERS: &[&str] = &[
        // Modal footers used across TUI confirmation dialogs.
        "esctocancel",
        "entertoconfirm",
        "entertoselect",
        // Trust-dialog wording.
        "trustthisfolder",
        "trustthefiles",
        "doyoutrust",
        "doyoutrustthe",
        // Numbered yes/no menus (`❯ 1. Yes` / `2. No`).
        "1.yes",
        "2.no",
        // Inline yes/no prompts.
        "[y/n]",
        "(y/n)",
    ];
    let grid = normalize(&probe());
    MARKERS.iter().any(|m| grid.contains(m))
}

fn done_log(job: &InjectJob<'_>, confirmed: bool, attempt: usize) {
    tracing::info!(
        job = job.job_id.0,
        agent = %job.agent_name,
        confirmed,
        attempt,
        "injection-exec: DONE",
    );
    // The body is now in the agent and submitted — announce the landing
    // so the REPL worker raises the "▸ prompt injected" toast and the
    // journal `Tell` at the real delivery time (this is the only place
    // a body is committed; the modal-blocked path returns without
    // reaching here, so no false "delivered" is emitted).
    if let Some(tx) = job.delivered_tx {
        let _ = tx.send(DetectorEvent::Delivered {
            job_id: job.job_id,
            agent_name: job.agent_name.to_string(),
            body: job.body.to_string(),
        });
    }
}

/// Type `body` wrapped in bracketed-paste markers so the agent's TUI
/// inserts it literally instead of interpreting it as keystrokes (see
/// [`PASTE_BEGIN`]). The trailing submit `\r` stays OUTSIDE the markers
/// — it is a keystroke.
fn type_body_pasted(writer: &SharedWriter, body: &str) -> std::io::Result<()> {
    type_bytes(writer, PASTE_BEGIN)?;
    type_bytes(writer, body.as_bytes())?;
    type_bytes(writer, PASTE_END)
}

/// Type `bytes` one at a time with a [`BYTE_GAP`] gap so a per-char TUI
/// stdin handler (claude) keeps up, the same as a human typist.
fn type_bytes(writer: &SharedWriter, bytes: &[u8]) -> std::io::Result<()> {
    for b in bytes {
        {
            let mut w = writer.lock();
            w.write_all(&[*b])?;
            w.flush()?;
        }
        thread::sleep(BYTE_GAP);
    }
    Ok(())
}

/// Poll the grid until the typed `body` appears (normalised), up to
/// [`CONFIRM_TIMEOUT`]. `true` means the agent echoed the input into
/// its visible box — proof the box was ready to receive it.
fn confirm_in_grid(probe: &GridProbe, body: &str) -> bool {
    let needle = normalize(body.as_bytes());
    if needle.is_empty() {
        return true;
    }
    // claude collapses a large multi-line paste into a placeholder
    // ("[Pasted text #1 +42 lines]") instead of echoing the content —
    // accept that as landing proof, but only when the body could have
    // collapsed (multi-line), so a stale placeholder never confirms a
    // single-line body.
    let placeholder_ok = body.contains('\n');
    let start = Instant::now();
    loop {
        let grid = normalize(&probe());
        if grid.contains(&needle) || (placeholder_ok && grid.contains("pastedtext")) {
            return true;
        }
        if start.elapsed() >= CONFIRM_TIMEOUT {
            return false;
        }
        thread::sleep(CONFIRM_POLL);
    }
}

/// Normalise terminal bytes for a robust substring match: strip ANSI
/// escapes, lower-case, drop ALL whitespace. Tolerant of SGR colouring,
/// box-drawing padding, prompt glyphs, and line-wrapping around the
/// echoed text.
fn normalize(bytes: &[u8]) -> String {
    strip_ansi(bytes)
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Remove CSI (`ESC [ … final`) and OSC (`ESC ] … BEL/ST`) sequences,
/// decoding the rest as lossy UTF-8. Defensive: an unterminated escape
/// consumes to end and never panics (CLAUDE.md: every byte untrusted).
fn strip_ansi(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        match bytes.get(i) {
            Some(b'[') => {
                i += 1;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1; // consume the final byte
            }
            Some(b']') => {
                i += 1;
                while i < bytes.len() && bytes[i] != 0x07 {
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                i += 1; // consume BEL or the '\' of ST
            }
            _ => i += 1, // ESC X / lone ESC
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[path = "injection_executor/tests.rs"]
#[cfg(test)]
mod tests;
