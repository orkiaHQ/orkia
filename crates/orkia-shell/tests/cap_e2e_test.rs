// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! `Repl` through `tick`, so it exercises the whole path the unit tests don't:
//! classifier registration → dispatch → per-agent policy write.
//!
//! The cage *enforcement* of those bits (mount ro/omit on Linux, SBPL on
//! macOS, the shim exec gate) is covered by unit tests in `orkia-cage` /
//! `orkia-sh` and by the Linux manual-QA flow in `qa/linux/cap-classes.md`;
//! this test owns the **surface + storage** half of the gate.

use std::sync::{Arc, Mutex};

use orkia_shell::config::ShellConfig;
use orkia_shell::decision::BlockContent;
use orkia_shell::renderer::{PromptContext, RenderEvent, ShellRenderer};
use orkia_shell::{HeuristicClassifier, HeuristicRouter, Repl};
use orkia_shell_types::Policy;
use tempfile::TempDir;

#[derive(Default, Clone)]
struct CapturingRenderer {
    events: Arc<Mutex<Vec<RenderEvent>>>,
}

impl ShellRenderer for CapturingRenderer {
    fn publish(&mut self, event: RenderEvent) {
        self.events.lock().expect("renderer lock").push(event);
    }
    fn read_line(&mut self, _ctx: &PromptContext) -> Option<String> {
        None
    }
}

fn error_count(events: &Mutex<Vec<RenderEvent>>, needle: &str) -> usize {
    events
        .lock()
        .expect("events lock")
        .iter()
        .filter(|e| matches!(e, RenderEvent::Block(BlockContent::Error(s)) if s.contains(needle)))
        .count()
}

/// All rendered block text joined — for asserting on grid/detail output.
fn all_text(events: &Mutex<Vec<RenderEvent>>) -> String {
    events
        .lock()
        .expect("events lock")
        .iter()
        .filter_map(|e| match e {
            RenderEvent::Block(BlockContent::SystemInfo(s))
            | RenderEvent::Block(BlockContent::Text(s))
            | RenderEvent::Block(BlockContent::Error(s)) => Some(s.clone()),
            RenderEvent::Block(BlockContent::Notice { text, .. }) => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_agent(data_dir: &std::path::Path, name: &str) {
    let dir = data_dir.join("agents").join(name);
    std::fs::create_dir_all(&dir).expect("mkdir agent");
    std::fs::write(
        dir.join("agent.toml"),
        format!("[agent]\nname = \"{name}\"\narchetype = \"eng\"\n"),
    )
    .expect("write agent.toml");
}

fn boot(dir: &TempDir) -> (Repl, Arc<Mutex<Vec<RenderEvent>>>) {
    let mut config = ShellConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    config.hydrate_agents_from_dir();
    let renderer = CapturingRenderer::default();
    let events = renderer.events.clone();
    let repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, config);
    (repl, events)
}

fn read_caps(path: &std::path::Path) -> Policy {
    let raw = std::fs::read_to_string(path).expect("policy.toml exists");
    toml::from_str(&raw).expect("policy parses")
}

/// `cap @agent +read +exec` writes a per-agent policy.toml with exactly those
/// bits set; `cap @agent +write` then flips write on (read already on).
#[tokio::test]
async fn cap_mutate_writes_per_agent_caps() {
    let dir = TempDir::new().expect("tmp");
    write_agent(dir.path(), "faye");
    let (mut repl, events) = boot(&dir);

    repl.tick("cap @faye +read +exec".into())
        .await
        .expect("tick");
    let policy_path = dir.path().join("agents/faye/policy.toml");
    let p = read_caps(&policy_path);
    assert!(p.caps.read && p.caps.exec && !p.caps.write, "{:?}", p.caps);
    assert_eq!(error_count(&events, "cap"), 0, "no error on valid mutate");

    repl.tick("cap @faye +write".into()).await.expect("tick");
    let p = read_caps(&policy_path);
    assert!(p.caps.read && p.caps.write && p.caps.exec, "{:?}", p.caps);
}

/// `+write` without `read` is refused (dependency error) and writes nothing.
#[tokio::test]
async fn cap_write_requires_read() {
    let dir = TempDir::new().expect("tmp");
    write_agent(dir.path(), "rex");
    let (mut repl, events) = boot(&dir);

    repl.tick("cap @rex +write".into()).await.expect("tick");
    assert!(
        error_count(&events, "write requires read") >= 1,
        "expected dependency error"
    );
    assert!(
        !dir.path().join("agents/rex/policy.toml").exists(),
        "refused mutate must not create a policy file"
    );
}

/// `+spawn` / `+reach` are reserved frontier classes: refused, non-persisting.
#[tokio::test]
async fn cap_frontier_is_refused_and_never_persisted() {
    let dir = TempDir::new().expect("tmp");
    write_agent(dir.path(), "faye");
    let (mut repl, events) = boot(&dir);

    // Seed a real policy first so we can prove the refusal leaves it untouched.
    repl.tick("cap @faye +read +exec".into())
        .await
        .expect("tick");
    let policy_path = dir.path().join("agents/faye/policy.toml");
    let before = std::fs::read_to_string(&policy_path).expect("seeded");

    for op in ["+spawn", "+reach", "-spawn"] {
        repl.tick(format!("cap @faye {op}")).await.expect("tick");
    }
    assert!(
        error_count(&events, "frontier") >= 3,
        "each frontier op must refuse"
    );
    let after = std::fs::read_to_string(&policy_path).expect("still there");
    assert_eq!(before, after, "frontier refusal must not rewrite policy");
    // The frontier never lands in storage: caps stays a 3-field table.
    let p = read_caps(&policy_path);
    assert!(p.caps.read && p.caps.exec && !p.caps.write);
}

/// Unknown agent is a clean error, not a panic or a stray file.
#[tokio::test]
async fn cap_unknown_agent_errors() {
    let dir = TempDir::new().expect("tmp");
    let (mut repl, events) = boot(&dir);
    repl.tick("cap @ghost +read".into()).await.expect("tick");
    assert!(error_count(&events, "unknown agent") >= 1);
}

/// The read-only views (`cap` grid, `cap @agent` detail) render without error.
#[tokio::test]
async fn cap_views_render_without_error() {
    let dir = TempDir::new().expect("tmp");
    write_agent(dir.path(), "faye");
    let (mut repl, events) = boot(&dir);

    repl.tick("cap".into()).await.expect("tick grid");
    repl.tick("cap @faye".into()).await.expect("tick detail");
    assert_eq!(error_count(&events, "cap"), 0, "views must not error");
}

/// A `cap @a +…` mutation emits an auditable `cap.set` event (SEAL trail) with
/// the before/after caps — captured off the REPL's event channel.
#[tokio::test]
async fn cap_mutate_emits_cap_set_audit_event() {
    use orkia_shell::protocol::EventPayload;
    let dir = TempDir::new().expect("tmp");
    write_agent(dir.path(), "faye");
    let (mut repl, _events) = boot(&dir);
    let mut rx = repl.take_orkia_event_rx().expect("event rx");

    repl.tick("cap @faye +read +exec".into())
        .await
        .expect("tick");

    let mut data = None;
    while let Ok(evt) = rx.try_recv() {
        if let EventPayload::Custom { name, data: d } = &evt.event
            && name == "cap.set"
        {
            data = Some(d.clone());
        }
    }
    let data = data.expect("a cap.set audit event was emitted");
    assert_eq!(data["agent"], "faye");
    assert_eq!(data["after"]["read"], true);
    assert_eq!(data["after"]["exec"], true);
    assert_eq!(data["after"]["write"], false);
    assert_eq!(data["before"]["exec"], false);
}

/// An agent with no own policy shows the caps it *inherits* from the global
/// `[cage].policy` — the bits the cage would actually enforce — not the all-off
/// default. Exercises `effective_caps` → `Inherited` + `read_global_policy`.
#[tokio::test]
async fn cap_detail_shows_inherited_global_caps() {
    let dir = TempDir::new().expect("tmp");
    write_agent(dir.path(), "faye"); // deliberately no own policy.toml
    let global = dir.path().join("global-policy.toml");
    std::fs::write(
        &global,
        "default_verdict = \"ask\"\n[caps]\nread = true\nwrite = false\nexec = true\n\
         \n[workspace]\nroot = \".\"\n",
    )
    .expect("write global policy");

    let mut config = ShellConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    config.cage.policy = Some(global);
    config.hydrate_agents_from_dir();
    let renderer = CapturingRenderer::default();
    let events = renderer.events.clone();
    let mut repl = Repl::new(renderer, HeuristicClassifier, HeuristicRouter, config);

    repl.tick("cap @faye".into()).await.expect("tick detail");
    let out = all_text(&events);
    assert!(
        out.contains("inherited"),
        "source should be inherited; got:\n{out}"
    );
    assert!(
        out.contains("r-x··"),
        "MODE should reflect inherited read+exec (write off); got:\n{out}"
    );
    // Detail is read-only: it must NOT materialize a per-agent policy file.
    assert!(
        !dir.path().join("agents/faye/policy.toml").exists(),
        "inspecting must not create a policy file"
    );
}
