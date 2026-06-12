// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Interactive wizards for `orkia setup`.
//!
//! Two entry points: [`fresh_setup`] (no `~/.orkia/` or `--force`) walks
//! the user through registry sync → RC migration → CLI detection →
//! agent selection → optional project. [`relaunch_menu`] is the
//! follow-up surface that lets the user add more agents/projects or
//! re-run subflows without nuking config.

use std::path::{Path, PathBuf};

use orkia_builtin::migrate_rc::{MigrateRcOpts, run_migration};
use orkia_shell::agent_dir;
use orkia_shell_types::Workspace;

use super::SetupArgs;
use super::builtins::builtin_archetypes;
use super::prompts::{ask, ask_select, ask_yn};
use super::registry::{ArchetypeMeta, ArchetypeRegistry};
use super::scaffold::scaffold_agent;
use super::tools::DetectedTools;

pub fn fresh_setup(home: &Path, orkia_dir: &Path, args: &SetupArgs) {
    banner("orkia — first-time setup");

    let registry = ArchetypeRegistry::new(orkia_dir);
    sync_registry_step(&registry, args.offline);
    let migrate_summary = migrate_rc_step(home);
    let tools = detect_tools_step();
    let ctx = ArchetypeCtx {
        orkia_dir,
        registry: &registry,
        tools: &tools,
        offline: args.offline,
    };
    let scaffolded = pick_and_scaffold_agents(&ctx, &[]);
    let project = maybe_create_project_step(orkia_dir, &scaffolded);
    print_done_summary(orkia_dir, &scaffolded, project.as_deref(), migrate_summary);
}

pub fn relaunch_menu(home: &Path, orkia_dir: &Path, args: &SetupArgs) {
    banner("orkia — existing setup detected");
    let registry = ArchetypeRegistry::new(orkia_dir);
    let existing = agent_dir::load_all_definitions(orkia_dir);
    let names: Vec<String> = existing.iter().map(|d| d.name.clone()).collect();
    eprintln!(
        "  agents:  {}",
        if names.is_empty() {
            "(none)".into()
        } else {
            names.join(", ")
        }
    );
    let workspace = Workspace::load(orkia_dir);
    let projects: Vec<String> = workspace.projects.iter().map(|p| p.name.clone()).collect();
    eprintln!(
        "  projects: {}",
        if projects.is_empty() {
            "(none)".into()
        } else {
            projects.join(", ")
        }
    );
    if registry.is_cached() {
        eprintln!("  registry: cached at {}", registry.path().display());
    } else {
        eprintln!("  registry: not cached");
    }
    loop {
        match menu_choice() {
            MenuChoice::AddAgent => {
                let tools = DetectedTools::scan();
                let ctx = ArchetypeCtx {
                    orkia_dir,
                    registry: &registry,
                    tools: &tools,
                    offline: args.offline,
                };
                let exclude = existing_archetype_names(orkia_dir);
                let scaffolded = pick_and_scaffold_agents(&ctx, &exclude);
                if !scaffolded.is_empty() {
                    eprintln!("  ✓ added {} agent(s)", scaffolded.len());
                }
            }
            MenuChoice::AddProject => {
                if let Some(p) = maybe_create_project_step(orkia_dir, &[]) {
                    eprintln!("  ✓ project '{p}' created");
                }
            }
            MenuChoice::UpdateRegistry => sync_registry_step(&registry, false),
            MenuChoice::RerunMigration => {
                migrate_rc_step(home);
            }
            MenuChoice::Reconfigure => {
                if ask_yn("wipe ~/.orkia and re-run the wizard from scratch?", false) {
                    let mut wipe_args = args.clone();
                    wipe_args.force = true;
                    if let Err(e) = super::run_setup(&wipe_args) {
                        eprintln!("  reconfigure failed: {e}");
                    }
                    break;
                }
            }
            MenuChoice::Quit => break,
            MenuChoice::Unknown => eprintln!("  (unrecognized choice)"),
        }
    }
}

fn banner(text: &str) {
    eprintln!();
    eprintln!("  \x1b[35m⬡\x1b[0m {text}");
    eprintln!();
}

fn step(title: &str) {
    eprintln!();
    eprintln!("  ─── {title} ───");
}

fn sync_registry_step(registry: &ArchetypeRegistry, offline: bool) {
    step("Registry");
    if offline {
        if registry.is_cached() {
            eprintln!("  --offline: using cached registry");
        } else {
            eprintln!("  --offline: no cache, falling back to builtin archetypes");
        }
        return;
    }
    eprintln!("  syncing archetypes from orkiaHQ/archetypes ...");
    match registry.sync() {
        Ok(()) => {
            let n = registry.list().len();
            eprintln!("  ✓ {n} archetype(s) available");
        }
        Err(e) => {
            eprintln!("  \x1b[33m⚠\x1b[0m sync failed ({e}); falling back to builtins");
        }
    }
}

fn migrate_rc_step(home: &Path) -> Option<MigrateSummary> {
    step("Shell Config");
    let orkiarc = home.join(".orkiarc");
    if orkiarc.exists() && !ask_yn("~/.orkiarc already exists — overwrite?", false) {
        eprintln!("  skipped");
        return None;
    }
    if !ask_yn("migrate ~/.zshrc (or .bashrc / fish) to ~/.orkiarc?", true) {
        eprintln!("  skipped");
        return None;
    }
    let opts = MigrateRcOpts::default();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    match run_migration(&opts, home, &orkiarc, &today) {
        Ok(report) => {
            eprintln!(
                "  ✓ {} migrated, {} translated, {} skipped (from {})",
                report.counts.migrated,
                report.counts.translated,
                report.counts.skipped,
                report.source_path.display(),
            );
            if let Some(err) = report.write_error {
                eprintln!("  \x1b[31m✗\x1b[0m write failed: {err}");
            }
            Some(MigrateSummary {
                source: report.source_path,
                migrated: report.counts.migrated,
                skipped: report.counts.skipped,
            })
        }
        Err(e) => {
            eprintln!("  \x1b[33m⚠\x1b[0m {e}");
            None
        }
    }
}

fn detect_tools_step() -> DetectedTools {
    step("CLI Tools");
    let tools = DetectedTools::scan();
    for t in &tools.tools {
        match &t.path {
            Some(p) => eprintln!("  ✓ {} — {}", t.name, p.display()),
            None => eprintln!("  ✗ {} — not found", t.name),
        }
    }
    if !tools.any_found() {
        eprintln!(
            "  \x1b[33m⚠\x1b[0m no known agent CLI found on PATH. Agents will be scaffolded\n  with `claude` as a placeholder; install at least one and edit agent.toml."
        );
    }
    tools
}

struct ArchetypeCtx<'a> {
    orkia_dir: &'a Path,
    registry: &'a ArchetypeRegistry,
    tools: &'a DetectedTools,
    offline: bool,
}

fn pick_and_scaffold_agents(ctx: &ArchetypeCtx<'_>, exclude: &[String]) -> Vec<String> {
    step("Agents");
    let all = available_archetypes(ctx.registry, ctx.offline);
    let candidates: Vec<&ArchetypeMeta> =
        all.iter().filter(|a| !exclude.contains(&a.name)).collect();
    if candidates.is_empty() {
        eprintln!("  (no archetypes available)");
        return Vec::new();
    }
    let options: Vec<(String, String)> = candidates
        .iter()
        .map(|a| {
            let label = if a.is_community {
                format!("{} (community)", a.name)
            } else {
                a.name.clone()
            };
            (label, a.description.clone())
        })
        .collect();
    let max_pick = candidates.len().min(if exclude.is_empty() { 3 } else { 1 });
    let prompt = if exclude.is_empty() {
        format!("select up to {max_pick} (comma-separated)")
    } else {
        "select one".into()
    };
    let picks = ask_select(&prompt, &options, max_pick);
    if picks.is_empty() {
        eprintln!("  (nothing selected)");
        return Vec::new();
    }
    scaffold_selected(ctx.orkia_dir, &candidates, &picks, ctx.tools)
}

fn scaffold_selected(
    orkia_dir: &Path,
    candidates: &[&ArchetypeMeta],
    picks: &[usize],
    tools: &DetectedTools,
) -> Vec<String> {
    let agents_dir = orkia_dir.join("agents");
    let mut created = Vec::new();
    for &i in picks {
        let arch = candidates[i];
        let suggested = arch
            .suggested_names
            .first()
            .cloned()
            .unwrap_or_else(|| arch.name.clone());
        let name = ask(
            &format!("name for '{}' agent?", arch.name),
            Some(&suggested),
        );
        if name.is_empty() {
            continue;
        }
        let cli = tools
            .best_tool_for(&arch.preferred_cli)
            .unwrap_or("claude")
            .to_string();
        let cli = ask(&format!("CLI tool for {name}?"), Some(&cli));
        match scaffold_agent(&agents_dir, arch, &name, &cli) {
            Ok(dir) => {
                eprintln!("  ✓ {} ({}, {}) → {}", name, arch.name, cli, dir.display());
                created.push(name);
            }
            Err(e) => eprintln!("  ✗ {name}: {e}"),
        }
    }
    created
}

fn maybe_create_project_step(orkia_dir: &Path, _scaffolded: &[String]) -> Option<String> {
    step("Project");
    if !ask_yn("create a starter project?", true) {
        eprintln!("  skipped");
        return None;
    }
    let name = ask("project name?", Some("default"));
    if name.is_empty() {
        return None;
    }
    let description = ask("description (optional)?", Some(""));
    let projects_root = orkia_dir.join("projects");
    let desc = if description.is_empty() {
        None
    } else {
        Some(description.as_str())
    };
    match Workspace::create_project(&projects_root, &name, desc, None) {
        Ok(path) => {
            eprintln!("  ✓ created project '{name}' at {}", path.display());
            Some(name)
        }
        Err(e) => {
            eprintln!("  ✗ project: {e}");
            None
        }
    }
}

fn print_done_summary(
    orkia_dir: &Path,
    agents: &[String],
    project: Option<&str>,
    rc: Option<MigrateSummary>,
) {
    step("Done");
    eprintln!("  ~/.orkia/ at {}", orkia_dir.display());
    if !agents.is_empty() {
        eprintln!("  agents:   {}", agents.join(", "));
    }
    if let Some(p) = project {
        eprintln!("  project:  {p}");
    }
    if let Some(rc) = rc {
        eprintln!(
            "  rc:       {} (migrated: {}, skipped: {})",
            rc.source.display(),
            rc.migrated,
            rc.skipped,
        );
    }
    eprintln!();
    eprintln!("  start orkia:    orkia          (shell mode)");
    eprintln!("                  orkia --tui    (TUI mode)");
    eprintln!();
}

fn available_archetypes(registry: &ArchetypeRegistry, offline: bool) -> Vec<ArchetypeMeta> {
    // Online: always try the registry. Offline: try only if cached.
    let should_read = !offline || registry.is_cached();
    if should_read {
        let list = registry.list();
        if !list.is_empty() {
            return list;
        }
    }
    builtin_archetypes()
}

fn existing_archetype_names(orkia_dir: &Path) -> Vec<String> {
    agent_dir::load_all_definitions(orkia_dir)
        .into_iter()
        .map(|d| d.archetype)
        .collect()
}

enum MenuChoice {
    AddAgent,
    AddProject,
    UpdateRegistry,
    RerunMigration,
    Reconfigure,
    Quit,
    Unknown,
}

fn menu_choice() -> MenuChoice {
    eprintln!();
    eprintln!("    [1] add an agent");
    eprintln!("    [2] add a project");
    eprintln!("    [3] update archetype registry");
    eprintln!("    [4] re-run RC migration");
    eprintln!("    [5] reconfigure from scratch (wipes ~/.orkia)");
    eprintln!("    [q] quit");
    match ask("choose", Some("q")).as_str() {
        "1" => MenuChoice::AddAgent,
        "2" => MenuChoice::AddProject,
        "3" => MenuChoice::UpdateRegistry,
        "4" => MenuChoice::RerunMigration,
        "5" => MenuChoice::Reconfigure,
        "q" | "Q" | "quit" | "exit" => MenuChoice::Quit,
        _ => MenuChoice::Unknown,
    }
}

struct MigrateSummary {
    source: PathBuf,
    migrated: usize,
    skipped: usize,
}
