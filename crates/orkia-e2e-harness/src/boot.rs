// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Session boot helpers: crontab shim, agent/project seeding, shell spawn.
//!
//! These are the private setup routines called by `OrkiaSession::from_compose`
//! and `reset_for_next_flow`. Extracted here to keep `session.rs` ≤600 lines.

use std::path::{Path, PathBuf};

use orkia_test_harness::{JournalTail, OrkiaBinary, OrkiaProcess, OrkiaSandbox, pty::PtyShape};

use crate::session::ShellSession;

/// Default faye script content. Flows that need richer agent behavior
/// (F004 stdin sink, F005 long-lived agent) overwrite it for the
/// duration of that flow; `reset_for_next_flow` restores this baseline.
pub(crate) const DEFAULT_FAYE_SCRIPT: &str = r#"name: faye-e2e
raw_mode: false
steps:
  - kind: print
    text: "faye: hello\n"
  - kind: osc133
    marker: prompt_start
  - kind: exit
    code: 0
"#;

/// Spool path for the isolated test crontab, derived from the sandbox
/// data dir. The `crontab` shim reads/writes this file instead of the
/// system crontab.
pub(crate) fn crontab_spool_path(data_dir: &Path) -> PathBuf {
    data_dir.join("test-crontab-spool")
}

/// Write a `crontab` shim into `<home>/bin/` that redirects to a sandbox
/// spool file. Orkia calls `Command::new("crontab")`, so prepending
/// `<home>/bin` to PATH makes that resolve here instead of the real
/// `/usr/bin/crontab` — isolating `every` from the host's crontab. Only
/// the storage backend is redirected; Orkia's cron-line generation, tags,
/// and pause/resume logic run unchanged. Returns the `<home>/bin` dir.
fn setup_crontab_shim(home: &Path) -> std::io::Result<PathBuf> {
    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let shim = bin_dir.join("crontab");
    // Implements the crontab(1) subset orkia uses (crontab.rs:56,87):
    //   -l        → print spool (exit 1 + "no crontab" stderr when empty,
    //               which orkia's load path treats as an empty crontab)
    //   -         → overwrite spool from stdin
    //   -r        → remove spool
    //   <file>    → install spool from file
    let script = r#"#!/bin/sh
spool="${ORKIA_TEST_CRONTAB_SPOOL:-$HOME/.orkia/test-crontab-spool}"
case "$1" in
  -l)
    if [ -s "$spool" ]; then cat "$spool"; else echo "no crontab for test-user" >&2; exit 1; fi ;;
  -)
    mkdir -p "$(dirname "$spool")"; cat > "$spool" ;;
  -r)
    rm -f "$spool" ;;
  *)
    if [ -n "$1" ] && [ "${1#-}" = "$1" ]; then
      mkdir -p "$(dirname "$spool")"; cat "$1" > "$spool"
    else
      echo "orkia-test-crontab: unsupported args: $*" >&2; exit 2
    fi ;;
esac
"#;
    std::fs::write(&shim, script)?;
    let mut perms = std::fs::metadata(&shim)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&shim, perms)?;
    Ok(bin_dir)
}

pub(crate) fn try_start_shell(env_spec: &crate::env::FlowEnv) -> Option<ShellSession> {
    let plan = env_spec.plan.as_env_value();
    let bin = match OrkiaBinary::resolve(false) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("orkia binary not resolved, shell features disabled: {e}");
            return None;
        }
    };
    let sandbox = match OrkiaSandbox::new() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("OrkiaSandbox::new failed: {e}");
            return None;
        }
    };

    if let Err(e) = seed_default_project(sandbox.data_dir().as_path()) {
        tracing::warn!("failed to seed default project: {e}");
        return None;
    }

    // Crontab isolation: shim on PATH + spool inside the sandbox.
    let bin_dir = match setup_crontab_shim(sandbox.home()) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("crontab shim setup failed: {e}");
            return None;
        }
    };
    let shim_path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let spool = crontab_spool_path(sandbox.data_dir().as_path());
    let spool_str = spool.display().to_string();
    // Faye agent → orkia-fake-agent. Best-effort: if the fake-agent
    // binary isn't built, F003-style flows that exercise delegate will
    // surface a clear "no command configured for faye" failure.
    if let Ok(fake) = OrkiaBinary::resolve_fake_agent(false) {
        // Pre-seed BOTH faye and sage with the default script.
        // orkia's `hydrate_agents_from_dir` only runs at startup, so
        // every agent we'll ever spawn must exist before the orkia
        // process boots. The per-flow script content (keepalive,
        // crash, etc.) gets written by `seed_agent_with_script`
        // mid-flow — orkia keeps using the boot-time agent.toml
        // command/args (which point at the script file path), and
        // orkia-fake-agent re-reads script.yaml from disk on each
        // spawn, so mid-flow content changes take effect.
        for name in ["faye", "sage"] {
            if let Err(e) = seed_agent_files(sandbox.data_dir().as_path(), name, &fake) {
                tracing::warn!("failed to seed {name} agent: {e}");
            }
        }
    }

    // Real login against the compose backend: log the per-plan fixture
    // account in, persist the verified (signed-JWT) session to a file the
    // shell reads via `ORKIA_SESSION_FILE`. No client-side plan assertion —
    // the plan comes from the backend's `organization.billing_plan`. If the
    // backend is unreachable the shell still boots, unauthenticated (Free).
    let backend_url =
        std::env::var("ORKIA_BACKEND_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let session_file = sandbox.data_dir().join("session.toml");
    let session_file_str = session_file.display().to_string();
    if let Err(e) = crate::login::login_to_session_file(
        &backend_url,
        env_spec.plan.fixture_email(),
        &session_file,
    ) {
        tracing::warn!(plan, "real login failed, shell boots unauthenticated: {e}");
    }

    // `EDITOR=true` makes `rfc create` / `rfc edit` no-op the editor spawn
    // (the Unix `true` binary exits 0 immediately so the RFC keeps its
    // scaffolded body).
    let mut env: Vec<(&str, &str)> = vec![
        ("ORKIA_BACKEND_URL", backend_url.as_str()),
        ("ORKIA_SESSION_FILE", session_file_str.as_str()),
        ("EDITOR", "true"),
        ("VISUAL", "true"),
        ("PATH", shim_path.as_str()),
        ("ORKIA_TEST_CRONTAB_SPOOL", spool_str.as_str()),
    ];
    // Per-flow extra env (e.g. ORKIA_SCHEDULED=1 for F502).
    for (k, v) in &env_spec.extra_env {
        env.push((k.as_str(), v.as_str()));
    }
    let process = match OrkiaProcess::spawn(&bin, &sandbox, &[], &env, PtyShape::default()) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("OrkiaProcess::spawn failed: {e}");
            return None;
        }
    };
    let journal = match JournalTail::start(sandbox.journal_path()) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!("JournalTail::start failed: {e}");
            return None;
        }
    };
    let data_dir = sandbox.data_dir();
    Some(ShellSession {
        sandbox,
        process,
        journal,
        data_dir,
    })
}

/// E2E default project — must match the `--project` arg used by flows.
pub const DEFAULT_PROJECT: &str = "default-project";

fn seed_default_project(data_dir: &Path) -> std::io::Result<()> {
    let proj = data_dir.join("projects").join(DEFAULT_PROJECT);
    std::fs::create_dir_all(proj.join("rfcs"))?;
    std::fs::create_dir_all(proj.join("issues"))?;
    // Static timestamp — the sandbox is rebuilt per-test, time doesn't matter.
    let toml = format!(
        "[project]\nname = \"{}\"\ndescription = \"e2e test project\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n\n[agents]\nassigned = []\n",
        DEFAULT_PROJECT,
    );
    std::fs::write(proj.join("project.toml"), toml)?;
    Ok(())
}

pub(crate) fn agent_script_path(data_dir: &Path, name: &str) -> PathBuf {
    data_dir.join("agents").join(name).join("script.yaml")
}

pub(crate) fn restore_default_faye_script(data_dir: &Path) -> std::io::Result<()> {
    // Restore EVERY pre-seeded agent's script back to the default —
    // each flow may rewrite any subset of them.
    for name in ["faye", "sage"] {
        let path = agent_script_path(data_dir, name);
        if path.parent().map(|p| p.exists()).unwrap_or(false) {
            std::fs::write(&path, DEFAULT_FAYE_SCRIPT)?;
        }
    }
    Ok(())
}

fn seed_agent_files(data_dir: &Path, name: &str, fake_agent_bin: &Path) -> std::io::Result<()> {
    let agent_dir = data_dir.join("agents").join(name);
    std::fs::create_dir_all(&agent_dir)?;
    let script_path = agent_dir.join("script.yaml");
    std::fs::write(&script_path, DEFAULT_FAYE_SCRIPT)?;
    // under `[runtime]`. Without that section the loader falls back to
    // `command = "claude"` and `args = []`, which silently launches
    // Claude Code from $PATH instead of the fake agent.
    let toml = format!(
        "[agent]\nname = \"{name}\"\n\n[runtime]\ncommand = \"{}\"\nargs = [\"--script\", \"{}\"]\n",
        fake_agent_bin.display(),
        script_path.display(),
    );
    std::fs::write(agent_dir.join("agent.toml"), toml)?;
    Ok(())
}
