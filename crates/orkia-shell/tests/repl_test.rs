// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell::config::ShellConfig;
use orkia_shell::decision::BlockContent;
use orkia_shell::renderer::{PromptContext, RenderEvent, ShellRenderer};
use orkia_shell::{HeuristicClassifier, HeuristicRouter, Repl};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[derive(Default, Clone)]
struct TestRenderer {
    events: Arc<Mutex<Vec<RenderEvent>>>,
}

impl ShellRenderer for TestRenderer {
    fn publish(&mut self, event: RenderEvent) {
        self.events.lock().expect("lock").push(event);
    }
    fn read_line(&mut self, _ctx: &PromptContext) -> Option<String> {
        None // not used; tests drive `tick` directly
    }
}

fn cfg(dir: &TempDir) -> ShellConfig {
    ShellConfig {
        data_dir: dir.path().to_path_buf(),
        agents: vec![],
        agent_commands: std::collections::HashMap::new(),
        native_agents: Default::default(),
        default_shell: None,
        default_project: None,
        default_scope: None,
        default_mode: None,
        load_bashrc: None,
        load_profile: None,
        notification_verbosity: None,
        cage: Default::default(),
        daemon: Default::default(),
    }
}

#[tokio::test]
async fn background_shell_job_spawns() {
    // `cmd &` must spawn a real PTY-backed child and surface a
    // `JobSpawned` event — the previous "not supported in this
    // Asserts the rejection path is removed; lifecycle / reaping
    // tests live in tests/bg_shell_test.rs.
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    // `sleep 0.05` short enough not to slow the suite; & marks bg.
    repl.tick("!sleep 0.05 &".into()).await.expect("bg tick");

    let events = events.lock().expect("lock");
    let saw_rejection = events.iter().any(|e| {
        matches!(
            e,
            RenderEvent::Block(BlockContent::Error(t)) if t.contains("not supported")
        )
    });
    assert!(
        !saw_rejection,
        "background spawn must not be rejected; events: {events:?}",
    );
}

#[tokio::test]
async fn shell_passthrough_completes_cleanly() {
    // Used to also assert `repl.seal()` had ≥ 2 records (decision +
    // outcome). Those records were dropped when the SEAL chain was
    // scoped per-job/per-project: passthrough commands no longer
    // generate decision/outcome records on the global chain.
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick("!echo hello".into()).await.expect("ok");
}

#[tokio::test]
async fn builtin_help_produces_blocks() {
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick("orkia help".into()).await.expect("ok");

    let events = events.lock().expect("lock");
    let block_count = events
        .iter()
        .filter(|e| matches!(e, RenderEvent::Block(_)))
        .count();
    assert!(
        block_count >= 3,
        "help should emit multiple blocks, got {block_count}"
    );
}

#[tokio::test]
async fn empty_input_is_noop() {
    // Empty / whitespace-only ticks must complete without panicking
    // and without spawning any side effects. Previously also
    // asserted the SEAL chain length was 0 — with scoped chains,
    // no global counter exists.
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));
    repl.tick("".into()).await.expect("ok");
    repl.tick("   ".into()).await.expect("ok");
}

#[tokio::test]
async fn agent_prefix_produces_agent_started() {
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick("@faye fix the bug".into()).await.expect("ok");

    let events = events.lock().expect("lock");
    let saw_started = events.iter().any(|e| matches!(
        e,
        RenderEvent::Block(BlockContent::SystemInfo(t)) if t.contains("faye") && t.contains("spawned")
    ));
    assert!(
        saw_started,
        "expected AgentStarted system-info, got events: {events:?}"
    );
}

#[tokio::test]
async fn brush_cd_persists_across_ticks() {
    // Proves brush IS the shell: cd in one tick changes the cwd that the
    // next tick (and the prompt) sees. Was impossible under `zsh -c` per
    // command, which always started in $PWD.
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    let target = tempfile::tempdir().expect("target");
    let target_path = target.path().to_owned();
    let cd_line = format!("!cd {}", target_path.display());

    repl.tick(cd_line).await.expect("cd ok");
    repl.tick("!pwd".into()).await.expect("pwd ok");

    let want_leaf = target_path
        .file_name()
        .expect("leaf")
        .to_string_lossy()
        .into_owned();
    let events = events.lock().expect("lock");
    let saw_cwd = events.iter().any(|e| {
        matches!(
            e,
            RenderEvent::Block(BlockContent::Text(t)) if t.contains(&want_leaf)
        )
    });
    assert!(
        saw_cwd,
        "expected pwd output to contain {want_leaf:?}, got {events:?}"
    );
}

#[tokio::test]
async fn brush_exit_builtin_completes_cleanly() {
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));
    // `exit` is a brush builtin — the path must complete without
    // panicking. The actual REPL loop exit on `should_exit=true`
    // is unit-tested in engine_test.rs. (Previously also asserted
    // SEAL captured decision+outcome — no longer applicable; see
    repl.tick("!exit 0".into()).await.expect("exit ok");
}

// NOTE (brush-engine migration): the prior `background_shell_returns_job_spawned`
// / `orkia_ps_shows_running_job` / `orkia_stop_kills_job` tests exercised
// `cmd &` shell backgrounding through `JobController::spawn_shell`. That
// code path is removed in this build — brush runs shell commands in-process,
// and only agent jobs flow through `JobController` now. ps/stop are still
// shell-backgrounded coverage back.

#[tokio::test]
async fn multi_agent_pipeline_requires_team_in_solo() {
    // `@a | @b` is rejected when no `AgentPipelineCoordinator` is
    // wired (the default in the OSS shell). The error message tells
    // the user a coordinator is required.
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick("@a do X | @b review".into()).await.expect("ok");

    let events = events.lock().expect("lock");
    let saw_team_msg = events.iter().any(|e| {
        matches!(
            e,
            RenderEvent::Block(BlockContent::Error(t)) if t.contains("Orkia Team")
        )
    });
    assert!(
        saw_team_msg,
        "expected Team-required error, got events: {events:?}"
    );
}

/// Collect the text of every emitted block (Text / SystemInfo / Error).
fn block_texts(events: &[RenderEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            RenderEvent::Block(BlockContent::Text(t))
            | RenderEvent::Block(BlockContent::SystemInfo(t))
            | RenderEvent::Block(BlockContent::Error(t)) => Some(t.clone()),
            RenderEvent::Block(BlockContent::TableRow(cells)) => Some(
                cells
                    .iter()
                    .map(|c| c.text.as_str())
                    .collect::<Vec<_>>()
                    .join("  "),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn exec_from_json_bridge_filters_records() {
    // echo (external) → from json (converter) → where (typed filter).
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick(
        "echo '[{\"status\":\"Running\"},{\"status\":\"Stopped\"}]' | from json | where status == Running"
            .into(),
    )
    .await
    .expect("tick");

    let events = events.lock().expect("lock");
    let texts = block_texts(&events).join("\n");
    assert!(
        texts.contains("Running"),
        "expected Running row; got: {texts}"
    );
    assert!(
        !texts.contains("Stopped"),
        "Stopped must be filtered out; got: {texts}"
    );
}

#[tokio::test]
async fn exec_bytes_into_where_is_type_mismatch() {
    // echo bytes straight into a structured command → fail-closed refusal.
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick("echo '{\"a\":1}' | where a == 1".into())
        .await
        .expect("tick");

    let events = events.lock().expect("lock");
    let saw_mismatch = events.iter().any(|e| {
        matches!(
            e,
            RenderEvent::Block(BlockContent::Error(t)) if t.contains("type mismatch")
        )
    });
    assert!(
        saw_mismatch,
        "expected a type mismatch error; got: {events:?}"
    );
}

#[tokio::test]
async fn exec_namespaced_ls_lists_directory() {
    // `orkia ls <dir>` is the typed producer; bare `ls` would stay POSIX.
    let dir = TempDir::new().expect("tmp");
    std::fs::write(dir.path().join("marker.txt"), b"hello").expect("write");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick(format!("orkia ls {}", dir.path().display()))
        .await
        .expect("tick");

    let events = events.lock().expect("lock");
    let texts = block_texts(&events).join("\n");
    assert!(
        texts.contains("marker.txt"),
        "expected the file listed; got: {texts}"
    );
}

#[tokio::test]
async fn exec_agent_piped_to_command_is_type_mismatch() {
    // TypeMismatch (agent emits ByteStream), not the old AgentOnLeft rule.
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick("@faye | where status == working".into())
        .await
        .expect("tick");

    let events = events.lock().expect("lock");
    let saw_mismatch = events.iter().any(|e| {
        matches!(
            e,
            RenderEvent::Block(BlockContent::Error(t))
                if t.contains("type mismatch") && t.contains("@faye")
        )
    });
    assert!(
        saw_mismatch,
        "expected agent->command TypeMismatch; got: {events:?}"
    );
}

#[tokio::test]
async fn exec_typed_into_external_grep_runs_end_to_end() {
    // `orkia ls <dir> | where size > 1kb | grep big` — the typed output is
    // serialized to TSV and streamed into grep's stdin (Value → Bytes sink).
    let dir = TempDir::new().expect("tmp");
    std::fs::write(dir.path().join("bigfile"), vec![0u8; 2048]).expect("write");
    std::fs::write(dir.path().join("smallfile"), b"x").expect("write");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick(format!(
        "orkia ls {} | where size > 1kb | grep big",
        dir.path().display()
    ))
    .await
    .expect("tick");

    let texts = block_texts(&events.lock().expect("lock")).join("\n");
    assert!(
        texts.contains("bigfile"),
        "grep should surface bigfile; got: {texts}"
    );
    assert!(
        !texts.contains("smallfile"),
        "smallfile filtered by where; got: {texts}"
    );
}

#[tokio::test]
async fn exec_early_termination_through_external_does_not_hang() {
    // `orkia ls <dir> | first 1 | cat` — if stdin EOF were not sent, cat would
    // block forever and this test would time out. Completing proves the sink
    // closed stdin after the early-terminated stream.
    let dir = TempDir::new().expect("tmp");
    for i in 0..20 {
        std::fs::write(dir.path().join(format!("f{i:02}")), b"data").expect("write");
    }
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick(format!("orkia ls {} | first 1 | cat", dir.path().display()))
        .await
        .expect("tick completes (no hang)");

    let rows: usize = block_texts(&events.lock().expect("lock"))
        .iter()
        .map(|t| {
            t.lines()
                .filter(|l| l.contains("f0") || l.contains("f1"))
                .count()
        })
        .sum();
    assert_eq!(rows, 1, "cat received exactly one row then EOF");
}

#[tokio::test]
async fn exec_display_streams_all_rows_across_chunks() {
    // More rows than DISPLAY_CHUNK_ROWS (256): the chunked display sink must
    // emit every row exactly once (header appears once). List a dedicated
    // subdir so the REPL's own data-dir files don't pollute the listing.
    let dir = TempDir::new().expect("tmp");
    let listme = dir.path().join("listme");
    std::fs::create_dir(&listme).expect("mkdir");
    for i in 0..300 {
        std::fs::write(listme.join(format!("file{i:03}")), b"x").expect("write");
    }
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick(format!("orkia ls {}", listme.display()))
        .await
        .expect("tick");

    let texts = block_texts(&events.lock().expect("lock"));
    let row_count: usize = texts
        .iter()
        .map(|t| t.lines().filter(|l| l.contains("file")).count())
        .sum();
    assert_eq!(
        row_count, 300,
        "every row emitted exactly once across chunks"
    );
}

#[tokio::test]
async fn exec_prefix_and_suffix_both_boundaries() {
    // echo (Bytes→Value prefix) | from json | where | grep (Value→Bytes suffix):
    // both conversion boundaries active in one pipeline.
    let dir = TempDir::new().expect("tmp");
    let renderer = TestRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, cfg(&dir));

    repl.tick(
        "echo '[{\"k\":\"keep\"},{\"k\":\"drop\"}]' | from json | where k == keep | grep keep"
            .into(),
    )
    .await
    .expect("tick");

    let texts = block_texts(&events.lock().expect("lock")).join("\n");
    assert!(
        texts.contains("keep"),
        "expected keep row through grep; got: {texts}"
    );
    assert!(
        !texts.contains("drop"),
        "drop filtered by where; got: {texts}"
    );
}
