// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Ignored QA harness for real interactive agent binaries.
//!
//! These tests are deliberately not CI defaults: Codex/Claude/Gemini presence,
//! auth state, and trust-prompt wording are workstation-specific. They still
//! run through the production PTY spawn path and refuse headless flags.
//!
//! Provider selection is runtime-configurable:
//! - `ORKIA_QA_AGENT=codex|claude|gemini` resolves the provider binary on PATH.
//! - `ORKIA_QA_AGENT_BIN=/path/to/agent` runs an explicit binary.
//! - `ORKIA_QA_AGENT_ARGS="..."` appends interactive-mode args only.
//! - `ORKIA_QA_AGENT_MARKER="..."` overrides the provider default marker.
//! - `ORKIA_QA_AGENT_MARKERS="a||b"` accepts any listed marker.

use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, read};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use orkia_rfc_core::frontmatter::{OperatorConstraints, OperatorFrontmatterBlock};
use orkia_rfc_core::{RfcId, RfcStore};
use orkia_shell::agent_context::AgentContext;
use orkia_shell::approval::ApprovalWatcher;
use orkia_shell::hooks::install_hooks;
use orkia_shell::injection_executor::{InjectionExecutor, output_transcript_probe};
use orkia_shell::job::JobController;
use orkia_shell::job::config::{Attachment, JobConfig};
use orkia_shell::job::spawn::SpawnDeps;
use orkia_shell::journal::{JournalEnvelope, JournalListener, LiveJournalHandlers};
use orkia_shell::protocol::{EventRouter, FanoutConfig};
use orkia_shell::terminal_state::DetectorEvent;
use orkia_shell::terminal_state::TerminalStateMachine;
use orkia_shell_types::ProviderId;
use orkia_shell_types::{ProcessGroupMode, StdinSource};
use tempfile::TempDir;
use tokio::sync::mpsc;

mod common;
use common::{FakeAgent, spawn_fake_agent};

const OPERATOR_QA_PROMPT: &str =
    "Create file operator-qa-outside.txt in this directory with content qa, then stop.";

#[test]
#[ignore = "requires ORKIA_QA_AGENT or ORKIA_QA_AGENT_BIN and a locally authenticated interactive agent"]
fn real_agent_trust_prompt_or_initial_screen_is_visible_in_pty() {
    let qa = RealAgentQa::from_env();
    let mut session = qa.spawn();
    let output = session.read_until_marker(Duration::from_secs(8));
    assert!(
        qa.markers.iter().any(|marker| output.contains(marker)),
        "expected one of markers {:?} in real PTY output; got:\n{}",
        qa.markers,
        output
    );
    assert_expected_fragments(&output);
    session.stop();
}

#[test]
#[ignore = "requires ORKIA_QA_AGENT or ORKIA_QA_AGENT_BIN; validates control bytes against a real TUI"]
fn real_agent_receives_ctrl_c_and_ctrl_z_without_pty_corruption() {
    let qa = RealAgentQa::from_env();
    let mut session = qa.spawn();
    let _ = session.read_for(Duration::from_secs(2));

    session.write_control(0x03);
    let after_ctrl_c = session.read_for(Duration::from_secs(2));
    assert!(
        !after_ctrl_c.contains("panic"),
        "Ctrl-C produced panic-like output:\n{after_ctrl_c}"
    );

    session.write_control(0x1a);
    let after_ctrl_z = session.read_for(Duration::from_secs(2));
    assert!(
        !after_ctrl_z.contains("panic"),
        "Ctrl-Z produced panic-like output:\n{after_ctrl_z}"
    );
    session.stop();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ORKIA_QA_AGENT or ORKIA_QA_AGENT_BIN; drives a real TUI until a provider hook reaches the operator"]
async fn real_agent_operator_drift_flows_from_hook_to_notification() {
    let qa = RealAgentQa::from_env();
    let mut fixture = OperatorQaFixture::new(&qa);
    let mut session = fixture.spawn_agent(&qa, operator_uses_initial_prompt(&qa));
    let initial_prompt = operator_uses_initial_prompt(&qa);

    let mut startup = session.read_until(Duration::from_secs(45), |text| {
        codex_ready_for_prompt(text) || codex_trust_prompt(text)
    });
    if codex_trust_prompt(&startup) {
        session.write_text("\r");
        startup.push_str(&session.read_until(Duration::from_secs(45), codex_ready_for_prompt));
    }
    if initial_prompt {
        startup.push_str(&session.read_for(Duration::from_secs(3)));
    } else {
        if !codex_ready_for_prompt(&startup) {
            startup.push_str(&session.read_for(Duration::from_secs(8)));
        }
        startup.push_str(&session.read_for(Duration::from_secs(8)));
        session.write_text(OPERATOR_QA_PROMPT);
    }

    let mut output = String::new();
    let event = fixture
        .recv_operator_event(&mut session, &mut output, Duration::from_secs(90))
        .await;
    assert_eq!(event.event.as_deref(), Some("operator.drift_detected"));
    assert!(
        event
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("outside allowed_paths"),
        "expected hard drift notification; event={event:?}; pty output:\n{output}"
    );
    let suggestion = fixture
        .recv_operator_event(&mut session, &mut output, Duration::from_secs(10))
        .await;
    assert_eq!(
        suggestion.event.as_deref(),
        Some("operator.suggestion_created")
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires ORKIA_QA_AGENT=claude and a locally authenticated interactive claude; drives the real TUI until a Write hook reaches the operator"]
async fn real_claude_operator_drift_flows_from_hook_to_notification() {
    // Real-agent acceptance for the drift detector (`operator.rs`) against a
    // live claude TUI. Mirrors `real_agent_operator_drift_flows_from_hook_to_notification`
    // but with claude-shaped readiness/trust handling and the claude bridge
    // routing (HOME wrapper, same mechanism as the codex path).
    //
    // Flow: an RFC declares `allowed_paths = ["allowed/**"]`. We ask claude to
    // create a file in the project root (outside `allowed/**`). Claude's
    // PreToolUse hook for the Write reaches the operator, which emits a hard
    // `operator.drift_detected` ("outside allowed_paths") followed by an inert
    // `operator.suggestion_created`. Run it with:
    //   ORKIA_QA_AGENT=claude cargo test -p orkia-shell \
    //     --test agent_pty_attention_qa \
    //     real_claude_operator_drift -- --ignored --nocapture
    let qa = RealAgentQa::from_env();
    assert_eq!(qa.provider, "claude", "set ORKIA_QA_AGENT=claude");
    let mut fixture = OperatorQaFixture::new(&qa);
    // Claude never takes the prompt as a `-p` arg (Invariant 5) — we inject it
    // interactively once the TUI is ready, through the production injection path.
    let mut session = fixture.spawn_agent(&qa, false);

    // Detect trust/readiness against the rendered grid, not the raw byte
    // stream: claude positions each word with an absolute cursor move, so
    // multi-word phrases are only contiguous once the grid is rendered.
    let startup = session.wait_snapshot(Duration::from_secs(45), |snap| {
        claude_ready_for_prompt(snap) || claude_trust_prompt(snap)
    });
    if claude_trust_prompt(&startup) {
        // Trust dialog defaults to the proceed option; Enter accepts it.
        session.write_text("\r");
        let _ = session.wait_snapshot(Duration::from_secs(30), claude_ready_for_prompt);
    }
    // PreToolUse fires before any permission dialog, so the drift surfaces even
    // if claude then pauses to ask — we assert on the operator event, not the write.
    session.write_text(OPERATOR_QA_PROMPT);

    let mut output = String::new();
    let event = fixture
        .recv_operator_event(&mut session, &mut output, Duration::from_secs(120))
        .await;
    assert_eq!(event.event.as_deref(), Some("operator.drift_detected"));
    assert!(
        event
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("outside allowed_paths"),
        "expected hard drift notification; event={event:?}; pty output:\n{output}"
    );
    let suggestion = fixture
        .recv_operator_event(&mut session, &mut output, Duration::from_secs(10))
        .await;
    assert_eq!(
        suggestion.event.as_deref(),
        Some("operator.suggestion_created")
    );
}

#[test]
#[ignore = "requires ORKIA_QA_AGENT=codex; empirically probes Codex TUI submit bytes"]
fn real_codex_submit_key_probe() {
    let qa = RealAgentQa::from_env();
    assert_eq!(qa.provider, "codex", "set ORKIA_QA_AGENT=codex");
    let dir = TempDir::new().expect("tmp");
    let home = dir.path().join("home");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    install_provider_hooks(&project_dir, &home, &qa);
    let agent_env = prepare_agent_env(dir.path(), &qa);
    let mut session = spawn_real_agent_in_project(RealAgentSpawn {
        qa: &qa,
        project_dir: &project_dir,
        operator_e2e: true,
        include_initial_prompt: false,
        extra_env: agent_env,
    });
    let mut output = session.read_until(Duration::from_secs(45), |text| {
        codex_ready_for_prompt(text) || codex_trust_prompt(text)
    });
    if codex_trust_prompt(&output) {
        session.write_raw(b"\r");
        output.push_str(&session.read_for(Duration::from_secs(25)));
    }
    output.push_str(&session.read_for(Duration::from_secs(15)));

    let candidates: &[(&str, &[u8])] = &[
        ("CR", b"\r"),
        ("LF", b"\n"),
        ("CRLF", b"\r\n"),
        ("CSI_13u", b"\x1b[13u"),
        ("CSI_13_1u", b"\x1b[13;1u"),
        ("SS3_M", b"\x1bOM"),
    ];
    for (name, seq) in candidates {
        let marker = format!("probe-submit-{name}");
        session.write_raw(&[0x15]);
        type_raw_slow(&mut session, marker.as_bytes());
        let before = session.read_for(Duration::from_secs(1));
        session.write_raw(seq);
        let after = session.read_for(Duration::from_secs(4));
        println!("candidate={name}\nbefore={before}\nafter={after}\n");
        if codex_submit_was_accepted(&after) {
            session.stop();
            return;
        }
    }
    panic!(
        "no submit candidate worked; last output:\n{}",
        session.read_for(Duration::from_secs(1))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual interactive probe: shows real Codex TUI and logs Orkia-visible PTY/readiness state"]
async fn real_codex_manual_readiness_probe() {
    let qa = RealAgentQa::from_env();
    assert_eq!(qa.provider, "codex", "set ORKIA_QA_AGENT=codex");

    let mut fixture = OperatorQaFixture::new(&qa);
    let mut session = fixture.spawn_agent(&qa, false);
    let log_path = manual_probe_log_path();
    let exit_path = manual_probe_exit_path();
    let _ = std::fs::remove_file(&exit_path);
    if let Some(parent) = log_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).expect("create manual readiness log directory");
    }
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open manual readiness log");

    writeln!(
        std::io::stderr(),
        "\nmanual Codex readiness probe\nlog: {}\nexit file: {}\nType in the Codex TUI normally. Press Ctrl-] to stop, or from another terminal run:\n  touch {}\n",
        log_path.display(),
        exit_path.display(),
        exit_path.display()
    )
    .expect("write instructions");
    writeln!(
        log,
        "manual Codex readiness probe started\nlog={}\nexit_file={}\nprompt_hint={OPERATOR_QA_PROMPT:?}\n",
        log_path.display(),
        exit_path.display()
    )
    .expect("write log header");

    let _raw = RawModeGuard::enable();
    let (stdin_tx, stdin_rx) = std_mpsc::channel::<ManualInput>();
    thread::spawn(move || {
        while let Ok(event) = read() {
            let input = manual_input_from_event(event);
            let stop = matches!(input, Some(ManualInput::Stop));
            if let Some(input) = input
                && stdin_tx.send(input).is_err()
            {
                break;
            }
            if stop {
                break;
            }
        }
    });

    let timeout = manual_probe_timeout();
    let started = Instant::now();
    let mut last_snapshot = Instant::now();
    let mut transcript = String::new();
    let mut stdout = std::io::stdout().lock();

    while started.elapsed() < timeout {
        if exit_path.exists() {
            writeln!(log, "\n[manual] stop requested with exit file").expect("write stop");
            session.stop();
            return;
        }

        while let Ok(input) = stdin_rx.try_recv() {
            match input {
                ManualInput::Stop => {
                    writeln!(log, "\n[manual] stop requested with ctrl-]").expect("write stop");
                    session.stop();
                    return;
                }
                ManualInput::Bytes(bytes) => {
                    log_byte(&mut log, "stdin", &bytes);
                    session.write_raw(&bytes);
                }
            }
        }

        while let Ok(chunk) = session.rx.try_recv() {
            stdout.write_all(&chunk).expect("write PTY to stdout");
            stdout.flush().expect("flush stdout");
            log_byte(&mut log, "pty", &chunk);
            transcript.push_str(&String::from_utf8_lossy(&chunk));
            if transcript.len() > 24_000 {
                let keep_from = transcript.len().saturating_sub(12_000);
                transcript = transcript[keep_from..].to_string();
            }
        }

        for event in session.drain_delivery_events() {
            writeln!(log, "\n[delivery_event] {event:?}").expect("write delivery event");
        }
        while let Ok(env) = fixture.journal_rx.try_recv() {
            writeln!(log, "\n[journal] {env:?}").expect("write journal event");
        }

        if last_snapshot.elapsed() >= Duration::from_millis(500) {
            let snapshot = session.visible_snapshot();
            log_readiness_state(&mut log, started.elapsed(), &transcript, &snapshot);
            last_snapshot = Instant::now();
        }

        thread::sleep(Duration::from_millis(15));
    }

    writeln!(log, "\n[manual] timeout after {:?}", timeout).expect("write timeout");
    session.stop();
}

struct RealAgentQa {
    provider: String,
    bin: String,
    args: Vec<String>,
    markers: Vec<String>,
}

impl RealAgentQa {
    fn from_env() -> Self {
        let provider = env::var("ORKIA_QA_AGENT").unwrap_or_else(|_| "custom".into());
        let bin = resolve_agent_bin(&provider);
        let args = env::var("ORKIA_QA_AGENT_ARGS")
            .unwrap_or_default()
            .split_whitespace()
            .map(String::from)
            .collect::<Vec<_>>();
        assert!(
            args.iter().all(|arg| !is_headless_arg(arg)),
            "QA must run interactive TUI mode, not print/headless mode"
        );
        let markers = configured_markers(&provider);
        Self {
            provider,
            bin,
            args,
            markers,
        }
    }

    fn spawn(&self) -> RealAgentSession {
        let dir = TempDir::new().expect("tmp");
        let (mut jobs, _events) = JobController::new();
        let job_id = spawn_fake_agent(
            &mut jobs,
            dir.path(),
            FakeAgent::cmd(&self.provider, &self.bin, &self.args),
        );
        let rx = jobs
            .get(job_id)
            .expect("job entry")
            .engine
            .subscribe_output();
        RealAgentSession {
            provider: self.provider.clone(),
            dir,
            jobs,
            job_id,
            rx,
            stopped: false,
            injection_executor: None,
            delivery_rx: None,
        }
    }
}

struct RealAgentSession {
    provider: String,
    dir: TempDir,
    jobs: JobController,
    job_id: orkia_shell_types::JobId,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    stopped: bool,
    injection_executor: Option<InjectionExecutor>,
    delivery_rx: Option<std_mpsc::Receiver<DetectorEvent>>,
}

impl RealAgentSession {
    fn read_until_marker(&mut self, timeout: Duration) -> String {
        let markers = configured_markers(&self.provider);
        self.read_until(timeout, |text| {
            markers.iter().any(|marker| text.contains(marker))
        })
    }

    fn read_for(&mut self, timeout: Duration) -> String {
        self.read_until(timeout, |_| false)
    }

    fn read_until(&mut self, timeout: Duration, done: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + timeout;
        let mut bytes = Vec::new();
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    bytes.extend_from_slice(&chunk);
                    let text = String::from_utf8_lossy(&bytes);
                    if done(&text) {
                        return text.into_owned();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Wait until the rendered grid satisfies `done`, polling the engine's
    /// visible snapshot. Claude (Ink) positions each word with an absolute
    /// cursor move, so multi-word phrases are never contiguous in the raw
    /// byte stream — only the rendered grid has clean, readable text. Trust
    /// and readiness detection must run against the grid, not the raw bytes.
    fn wait_snapshot(&mut self, timeout: Duration, done: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let snap = self.visible_snapshot();
            if done(&snap) || Instant::now() >= deadline {
                return snap;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn write_control(&mut self, byte: u8) {
        self.jobs
            .write_to_pty(self.job_id, &[byte])
            .expect("write control byte");
    }

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if let Some(exec) = &self.injection_executor {
            exec.unregister(self.job_id);
        }
        let _ = self.jobs.stop(self.job_id);
        let _ = self.dir.path();
    }

    fn write_text(&mut self, text: &str) {
        if !text.ends_with('\r')
            && !text.ends_with('\n')
            && let Some(exec) = &self.injection_executor
        {
            exec.inject(self.job_id, &self.provider, text);
            return;
        }
        self.jobs
            .write_to_pty(self.job_id, text.as_bytes())
            .expect("write text");
    }

    fn write_raw(&mut self, bytes: &[u8]) {
        self.jobs
            .write_to_pty(self.job_id, bytes)
            .expect("write raw");
    }

    fn drain_available(&mut self) -> String {
        let mut bytes = Vec::new();
        while let Ok(chunk) = self.rx.try_recv() {
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn drain_delivery_events(&mut self) -> Vec<DetectorEvent> {
        let Some(rx) = &self.delivery_rx else {
            return Vec::new();
        };
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn visible_snapshot(&self) -> String {
        self.jobs
            .get(self.job_id)
            .map(|entry| String::from_utf8_lossy(&entry.engine.render_visible_snapshot()).into())
            .unwrap_or_default()
    }
}

fn type_raw_slow(session: &mut RealAgentSession, bytes: &[u8]) {
    for byte in bytes {
        session.write_raw(&[*byte]);
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Drop for RealAgentSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn resolve_agent_bin(provider: &str) -> String {
    if let Ok(bin) = env::var("ORKIA_QA_AGENT_BIN") {
        return bin;
    }
    let command = match provider {
        "codex" | "claude" | "gemini" => provider,
        "custom" => {
            panic!("set ORKIA_QA_AGENT=codex|claude|gemini or ORKIA_QA_AGENT_BIN=/path/to/agent")
        }
        other => other,
    };
    find_on_path(command)
        .unwrap_or_else(|| panic!("could not find interactive agent binary `{command}` on PATH"))
        .to_string_lossy()
        .into_owned()
}

struct OperatorQaFixture {
    _dir: TempDir,
    project_dir: PathBuf,
    agent_env: Vec<(String, String)>,
    journal_rx: mpsc::UnboundedReceiver<JournalEnvelope>,
    _listener: JournalListener,
    _fanout: tokio::task::JoinHandle<()>,
    _operator: tokio::task::JoinHandle<()>,
    scopes: orkia_kernel::JobScopes,
}

impl OperatorQaFixture {
    fn new(qa: &RealAgentQa) -> Self {
        let dir = TempDir::new().expect("tmp");
        let home = dir.path().join("home");
        let data_dir = home.join(".orkia");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        write_operator_rfc(&data_dir);
        install_provider_hooks(&project_dir, &home, qa);
        let agent_env = prepare_agent_env(dir.path(), qa);

        let (router, router_rx) = EventRouter::new_with_rx();
        let (journal_tx, journal_rx) = mpsc::unbounded_channel();
        let listener = JournalListener::start_with_channel(
            &data_dir,
            LiveJournalHandlers {
                router: Some(std::sync::Arc::new(router.clone())
                    as std::sync::Arc<dyn orkia_shell::journal::HookRouter>),
                ..Default::default()
            },
            journal_tx.clone(),
        )
        .expect("journal listener");
        let scopes = orkia_kernel::new_job_scopes();
        let (operator_tx, operator_rx) = mpsc::unbounded_channel();
        let fanout = orkia_shell::protocol::spawn_fanout(
            router_rx,
            FanoutConfig {
                job_scopes: scopes.clone(),
                outputs: vec![operator_tx],
            },
        );
        let operator = orkia_shell::operator::spawn(
            operator_rx,
            orkia_shell::operator::OperatorConfig {
                data_dir: data_dir.clone(),
                router,
                journal_tx: Some(journal_tx),
            },
        );
        Self {
            _dir: dir,
            project_dir,
            agent_env,
            journal_rx,
            _listener: listener,
            _fanout: fanout,
            _operator: operator,
            scopes,
        }
    }

    fn spawn_agent(&mut self, qa: &RealAgentQa, include_initial_prompt: bool) -> RealAgentSession {
        let session = spawn_real_agent_in_project(RealAgentSpawn {
            qa,
            project_dir: &self.project_dir,
            operator_e2e: true,
            include_initial_prompt,
            extra_env: self.agent_env.clone(),
        });
        let mut map = self.scopes.write().expect("scope lock");
        map.insert(
            session.job_id.0,
            orkia_kernel::JobScope {
                project_id: None,
                rfc_ref: Some(orkia_reasoning_core::dto::RfcRef::new(RfcId::new(
                    "operator-qa",
                ))),
            },
        );
        drop(map);
        session
    }

    async fn recv_operator_event(
        &mut self,
        session: &mut RealAgentSession,
        output: &mut String,
        timeout: Duration,
    ) -> JournalEnvelope {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            output.push_str(&session.drain_available());
            for event in session.drain_delivery_events() {
                output.push_str(&format!("\n[delivery_event] {event:?}\n"));
            }
            if output.contains("Please restart Codex") || output.contains("Update ran successfully")
            {
                panic!(
                    "agent updated itself and requires restart; rerun this test. pty output:\n{output}"
                );
            }
            match self.journal_rx.try_recv() {
                Ok(env) if env.source.as_deref() == Some("orkia-operator") => return env,
                Ok(_) => {}
                Err(mpsc::error::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    panic!("journal channel disconnected; pty output:\n{output}");
                }
            }
        }
        let snapshot = session.visible_snapshot();
        panic!(
            "timed out waiting for operator event; pty output:\n{output}\nvisible snapshot:\n{snapshot}"
        );
    }
}

fn write_operator_rfc(data_dir: &std::path::Path) {
    let project_dir = data_dir.join("projects").join("qa");
    std::fs::create_dir_all(&project_dir).expect("rfc project dir");
    let store = RfcStore::new(project_dir);
    let id = RfcId::new("operator-qa");
    let mut rec = store.create(&id, Some("operator qa")).expect("create rfc");
    rec.fm.operator = Some(OperatorFrontmatterBlock {
        constraints: Some(OperatorConstraints {
            allowed_paths: vec!["allowed/**".into()],
            forbidden_paths: Vec::new(),
            forbidden_commands: Vec::new(),
            risk_ceiling: Some("high".into()),
            watch_paths: Vec::new(),
            contract_paths: Vec::new(),
        }),
    });
    store
        .save(rec.fm, "QA RFC: only allowed/** may be written.\n".into())
        .expect("save rfc");
}

fn install_provider_hooks(
    project_dir: &std::path::Path,
    bridge_home: &std::path::Path,
    qa: &RealAgentQa,
) {
    let provider = ProviderId::parse(&qa.provider);
    install_hooks(project_dir, provider, false).expect("install hooks");
    if provider == ProviderId::Codex {
        patch_codex_hook_home(project_dir, bridge_home);
    }
    if provider == ProviderId::Claude {
        patch_claude_hook_home(project_dir, bridge_home);
    }
}

/// Route claude's bridge hooks to the fixture journal the same way the codex
/// path does: rewrite each `command` in `.claude/settings.json` to a wrapper
/// that runs `orkia bridge` with `HOME` pointed at `bridge_home` (whose
/// `.orkia` IS the fixture data_dir). Claude itself keeps the real `HOME` so
/// its keychain auth still works — only the bridge subprocess is redirected.
fn patch_claude_hook_home(project_dir: &std::path::Path, bridge_home: &std::path::Path) {
    let path = project_dir.join(".claude").join("settings.json");
    let raw = std::fs::read_to_string(&path).expect("read claude settings");
    let mut settings: serde_json::Value =
        serde_json::from_str(&raw).expect("parse claude settings");
    let bridge = write_claude_bridge_wrapper(project_dir, bridge_home);
    replace_hook_commands(&mut settings, &bridge.to_string_lossy());
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&settings).expect("serialize claude settings"),
    )
    .expect("write claude settings");
}

fn write_claude_bridge_wrapper(
    project_dir: &std::path::Path,
    bridge_home: &std::path::Path,
) -> PathBuf {
    let dir = project_dir.join(".orkia-qa");
    std::fs::create_dir_all(&dir).expect("create hook wrapper dir");
    let path = dir.join("claude-hook-bridge.sh");
    // The spawn injects `ORKIA_SOCKET_PATH` pointing at the JobController's own
    // run-dir socket (no listener in this fixture); claude's hook subprocess
    // inherits it, and `orkia bridge`'s `socket_path()` prefers it over the
    // HOME-derived path. Unset it so the bridge falls back to
    // `$HOME/.orkia/run/orkia.sock` — which, with HOME=bridge_home, is exactly
    // the fixture journal listener's socket.
    let body = format!(
        "#!/bin/sh\nunset ORKIA_SOCKET_PATH\nHOME={} exec {} bridge --source claude --scope job\n",
        shell_quote(&bridge_home.to_string_lossy()),
        shell_quote(&resolve_orkia_bridge_bin().to_string_lossy())
    );
    std::fs::write(&path, body).expect("write hook wrapper");
    make_executable(&path);
    path
}

fn prepare_agent_env(test_root: &std::path::Path, qa: &RealAgentQa) -> Vec<(String, String)> {
    if qa.provider != "codex" {
        return Vec::new();
    }
    let codex_home = test_root.join("codex-home");
    std::fs::create_dir_all(&codex_home).expect("create isolated codex home");
    copy_codex_auth(&codex_home);
    write_codex_qa_config(&codex_home);
    vec![(
        "CODEX_HOME".into(),
        codex_home.to_string_lossy().into_owned(),
    )]
}

fn copy_codex_auth(codex_home: &std::path::Path) {
    let Some(home) = env::var_os("HOME") else {
        return;
    };
    let source = PathBuf::from(home).join(".codex").join("auth.json");
    if source.is_file() {
        let dest = codex_home.join("auth.json");
        std::fs::copy(source, dest).expect("copy codex auth into isolated home");
    }
}

fn write_codex_qa_config(codex_home: &std::path::Path) {
    let body = "[features]\nhooks = true\n";
    std::fs::write(codex_home.join("config.toml"), body).expect("write codex qa config");
}

fn patch_codex_hook_home(project_dir: &std::path::Path, bridge_home: &std::path::Path) {
    let path = project_dir.join(".codex").join("hooks.json");
    let raw = std::fs::read_to_string(&path).expect("read codex hooks");
    let mut hooks: serde_json::Value = serde_json::from_str(&raw).expect("parse codex hooks");
    let bridge = write_codex_bridge_wrapper(project_dir, bridge_home);
    replace_hook_commands(&mut hooks, &bridge.to_string_lossy());
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&hooks).expect("serialize codex hooks"),
    )
    .expect("write codex hooks");
}

fn write_codex_bridge_wrapper(
    project_dir: &std::path::Path,
    bridge_home: &std::path::Path,
) -> PathBuf {
    let dir = project_dir.join(".orkia-qa");
    std::fs::create_dir_all(&dir).expect("create hook wrapper dir");
    let path = dir.join("codex-hook-bridge.sh");
    let body = format!(
        "#!/bin/sh\nHOME={} exec {} bridge --source codex\n",
        shell_quote(&bridge_home.to_string_lossy()),
        shell_quote(&resolve_orkia_bridge_bin().to_string_lossy())
    );
    std::fs::write(&path, body).expect("write hook wrapper");
    make_executable(&path);
    path
}

#[cfg(unix)]
fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .expect("hook wrapper metadata")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod hook wrapper");
}

#[cfg(not(unix))]
fn make_executable(_path: &std::path::Path) {}

fn replace_hook_commands(value: &mut serde_json::Value, command: &str) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("type").and_then(serde_json::Value::as_str) == Some("command")
                && map.contains_key("command")
            {
                map.insert(
                    "command".into(),
                    serde_json::Value::String(command.to_string()),
                );
            }
            for child in map.values_mut() {
                replace_hook_commands(child, command);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                replace_hook_commands(item, command);
            }
        }
        _ => {}
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn resolve_orkia_bridge_bin() -> PathBuf {
    if let Ok(path) = env::var("CARGO_BIN_EXE_orkia") {
        return PathBuf::from(path);
    }
    let exe = env::current_exe().expect("current test exe path");
    let debug_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("target debug dir");
    let bin = debug_dir.join("orkia");
    assert!(
        bin.is_file(),
        "expected built orkia binary at {}; run `cargo build -p orkia` before the real-agent QA test",
        bin.display()
    );
    bin
}

struct RealAgentSpawn<'a> {
    qa: &'a RealAgentQa,
    project_dir: &'a std::path::Path,
    operator_e2e: bool,
    include_initial_prompt: bool,
    extra_env: Vec<(String, String)>,
}

fn spawn_real_agent_in_project(spec: RealAgentSpawn<'_>) -> RealAgentSession {
    let dir = TempDir::new().expect("tmp");
    let (mut jobs, _events) = JobController::new();
    let approvals = ApprovalWatcher::new(dir.path());
    let state_machine = TerminalStateMachine::new();
    let (delivery_tx, delivery_rx) = std_mpsc::channel();
    let injection_executor = InjectionExecutor::spawn_with_delivery(delivery_tx);
    let job_projects = Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new()));
    let router = EventRouter::new();
    let args = agent_args(
        spec.qa,
        spec.operator_e2e,
        spec.project_dir,
        spec.include_initial_prompt,
    );
    let mut env = vec![("ORKIA_AGENT_NAME".into(), spec.qa.provider.clone())];
    env.extend(spec.extra_env);
    // Claude renders its TUI inline on the primary screen (no alt-screen), so
    // the engine reader only advances the alacritty grid — and thus the grid
    // probe used for prompt detection and the visible snapshot only work — when
    // the engine is marked persistent. Production marks an engine persistent iff
    // the spawn carries an `AgentContext` (every `@agent` dispatch does). Attach
    // a minimal one so claude spawns exactly as in production. Codex detects
    // readiness off the raw output transcript, so it needs no agent context.
    let attachments = if spec.qa.provider == "claude" {
        vec![Attachment::AgentContext {
            context: minimal_agent_context(),
        }]
    } else {
        Vec::new()
    };
    let config = JobConfig {
        command: &spec.qa.bin,
        // Generic keeps the spawn plan from injecting provider MCP-config flags
        // we don't want here; the attachment alone flips the engine persistent.
        provider: orkia_shell_types::ProviderId::Generic,
        args: &args,
        label: format!("{} ({})", spec.qa.provider, spec.qa.bin),
        env,
        working_dir: Some(spec.project_dir.to_path_buf()),
        stdin: StdinSource::Pty,
        process_group: ProcessGroupMode::NewSession,
        attachments,
        cage_wrapper: None,
    };
    let deps = SpawnDeps {
        approvals: &approvals,
        event_router: &router,
        state_machine: &state_machine,
        injection_executor: &injection_executor,
        job_projects: &job_projects,
        agent_name: &spec.qa.provider,
    };
    let job_id = jobs.spawn(config, deps).expect("spawn real agent").job_id;
    let entry = jobs.get(job_id).expect("job entry");
    let probe = if spec.qa.provider == "codex" {
        Some(output_transcript_probe(entry.engine.subscribe_output()))
    } else {
        Some(entry.engine.grid_probe())
    };
    injection_executor.register(job_id, entry.engine.writer(), probe);
    let rx = entry.engine.subscribe_output();
    RealAgentSession {
        provider: spec.qa.provider.clone(),
        dir,
        jobs,
        job_id,
        rx,
        stopped: false,
        injection_executor: Some(injection_executor),
        delivery_rx: Some(delivery_rx),
    }
}

/// A minimal `AgentContext` whose only purpose is to flip the engine into
/// persistent (always-live grid) mode via `has_agent_context()`. Empty system
/// prompt / memory / tools — we don't want any provider MCP-config args, just
/// the persistent grid that claude's inline TUI needs to render and be probed.
fn minimal_agent_context() -> AgentContext {
    AgentContext {
        name: "operator-qa".into(),
        assembled: String::new(),
        system_prompt: String::new(),
        memory: String::new(),
        tools: orkia_shell_types::AgentToolsFile::default(),
        knowledge_mcp_bridge: false,
    }
}

fn agent_args(
    qa: &RealAgentQa,
    operator_e2e: bool,
    project_dir: &std::path::Path,
    include_initial_prompt: bool,
) -> Vec<String> {
    let mut args = qa.args.clone();
    if operator_e2e && qa.provider == "codex" {
        append_missing(&mut args, "--disable", "codex_hooks");
        append_missing(&mut args, "--enable", "hooks");
        append_flag(&mut args, "--dangerously-bypass-hook-trust");
        append_missing(&mut args, "--ask-for-approval", "never");
        append_missing(&mut args, "--sandbox", "danger-full-access");
        append_missing(
            &mut args,
            "-c",
            &format!(
                "projects.\"{}\".trust_level=\"trusted\"",
                toml_key_escape(&project_dir.to_string_lossy())
            ),
        );
        if include_initial_prompt {
            args.push(OPERATOR_QA_PROMPT.into());
        }
    }
    args
}

fn operator_uses_initial_prompt(qa: &RealAgentQa) -> bool {
    qa.provider == "codex"
        && env::var("ORKIA_QA_OPERATOR_INJECT").is_err()
        && env::var("ORKIA_QA_SUBMIT_PROBE").is_err()
}

fn codex_ready_for_prompt(output: &str) -> bool {
    output.contains("To get started")
        || output.contains("/init")
        || output.contains("tab to queue message")
        || output.contains("Run /review on my current changes")
        || output.contains("Write tests for @filename")
}

fn codex_submit_was_accepted(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("working")
        || lower.contains("thinking")
        || output.contains("UserPromptSubmit hook")
        || lower.contains("tokens")
}

fn codex_trust_prompt(output: &str) -> bool {
    output.contains("trust") && output.contains("Press enter")
}

/// Strip ANSI/VT escape sequences so phrase matching sees only the visible
/// text. The grid snapshot interleaves SGR colour and cursor escapes between
/// styled runs; without stripping, a phrase that crosses a style boundary
/// would not match as a contiguous substring.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut bytes = input.bytes().peekable();
    while let Some(b) = bytes.next() {
        if b == 0x1b {
            // CSI: ESC [ ... final-byte in 0x40..=0x7e; otherwise a short
            // escape — drop the single following byte.
            if bytes.peek() == Some(&b'[') {
                bytes.next();
                for c in bytes.by_ref() {
                    if (0x40..=0x7e).contains(&c) {
                        break;
                    }
                }
            } else {
                bytes.next();
            }
        } else {
            out.push(b as char);
        }
    }
    out
}

fn claude_ready_for_prompt(output: &str) -> bool {
    // Real claude 2.1.177 ready screen markers (verified against the live TUI):
    // the input box footer carries "auto mode on", the banner says "Welcome
    // back". Older hints kept for forward/back compatibility across versions.
    let text = strip_ansi(output);
    text.contains("auto mode on")
        || text.contains("shift+tab to cycle")
        || text.contains("Welcome back")
        || text.contains("for shortcuts")
        || text.contains("Welcome to Claude")
        || text.contains("? for help")
        || text.contains("Try \"")
}

fn claude_trust_prompt(output: &str) -> bool {
    // Real claude 2.1.177 trust dialog (verified against the live TUI):
    // "Quick safety check: Is this a project you created or one you trust?"
    // with "1. Yes, I trust this folder". Older wording kept as a fallback.
    let text = strip_ansi(output);
    text.contains("I trust this folder")
        || text.contains("Is this a project you created or one you trust")
        || text.contains("Do you trust the files")
        || (text.contains("trust") && text.contains("proceed"))
}

fn toml_key_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn append_missing(args: &mut Vec<String>, flag: &str, value: &str) {
    if args.iter().any(|arg| arg == flag) {
        return;
    }
    args.push(flag.into());
    args.push(value.into());
}

fn append_flag(args: &mut Vec<String>, flag: &str) {
    if !args.iter().any(|arg| arg == flag) {
        args.push(flag.into());
    }
}

fn find_on_path(command: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

fn configured_markers(provider: &str) -> Vec<String> {
    if let Ok(marker) = env::var("ORKIA_QA_AGENT_MARKER") {
        return vec![marker];
    }
    if let Ok(raw) = env::var("ORKIA_QA_AGENT_MARKERS") {
        let markers: Vec<_> = raw
            .split("||")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !markers.is_empty() {
            return markers;
        }
    }
    default_markers(provider)
}

fn default_markers(provider: &str) -> Vec<String> {
    match provider {
        "codex" => vec![
            "Do you trust".into(),
            "Press enter to continue".into(),
            "To get started".into(),
        ],
        "claude" => vec!["Claude".into(), "Do you trust".into(), ">".into()],
        "gemini" => vec!["Gemini".into(), ">".into()],
        _ => vec![">".into()],
    }
}

fn is_headless_arg(arg: &str) -> bool {
    matches!(
        arg,
        "-p" | "--prompt" | "exec" | "run" | "--print" | "--headless" | "--non-interactive"
    )
}

fn assert_expected_fragments(output: &str) {
    let Ok(raw) = env::var("ORKIA_QA_AGENT_EXPECT_CONTAINS") else {
        return;
    };
    for fragment in raw.split("||").map(str::trim).filter(|s| !s.is_empty()) {
        assert!(
            output.contains(fragment),
            "expected stable provider fragment {:?} in PTY output; got:\n{}",
            fragment,
            output
        );
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Self {
        enable_raw_mode().expect("enable terminal raw mode");
        Self
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

enum ManualInput {
    Bytes(Vec<u8>),
    Stop,
}

fn manual_input_from_event(event: Event) -> Option<ManualInput> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => key_event_to_input(key),
        Event::Paste(text) => Some(ManualInput::Bytes(text.into_bytes())),
        _ => None,
    }
}

fn key_event_to_input(key: KeyEvent) -> Option<ManualInput> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return control_key_to_input(key.code);
    }
    match key.code {
        KeyCode::Char(ch) => {
            let mut bytes = Vec::new();
            if key.modifiers.contains(KeyModifiers::ALT) {
                bytes.push(0x1b);
            }
            let mut buf = [0_u8; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            Some(ManualInput::Bytes(bytes))
        }
        KeyCode::Enter => Some(ManualInput::Bytes(b"\r".to_vec())),
        KeyCode::Backspace => Some(ManualInput::Bytes(vec![0x7f])),
        KeyCode::Tab => Some(ManualInput::Bytes(b"\t".to_vec())),
        KeyCode::Esc => Some(ManualInput::Bytes(vec![0x1b])),
        KeyCode::Up => Some(ManualInput::Bytes(b"\x1b[A".to_vec())),
        KeyCode::Down => Some(ManualInput::Bytes(b"\x1b[B".to_vec())),
        KeyCode::Right => Some(ManualInput::Bytes(b"\x1b[C".to_vec())),
        KeyCode::Left => Some(ManualInput::Bytes(b"\x1b[D".to_vec())),
        KeyCode::Home => Some(ManualInput::Bytes(b"\x1b[H".to_vec())),
        KeyCode::End => Some(ManualInput::Bytes(b"\x1b[F".to_vec())),
        KeyCode::Delete => Some(ManualInput::Bytes(b"\x1b[3~".to_vec())),
        _ => None,
    }
}

fn control_key_to_input(code: KeyCode) -> Option<ManualInput> {
    match code {
        KeyCode::Char(']') => Some(ManualInput::Stop),
        KeyCode::Char('c') | KeyCode::Char('C') => Some(ManualInput::Bytes(vec![0x03])),
        KeyCode::Char('d') | KeyCode::Char('D') => Some(ManualInput::Bytes(vec![0x04])),
        KeyCode::Char('u') | KeyCode::Char('U') => Some(ManualInput::Bytes(vec![0x15])),
        KeyCode::Char('w') | KeyCode::Char('W') => Some(ManualInput::Bytes(vec![0x17])),
        KeyCode::Char('a') | KeyCode::Char('A') => Some(ManualInput::Bytes(vec![0x01])),
        KeyCode::Char('e') | KeyCode::Char('E') => Some(ManualInput::Bytes(vec![0x05])),
        KeyCode::Char('l') | KeyCode::Char('L') => Some(ManualInput::Bytes(vec![0x0c])),
        _ => None,
    }
}

fn manual_probe_log_path() -> PathBuf {
    if let Ok(path) = env::var("ORKIA_QA_MANUAL_LOG") {
        return PathBuf::from(path);
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    PathBuf::from("target").join(format!("orkia-codex-readiness-{stamp}.log"))
}

fn manual_probe_exit_path() -> PathBuf {
    env::var("ORKIA_QA_MANUAL_EXIT_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target").join("orkia-codex-readiness.stop"))
}

fn manual_probe_timeout() -> Duration {
    env::var("ORKIA_QA_MANUAL_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(300))
}

fn log_byte(log: &mut std::fs::File, source: &str, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    writeln!(log, "\n[{source}_bytes] len={} hex={hex}", bytes.len()).expect("write byte header");
    writeln!(log, "{text:?}").expect("write byte text");
}

fn log_readiness_state(
    log: &mut std::fs::File,
    elapsed: Duration,
    transcript: &str,
    snapshot: &str,
) {
    let prompt_in_transcript = transcript.contains(OPERATOR_QA_PROMPT);
    let prompt_in_snapshot = snapshot.contains(OPERATOR_QA_PROMPT);
    writeln!(
        log,
        "\n[readiness] elapsed_ms={} transcript_ready={} snapshot_ready={} transcript_trust={} snapshot_trust={} transcript_submit_accepted={} snapshot_submit_accepted={} prompt_in_transcript={} prompt_in_snapshot={}",
        elapsed.as_millis(),
        codex_ready_for_prompt(transcript),
        codex_ready_for_prompt(snapshot),
        codex_trust_prompt(transcript),
        codex_trust_prompt(snapshot),
        codex_submit_was_accepted(transcript),
        codex_submit_was_accepted(snapshot),
        prompt_in_transcript,
        prompt_in_snapshot,
    )
    .expect("write readiness");
    writeln!(log, "[visible_snapshot]\n{snapshot:?}").expect("write snapshot");
}
