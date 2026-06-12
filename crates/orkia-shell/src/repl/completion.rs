// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    /// Boot the brush session on demand. `run()` calls this eagerly; the
    /// shell-dispatch path also calls it lazily so the engine survives
    /// callers that drive `tick` without going through `run()` (e.g. tests).
    pub(crate) async fn ensure_brush(&mut self) -> Result<(), ShellError> {
        if self.brush.is_some() {
            return Ok(());
        }

        // Tests use `Repl::new` without going through `run()`; they
        // shouldn't accidentally source the developer's `~/.bashrc`.
        // So lazy boot starts hermetic (default opts) — the eager
        // production path in `run()` builds with the configured opts
        // before the first prompt.
        let mut b = BrushSession::new().await?;
        self.surface_rc_warnings(&mut b);
        self.try_source_orkiarc(&mut b).await;
        self.cwd_cache = Some(b.cwd().to_path_buf());
        let arc = Arc::new(tokio::sync::Mutex::new(b));
        self.install_completion(arc.clone());
        self.brush = Some(arc);
        Ok(())
    }

    /// Eager bring-up used by `run()` — honours config flags + login
    /// detection and surfaces RC warnings as Error blocks. Offers the
    /// first-run migration before sourcing `.orkiarc`.
    pub(crate) async fn boot_brush_for_run(&mut self) -> Result<(), ShellError> {
        if self.brush.is_some() {
            return Ok(());
        }
        let opts = crate::engine::ShellEngineOptions {
            load_bashrc: self.config.load_bashrc.unwrap_or(true),
            load_profile: self.config.load_profile.unwrap_or(true),
            login: self.login_shell,
        };
        let mut b = BrushSession::new_with_options(opts).await?;
        self.surface_rc_warnings(&mut b);
        // First-run check happens before .orkiarc is sourced: if the
        // file doesn't exist yet and we can find an existing rc, ask
        // the user once if they want to convert it. After this the
        // normal .orkiarc source path runs (it'll either find the file
        // we just wrote or no-op).
        self.maybe_offer_first_run_migration();
        self.try_source_orkiarc(&mut b).await;
        self.cwd_cache = Some(b.cwd().to_path_buf());
        let arc = Arc::new(tokio::sync::Mutex::new(b));
        self.install_completion(arc.clone());
        self.brush = Some(arc);
        Ok(())
    }

    /// Refresh the helper's shared state (agents + history tail) so
    /// tab-completion and inline hints reflect the current session.
    /// Publishes a fresh snapshot atomically via `ArcSwap` — no lock,
    /// the rustyline completion worker reads via `load()`.
    pub(crate) fn refresh_completion_snapshot(&mut self) {
        use crate::completion::HelperShared;
        let Some(any) = self.renderer.completion_shared() else {
            return;
        };
        let Ok(shared) = any.downcast::<Arc<arc_swap::ArcSwap<HelperShared>>>() else {
            return;
        };
        let tail: Vec<String> = self
            .history
            .entries()
            .iter()
            .rev()
            .take(256)
            .map(|e| e.line.clone())
            .collect();
        let (team_identifiers, project_identifiers) = self.completion_snapshot_sources();
        // change (F1/F3); dynamic set (aliases) + frequency map re-read each
        // prompt (F2/F4). All cold — never on the keystroke path.
        let stable_commands = {
            let current = shared.load();
            self.refresh_stable_commands(&current.stable_commands)
        };
        let dynamic_commands = self.read_dynamic_commands();
        let command_freq = self.build_command_freq();
        let next = HelperShared {
            agents: self.agents.iter().map(|a| a.name.clone()).collect(),
            history_tail: tail.into_iter().rev().collect(),
            team_identifiers,
            project_identifiers,
            stable_commands,
            dynamic_commands,
            command_freq,
        };
        shared.store(Arc::new(next));
    }

    /// rescanning `$PATH` only when a PATH dir's mtime (or the set of dirs)
    /// changed since the last scan. Otherwise clone the existing `Arc` (O(1)
    /// refcount, not a deep copy). `stat`ing the dirs is cold (pre-prompt).
    pub(crate) fn refresh_stable_commands(
        &mut self,
        current: &Arc<Vec<String>>,
    ) -> Arc<Vec<String>> {
        let fresh = current_path_mtimes();
        if current.is_empty() || fresh != self.path_mtimes {
            self.path_mtimes = fresh;
            Arc::new(self.scan_stable_commands())
        } else {
            current.clone()
        }
    }

    /// The sorted, deduped stable command set: builtin-table names ∪
    /// `CommandRegistry` names ∪ `$PATH` executable basenames. The expensive
    /// `$PATH` walk — only run when [`Self::refresh_stable_commands`] detects a
    /// change. Unreadable PATH dirs are skipped silently.
    pub(crate) fn scan_stable_commands(&self) -> Vec<String> {
        use std::collections::BTreeSet;
        let mut set: BTreeSet<String> = crate::builtin_table::completion_names()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        set.extend(self.registry.names());
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if let Ok(name) = entry.file_name().into_string() {
                        set.insert(name);
                    }
                }
            }
        }
        set.into_iter().collect()
    }

    /// alias names, sorted. Re-read each prompt so a session-defined alias is
    /// recognized next prompt. A non-blocking `try_lock` on the brush mutex:
    /// if it's momentarily held, the set is left empty this prompt (graceful
    /// degradation) — never blocks the pre-prompt path. Function names are
    /// deferred (no `brush_core::Shell` accessor yet).
    pub(crate) fn read_dynamic_commands(&self) -> Vec<String> {
        match self.brush.as_ref() {
            Some(brush) => read_alias_names(brush),
            None => Vec::new(),
        }
    }

    /// recent history (cold). Raw counts over a bounded window — enough to rank
    /// `git` over `gcc`; recency decay is a deferred refinement.
    pub(crate) fn build_command_freq(&self) -> std::collections::HashMap<String, f64> {
        const FREQ_WINDOW: usize = 2000;
        let mut freq: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        for entry in self.history.entries().iter().rev().take(FREQ_WINDOW) {
            if let Some(first) = entry.line.split_whitespace().next() {
                *freq.entry(first.to_string()).or_insert(0.0) += 1.0;
            }
        }
        freq
    }

    /// completion. Returns `(team_identifiers, project_identifiers)`.
    /// Best-effort: an empty cache or sync read failure yields empty
    /// vecs (callers degrade to non-cache completion paths).
    pub(crate) fn completion_snapshot_sources(&self) -> (Vec<String>, Vec<String>) {
        // Best-effort blocking read of the in-memory cache — the cache
        // RwLock is uncontested between mutations so this typically
        // returns immediately. `try_read` avoids ever blocking the
        // REPL's pre-prompt refresh path.
        let cache_data = {
            let lock = self.team_cache.inner_lock();
            lock.try_read().ok().and_then(|g| g.clone())
        };
        let team_identifiers = cache_data
            .as_ref()
            .map(|d| d.teams.iter().map(|t| t.identifier.clone()).collect())
            .unwrap_or_default();
        let project_identifiers = self
            .workspace
            .projects
            .iter()
            .map(|p| p.name.clone())
            .collect();
        (team_identifiers, project_identifiers)
    }

    /// Build the completion helper backed by brush and install it on
    /// the renderer. Also seed the helper's shared state with current
    /// agents + a snapshot of the most recent history lines.
    pub(crate) fn install_completion(&mut self, brush: Arc<tokio::sync::Mutex<BrushSession>>) {
        use crate::completion::{BrushCompletionProvider, HelperShared, OrkiaHelper};

        // Try to pull the shared box the renderer already created.
        let shared = match self.renderer.completion_shared() {
            Some(any) => match any.downcast::<Arc<arc_swap::ArcSwap<HelperShared>>>() {
                Ok(boxed) => *boxed,
                Err(_) => HelperShared::new_arc(),
            },
            None => return,
        };

        // Seed agents + history tail before plugging the helper in.
        let history_tail = self
            .history
            .entries()
            .iter()
            .rev()
            .take(256)
            .map(|e| e.line.clone())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let (team_identifiers, project_identifiers) = self.completion_snapshot_sources();
        // Force the initial stable scan (empty Arc ⇒ rescan + record mtimes).
        let stable_commands = self.refresh_stable_commands(&Arc::new(Vec::new()));
        let command_freq = self.build_command_freq();
        // `self.brush` isn't set until after this call, so read aliases from
        // the brush handle we were handed — `.orkiarc` aliases are live now.
        let dynamic_commands = read_alias_names(&brush);
        shared.store(Arc::new(HelperShared {
            agents: self.agents.iter().map(|a| a.name.clone()).collect(),
            history_tail,
            team_identifiers,
            project_identifiers,
            stable_commands,
            dynamic_commands,
            command_freq,
        }));

        let provider = BrushCompletionProvider::spawn(brush);
        let helper = OrkiaHelper::new(Box::new(provider), shared);
        self.renderer.install_completion_helper(Box::new(helper));
    }

    /// Source `~/.orkiarc` if present. Syntax errors are surfaced as a
    /// warning block but do not abort startup.
    pub(crate) async fn try_source_orkiarc(&mut self, b: &mut BrushSession) {
        let Some(home) = dirs_home() else {
            return;
        };
        let path = home.join(".orkiarc");
        match b.source_if_exists(&path).await {
            Ok(_) => {}
            Err(err) => {
                self.emit_block(BlockContent::Error(format!("{}: {err}", path.display())));
            }
        }
    }
}
