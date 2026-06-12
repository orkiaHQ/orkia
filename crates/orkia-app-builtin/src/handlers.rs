// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0 — see the top-level LICENSE file for
// the Elastic License 2.0 terms.

//! Pure-logic handlers for `orkia app *`. Returns `Vec<BlockContent>` so
//! the REPL can hand the output to the renderer without touching disk
//! itself beyond what the helpers below do.

use std::path::{Path, PathBuf};

use orkia_shell_types::{BlockContent, JobInfo, JobKind};

use crate::discover::{self, seal_event_count};

/// Information the REPL needs to spawn the viewer for an app.
///
/// We keep this small and serializable: the actual `Command` is built in
/// the shell crate because `JobController` is what owns the child.
#[derive(Debug, Clone)]
pub struct AppRunSpec {
    pub app_name: String,
    pub app_dir: PathBuf,
    pub bundle_id: String,
}

pub fn list(root: &Path, jobs: &[JobInfo]) -> Vec<BlockContent> {
    let apps = match discover::discover_all(root) {
        Ok(a) => a,
        Err(e) => return vec![BlockContent::Error(format!("app list: {e}"))],
    };
    if apps.is_empty() {
        return vec![BlockContent::SystemInfo("no forge apps installed".into())];
    }
    let mut blocks = Vec::new();
    blocks.push(BlockContent::SystemInfo(format!(
        " {:<18} {:<8} {:<10} {:<20}",
        "NAME", "VERSION", "STATUS", "RFC"
    )));
    for app in &apps {
        let status = if is_running(&app.name, jobs) {
            "running"
        } else {
            "idle"
        };
        blocks.push(BlockContent::Text(format!(
            " {:<18} {:<8} {:<10} {:<20}",
            truncate(&app.name, 18),
            truncate(&app.manifest.forge.version, 8),
            status,
            truncate(&app.manifest.forge.rfc_id, 20),
        )));
    }
    blocks
}

pub fn prepare_run(root: &Path, name: &str) -> Result<AppRunSpec, Vec<BlockContent>> {
    let app = match discover::load_app(name, root) {
        Ok(a) => a,
        Err(e) => return Err(vec![BlockContent::Error(format!("app run: {e}"))]),
    };
    Ok(AppRunSpec {
        app_name: app.name.clone(),
        app_dir: app.dir.clone(),
        bundle_id: format!("orkia.forge.{}", app.name),
    })
}

pub fn edit(root: &Path, name: &str) -> Result<PathBuf, Vec<BlockContent>> {
    match discover::load_app(name, root) {
        Ok(a) => Ok(a.dir),
        Err(e) => Err(vec![BlockContent::Error(format!("app edit: {e}"))]),
    }
}

/// Confirm-and-delete. `confirm` must equal `name` for the deletion to
/// proceed; otherwise we render a preview and ask the user to re-run with
pub fn remove(root: &Path, name: &str, confirm: Option<&str>) -> Vec<BlockContent> {
    let app = match discover::load_app(name, root) {
        Ok(a) => a,
        Err(e) => return vec![BlockContent::Error(format!("app remove: {e}"))],
    };
    match confirm {
        Some(c) if c == name => match std::fs::remove_dir_all(&app.dir) {
            Ok(()) => vec![BlockContent::SystemInfo(format!(
                "removed {}",
                app.dir.display()
            ))],
            Err(e) => vec![BlockContent::Error(format!("app remove: {e}"))],
        },
        _ => {
            let mut blocks = Vec::new();
            blocks.push(BlockContent::SystemInfo(
                "this will permanently delete:".into(),
            ));
            blocks.push(BlockContent::SystemInfo(format!("  {}", app.dir.display())));
            blocks.push(BlockContent::SystemInfo(format!(
                "  including stored data and SEAL records ({} events)",
                seal_event_count(&app)
            )));
            blocks.push(BlockContent::SystemInfo(format!(
                "re-run with `--confirm {name}` to authorize"
            )));
            blocks
        }
    }
}

pub fn inspect(root: &Path, name: &str, jobs: &[JobInfo]) -> Vec<BlockContent> {
    let app = match discover::load_app(name, root) {
        Ok(a) => a,
        Err(e) => return vec![BlockContent::Error(format!("app inspect: {e}"))],
    };
    let mut blocks = Vec::new();
    push_kv(&mut blocks, "name", &app.manifest.forge.name);
    push_kv(&mut blocks, "version", &app.manifest.forge.version);
    push_kv(
        &mut blocks,
        "api_version",
        &app.manifest.forge.api_version.to_string(),
    );
    push_kv(
        &mut blocks,
        "rfc",
        &format!(
            "{} ({})",
            app.manifest.forge.rfc_id, app.manifest.forge.rfc_hash
        ),
    );
    push_kv(
        &mut blocks,
        "created",
        &app.manifest.forge.created_at.to_rfc3339(),
    );
    push_kv(&mut blocks, "path", &app.dir.display().to_string());
    push_kv(
        &mut blocks,
        "window",
        &format!(
            "{} x {}, title \"{}\"",
            app.manifest.forge.window.width,
            app.manifest.forge.window.height,
            app.manifest.forge.window.title,
        ),
    );
    push_kv(
        &mut blocks,
        "permissions",
        &format!(
            "storage:{} agent:{} network:{:?} notif:{}",
            yn(app.manifest.forge.permissions.storage),
            yn(app.manifest.forge.permissions.agent),
            app.manifest.forge.permissions.network,
            yn(app.manifest.forge.permissions.notification),
        ),
    );
    let status = if is_running(&app.name, jobs) {
        "running"
    } else {
        "idle"
    };
    push_kv(&mut blocks, "status", status);
    push_kv(
        &mut blocks,
        "seal events",
        &format!("{} (app-local)", seal_event_count(&app)),
    );
    blocks
}

fn push_kv(blocks: &mut Vec<BlockContent>, k: &str, v: &str) {
    blocks.push(BlockContent::Text(format!("  {k:<14} {v}")));
}

fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn is_running(name: &str, jobs: &[JobInfo]) -> bool {
    jobs.iter()
        .any(|j| matches!(&j.kind, JobKind::ForgeApp { app_name } if app_name == name))
}

/// V2: `orkia app perms <name>` — show permissions from manifest.
pub fn perms(root: &Path, name: &str) -> Vec<BlockContent> {
    let app = match discover::load_app(name, root) {
        Ok(a) => a,
        Err(e) => return vec![BlockContent::Error(format!("app perms: {e}"))],
    };
    let p = &app.manifest.forge.permissions;
    let mut blocks = Vec::new();
    blocks.push(BlockContent::Text(format!("  app:           {}", app.name)));
    blocks.push(BlockContent::Text(
        "  permissions (from manifest.toml):".into(),
    ));
    blocks.push(BlockContent::Text(format!(
        "    storage:       {}",
        if p.storage { "yes" } else { "no" }
    )));
    blocks.push(BlockContent::Text(format!(
        "    agent:         {}",
        if p.agent { "yes" } else { "no" }
    )));
    if p.network.is_empty() {
        blocks.push(BlockContent::Text(
            "    network:       0 domains (denied)".into(),
        ));
    } else {
        blocks.push(BlockContent::Text(format!(
            "    network:       {} domain(s) allowed",
            p.network.len()
        )));
        for d in &p.network {
            blocks.push(BlockContent::Text(format!("      • {d}")));
        }
    }
    blocks.push(BlockContent::Text(format!(
        "    notification:  {}",
        if p.notification { "yes" } else { "no" }
    )));
    blocks
}

/// V2: `orkia app seal <name> [--since 1h] [--verify]` — render SEAL chain.
///
/// interface to **Forge App Provenance** (ledger #3 in `SEAL-FAMILY.md`).
/// We emit a one-line clarifier so callers conflating it with SEAL v1
/// audit get redirected; the rest of the output is unchanged.
pub fn seal(root: &Path, name: &str, since: Option<&str>, verify: bool) -> Vec<BlockContent> {
    tracing::info!(
        "`orkia app seal` inspects the Forge App Provenance ledger. For SEAL v1 \
         compliance documents see `orkia rfc seal <slug>`."
    );
    let app = match discover::load_app(name, root) {
        Ok(a) => a,
        Err(e) => return vec![BlockContent::Error(format!("app seal: {e}"))],
    };
    let seal_dir = app.dir.join("seal");
    let mut blocks = Vec::new();
    blocks.push(BlockContent::SystemInfo(
        "Note: `app seal` reads Forge App Provenance (ledger #3). SEAL v1 \
         compliance documents come from `orkia rfc seal <slug>`."
            .into(),
    ));
    blocks.push(BlockContent::Text(format!("  app:           {name}")));
    if verify {
        match orkia_forge_seal::verify_chain(&seal_dir) {
            Ok(r) => {
                blocks.push(BlockContent::Text(format!(
                    "  chain:         {} events verified",
                    r.events
                )));
                blocks.push(BlockContent::Text(format!(
                    "  last_hash:     {}",
                    r.last_hash
                )));
            }
            Err(e) => {
                blocks.push(BlockContent::Error(format!("  verify failed: {e}")));
            }
        }
        return blocks;
    }

    let events_path = seal_dir.join("events.jsonl");
    let raw = match std::fs::read_to_string(&events_path) {
        Ok(r) => r,
        Err(e) => {
            blocks.push(BlockContent::Error(format!("  read: {e}")));
            return blocks;
        }
    };
    let cutoff = since.and_then(parse_since);
    let mut shown = 0u32;
    for line in raw.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<orkia_forge_seal::SealRecord>(line) {
            if let Some(c) = cutoff
                && rec.ts < c
            {
                continue;
            }
            blocks.push(BlockContent::Text(format!(
                "  #{:<3} {:<28} {}",
                rec.id,
                rec.kind,
                rec.ts.format("%Y-%m-%d %H:%M:%S")
            )));
            shown += 1;
            if shown >= 20 {
                break;
            }
        }
    }
    if shown == 0 {
        blocks.push(BlockContent::Text("  (no events in range)".into()));
    }
    blocks
}

fn parse_since(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // Accept e.g. "1h", "30m", "2d".
    let (n_str, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = n_str.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return None,
    };
    Some(chrono::Utc::now() - chrono::Duration::seconds(secs))
}

/// V2: `orkia app agent <name>` — show agent state from local archetype +
/// (optionally) server-side stats. The server-side fetch lands in the
/// shell handler; this pure-logic version only reads what's on disk.
pub fn agent(root: &Path, name: &str) -> Vec<BlockContent> {
    let app = match discover::load_app(name, root) {
        Ok(a) => a,
        Err(e) => return vec![BlockContent::Error(format!("app agent: {e}"))],
    };
    let archetype_path = app.dir.join("agent").join("archetype.toml");
    let mut blocks = Vec::new();
    blocks.push(BlockContent::Text(format!("  app:            {name}")));
    if !archetype_path.exists() {
        blocks.push(BlockContent::Text("  agent defined:  no".into()));
        blocks.push(BlockContent::SystemInfo(
            "this app does not have an embedded agent.".into(),
        ));
        blocks.push(BlockContent::SystemInfo(
            "add [forge.agent] to the RFC and rebuild with --rerun to add one.".into(),
        ));
        return blocks;
    }
    let raw = match std::fs::read_to_string(&archetype_path) {
        Ok(r) => r,
        Err(e) => {
            blocks.push(BlockContent::Error(format!("  read: {e}")));
            return blocks;
        }
    };
    // The archetype TOML is wrapped in `[agent]` per write_agent_scaffold.
    #[derive(serde::Deserialize)]
    struct Wrap {
        agent: orkia_rfc_core::frontmatter::ForgeAgentBlock,
    }
    let wrap: Wrap = match toml::from_str(&raw) {
        Ok(w) => w,
        Err(e) => {
            blocks.push(BlockContent::Error(format!("  parse: {e}")));
            return blocks;
        }
    };
    let a = &wrap.agent;
    blocks.push(BlockContent::Text("  agent defined:  yes".into()));
    blocks.push(BlockContent::Text(format!(
        "  archetype:      {}",
        a.archetype
    )));
    if let Some(model) = &a.model {
        blocks.push(BlockContent::Text(format!("  model:          {model}")));
    }
    if let Some(desc) = &a.description {
        blocks.push(BlockContent::Text(format!("  description:    {desc}")));
    }
    if let Some(rate) = a.max_invocations_per_hour {
        blocks.push(BlockContent::Text(format!("  rate limit:     {rate}/hour")));
    }
    if let Some(cost) = a.max_cost_cents_per_invocation {
        blocks.push(BlockContent::Text(format!(
            "  cost ceiling:   {cost}¢/invocation"
        )));
    }
    blocks.push(BlockContent::Text("  tools enabled:".into()));
    if a.tools.fetch {
        blocks.push(BlockContent::Text(
            "    • fetch (constrained to app's network whitelist)".into(),
        ));
    }
    if !a.tools.fetch {
        blocks.push(BlockContent::Text("    (none)".into()));
    }
    blocks.push(BlockContent::SystemInfo(
        "(invocation history + trust score requires backend; see `orkia app usage`)".into(),
    ));
    blocks
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use orkia_forge_types::{ForgeConfig, ForgeManifest, Permissions, WindowConfig};
    use std::fs;
    use tempfile::TempDir;

    fn write_app(root: &Path, name: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let m = ForgeManifest {
            forge: ForgeConfig {
                name: name.into(),
                description: String::new(),
                version: "0.1.0".into(),
                api_version: 1,
                rfc_id: name.into(),
                rfc_hash: "sha256:0".into(),
                created_at: Utc::now(),
                icon: "default".into(),
                window: WindowConfig {
                    title: "T".into(),
                    width: 480,
                    height: 320,
                    resizable: true,
                },
                permissions: Permissions::default(),
            },
        };
        m.save(&dir.join("manifest.toml")).unwrap();
    }

    #[test]
    fn list_when_empty() {
        let tmp = TempDir::new().unwrap();
        let out = list(tmp.path(), &[]);
        assert!(matches!(out[0], BlockContent::SystemInfo(_)));
    }

    #[test]
    fn list_with_apps() {
        let tmp = TempDir::new().unwrap();
        write_app(tmp.path(), "hello");
        let out = list(tmp.path(), &[]);
        // header + one row.
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn remove_without_confirm_renders_preview() {
        let tmp = TempDir::new().unwrap();
        write_app(tmp.path(), "hello");
        let out = remove(tmp.path(), "hello", None);
        assert!(tmp.path().join("hello").exists());
        let txt = format!("{out:?}");
        assert!(txt.contains("--confirm"));
    }

    #[test]
    fn remove_with_wrong_confirm_renders_preview() {
        let tmp = TempDir::new().unwrap();
        write_app(tmp.path(), "hello");
        let out = remove(tmp.path(), "hello", Some("typo"));
        assert!(tmp.path().join("hello").exists());
        let txt = format!("{out:?}");
        assert!(txt.contains("--confirm"));
    }

    #[test]
    fn remove_with_matching_confirm_deletes() {
        let tmp = TempDir::new().unwrap();
        write_app(tmp.path(), "hello");
        let out = remove(tmp.path(), "hello", Some("hello"));
        assert!(!tmp.path().join("hello").exists());
        let txt = format!("{out:?}");
        assert!(txt.contains("removed"));
    }

    #[test]
    fn inspect_reports_fields() {
        let tmp = TempDir::new().unwrap();
        write_app(tmp.path(), "hello");
        let out = inspect(tmp.path(), "hello", &[]);
        let txt = format!("{out:?}");
        assert!(txt.contains("hello"));
        assert!(txt.contains("0.1.0"));
        assert!(txt.contains("storage:yes"));
    }

    #[test]
    fn prepare_run_returns_spec() {
        let tmp = TempDir::new().unwrap();
        write_app(tmp.path(), "hello");
        let spec = prepare_run(tmp.path(), "hello").unwrap();
        assert_eq!(spec.app_name, "hello");
        assert_eq!(spec.bundle_id, "orkia.forge.hello");
    }
}
