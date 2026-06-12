// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use super::*;

impl Repl {
    pub(crate) fn parse_builtin(&self, line: &str) -> Result<Decision, ShellError> {
        let mut rest = line.trim();
        // Strip optional `orkia` / `/` prefixes recursively (matches classifier).
        loop {
            if let Some(r) = rest.strip_prefix("orkia ") {
                rest = r.trim();
                continue;
            }
            if rest == "orkia" {
                rest = "";
                break;
            }
            if let Some(r) = rest.strip_prefix('/') {
                rest = r.trim();
                continue;
            }
            break;
        }

        let tokens = tokenize_args(rest);
        let mut it = tokens.into_iter();
        // Empty (bare `orkia`) lands on `help`. The per-command argument parsing
        // + name validation live in `dispatch_named`; this just splits the line
        // into a command name and its raw args.
        let name = it.next().unwrap_or_else(|| "help".to_string());
        let args: Vec<String> = it.collect();
        Ok(Decision::Builtin { name, args })
    }

    pub(crate) fn parse_agent_or_pipeline(
        &self,
        line: &str,
        first_agent: String,
    ) -> Result<Decision, ShellError> {
        if line.contains('|') {
            return parse_pipeline(line).map(Decision::Pipeline);
        }
        // Bare `@` (no name, no body) — most likely a typo or the user
        // is exploring. Emit a helpful Builtin-style hint instead of
        // silently no-op'ing through dispatch_agent.
        if first_agent.is_empty() && line.trim() == "@" {
            return Ok(Decision::Builtin {
                name: "agent".to_string(),
                args: Vec::new(),
            });
        }
        // Strip a standalone `--once` token (the single one-shot signal —
        // so plain `@faye … --once` and the sink form parse it identically.
        let (_, body, once) = crate::exec::parse::split_agent_stage(line);
        Ok(Decision::Agent {
            name: Some(first_agent),
            body,
            once,
        })
    }

    /// Snapshot of exported brush vars for propagation into agents.
    /// Empty if the engine hasn't booted yet (which should not happen
    /// after `run()` initialization).
    pub(crate) async fn shell_env_for_agents(&self) -> Vec<(String, String)> {
        match self.brush.as_ref() {
            Some(arc) => arc.lock().await.exported_env(),
            None => Vec::new(),
        }
    }

    /// The directory an agent should run in — the user's shell cwd
    /// (mirrored synchronously in `cwd_cache`), falling back to the
    /// process cwd. Used both as the spawn `working_dir` and as the
    /// directory whose trust we gate on, so the two always agree.
    pub(crate) fn agent_cwd(&self) -> Option<std::path::PathBuf> {
        let raw = self
            .cwd_cache
            .clone()
            .or_else(|| std::env::current_dir().ok())?;
        // Canonicalise so the trust dir, the spawn `working_dir`, and the
        // pre-trusted config key all agree (e.g. macOS `/var` → `/private/var`).
        Some(std::fs::canonicalize(&raw).unwrap_or(raw))
    }

    /// True if `dir` is trusted — either orkia recorded the user's
    /// consent, or the provider's own config already trusts it.
    pub(crate) fn dir_is_trusted(
        &self,
        dir: &std::path::Path,
        provider: orkia_shell_types::ProviderId,
    ) -> bool {
        if self.trust_registry.is_trusted(dir) {
            return true;
        }
        match trust_home() {
            Some(home) => crate::trust::provider_for(provider, home).is_trusted(dir),
            None => false,
        }
    }

    pub(crate) fn prompt_context(&mut self) -> PromptContext {
        let cwd_path = self
            .cwd_cache
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let cwd = cwd_path.display().to_string();
        let notifications = std::mem::take(&mut self.notification_queue);
        let rfc_scope = self.rfc_scope_segment_cache.clone();
        let scope = self.resolve_prompt_scope(&cwd_path);
        PromptContext {
            cwd,
            agent_count: self.agents.len(),
            seal_active: true,
            connected: self.connected,
            pending_approvals: self.approvals.pending().len(),
            attention_hint: self.attention.hint(),
            notifications,
            rfc_scope,
            scope,
        }
    }

    /// Resolve the effective scope for the prompt marker. Walks the
    /// inheritance chain workspace_default → project (matching cwd) →
    /// (rfc only when `rfc cd` is active). Returns `None` when no
    /// scope was ever declared along the chain so the renderer can
    /// suppress the marker entirely (the renderer also suppresses
    pub(crate) fn resolve_prompt_scope(
        &self,
        cwd: &std::path::Path,
    ) -> Option<orkia_shell_types::Scope> {
        let project_scope = self
            .workspace
            .resolve_project_name(None, cwd, self.config.default_project.as_deref())
            .and_then(|name| self.workspace.project(&name).cloned())
            .and_then(|p| p.scope);
        let rfc_scope = self.rfc_scope.as_ref().and_then(|s| {
            self.workspace
                .project(&s.project)
                .and_then(|p| p.rfcs.iter().find(|r| r.slug == s.rfc_id.as_str()))
                .and_then(|r| r.scope)
        });
        let workspace_default = self.config.default_scope;
        let effective = orkia_shell_types::resolve_effective_scope(
            workspace_default,
            project_scope,
            rfc_scope,
            None,
        );
        // Distinguish "explicit Private" from "unset". The prompt
        // renderer treats both as no-marker, but other callers may
        // want to know — return Some(effective) only if at least one
        // declaration existed along the chain.
        if workspace_default.is_some() || project_scope.is_some() || rfc_scope.is_some() {
            Some(effective)
        } else {
            None
        }
    }

    /// Re-load the scope segment from disk and populate the cache.
    /// Called eagerly on `rfc cd` and on every RFC builtin completion;
    /// the prompt path reads `rfc_scope_segment_cache` directly without
    /// any I/O.
    pub(crate) fn refresh_rfc_scope_segment(&mut self) {
        self.rfc_scope_segment_cache = self.load_rfc_scope_segment();
    }

    pub(crate) fn load_rfc_scope_segment(&self) -> Option<orkia_shell_types::RfcScopeSegment> {
        let scope = self.rfc_scope.as_ref()?;
        let project = self.workspace.project(&scope.project)?;
        let store = orkia_rfc_core::RfcStore::new(project.path.clone());
        let rec = store.load(&scope.rfc_id).ok()?;
        let counts = store.decision_counts(&scope.rfc_id).ok()?;
        Some(orkia_shell_types::RfcScopeSegment {
            id: rec.fm.id.0,
            state: format!("{:?}", rec.fm.state),
            version: rec.fm.version,
            open_clarifications: counts.open_clarifications,
            unreviewed_decisions: counts.unreviewed_decisions,
        })
    }
}
