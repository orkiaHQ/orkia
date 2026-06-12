// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Dual-emission helper for scope events.
//!
//! 1. SealChain append must succeed (durable, fail-closed).
//! 2. Journal publish runs after, best-effort.
//!
//! Failure of the SealChain append aborts the entire operation — the
//! caller must roll back any in-memory change. Failure of the Journal
//! append is logged inside `JournalStore::append` (which already returns
//! `()` and absorbs serialization/IPC errors) and does not unwind the
//! SealChain commit.
//!
//! PR1b ships the helper as foundation. Nothing in the shell calls it
//! yet — PR2 wires it into the user-facing builtins.

use orkia_shell_types::journal::types::{EventType, JournalEnvelope};
use orkia_shell_types::scope::Scope;
use orkia_shell_types::seal::SealError;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::journal::store::JournalStore;
use crate::seal::SealManager;

/// What happened, in a shape suitable for both the SealChain record and
/// the JournalEnvelope. Holds the durable form of the event before it
/// is split across the two sinks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeChangeEvent {
    /// SEAL event-kind string, e.g. `"workspace.scope_default_changed"`,
    /// `"project.scope_changed"`, `"rfc.scope_changed"`,
    /// `"issue.scope_changed"`, `"rfc.scope_set"` (creation).
    pub kind: String,

    /// Project chain to write the SEAL record to. `None` routes the
    /// record to the workspace-level chain via
    /// [`SealManager::seal_workspace`].
    pub project: Option<String>,

    /// Stable identifier of the artifact whose scope changed:
    /// project name, RFC slug, issue id, or workspace identifier.
    pub artifact_id: String,

    /// Previous scope, when this is a change. `None` for the initial
    /// set-on-creation case.
    pub previous: Option<Scope>,

    /// New scope (current state after the operation).
    pub current: Scope,

    /// The actor who triggered the change — an account id, agent name,
    /// or `"system"` for inheritance defaults.
    pub actor: String,
}

/// Emit a scope event durably (SealChain) then live (Journal).
///
/// Returns `Err` only if the SealChain append fails. A Journal append
/// failure is swallowed inside [`JournalStore::append`] per its existing
/// contract — that path is not meant to unwind the durable commit.
pub fn emit_scope_event(
    manager: &mut SealManager,
    journal: &mut JournalStore,
    event: ScopeChangeEvent,
) -> Result<(), SealError> {
    let detail = json!({
        "kind": event.kind,
        "artifact_id": event.artifact_id,
        "previous": event.previous.map(|s| s.as_str()),
        "current": event.current.as_str(),
        "actor": event.actor,
    });

    // 1. Durable first — fail-closed. If this returns Err the caller
    //    must roll back its in-memory edit and never publish step 2.
    match event.project.as_deref() {
        Some(project) => manager.seal_project(project, &event.kind, detail.clone())?,
        None => manager.seal_workspace(&event.kind, detail.clone())?,
    }

    // 2. Live publish — best-effort. JournalStore::append returns ()
    //    and logs internally on failure; we don't propagate.
    let mut envelope = JournalEnvelope::now(EventType::ScopeChange);
    envelope.source = Some("scope".to_string());
    envelope.event = Some(event.kind.clone());
    if let Some(project) = event.project.as_ref() {
        envelope.target = Some(project.clone());
    }
    if let Some(obj) = detail.as_object() {
        for (k, v) in obj {
            envelope.extra.insert(k.clone(), v.clone());
        }
    }
    journal.append(&envelope);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, SealManager, JournalStore) {
        let dir = tempdir().unwrap();
        let manager = SealManager::new(dir.path().to_path_buf());
        let journal = JournalStore::new(dir.path());
        (dir, manager, journal)
    }

    fn workspace_event() -> ScopeChangeEvent {
        ScopeChangeEvent {
            kind: "workspace.scope_default_changed".into(),
            project: None,
            artifact_id: "ws-1".into(),
            previous: Some(Scope::Private),
            current: Scope::Team,
            actor: "test-user".into(),
        }
    }

    fn project_event() -> ScopeChangeEvent {
        ScopeChangeEvent {
            kind: "project.scope_changed".into(),
            project: Some("test-project".into()),
            artifact_id: "test-project".into(),
            previous: Some(Scope::Private),
            current: Scope::Public,
            actor: "test-user".into(),
        }
    }

    fn read_journal(dir: &Path) -> String {
        // Give the writer thread a beat to flush the line.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::read_to_string(dir.join("journal.jsonl")).unwrap_or_default()
    }

    #[test]
    fn dual_emission_workspace_event_writes_both_sides() {
        let (dir, mut mgr, mut journal) = setup();
        emit_scope_event(&mut mgr, &mut journal, workspace_event()).expect("emit ok");

        // SEAL: workspace chain has one record.
        let chain = mgr.workspace_chain().expect("workspace chain created");
        assert_eq!(chain.len(), 1);
        let rec = &chain.records()[0];
        assert_eq!(rec.event_type, "workspace.scope_default_changed");
        assert_eq!(
            rec.detail.get("current").and_then(|v| v.as_str()),
            Some("team")
        );

        // Journal: an envelope landed on disk. EventType::ScopeChange
        // serializes as "scopechange" via the enum's rename_all rule.
        let body = read_journal(dir.path());
        assert!(body.contains("scopechange"), "missing type: {body}");
        assert!(body.contains("workspace.scope_default_changed"));
    }

    #[test]
    fn dual_emission_project_event_writes_both_sides() {
        let (dir, mut mgr, mut journal) = setup();
        emit_scope_event(&mut mgr, &mut journal, project_event()).expect("emit ok");

        let chain = mgr.project_chain("test-project").expect("project chain");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.records()[0].event_type, "project.scope_changed");

        let body = read_journal(dir.path());
        assert!(body.contains("project.scope_changed"));
    }

    #[cfg(unix)]
    #[test]
    fn seal_failure_aborts_emit_and_skips_journal() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, mut mgr, mut journal) = setup();

        // Force the workspace chain to exist + then make its parent
        // directory read-only so the second append's open() fails.
        emit_scope_event(&mut mgr, &mut journal, workspace_event()).expect("first emit ok");
        let ws_dir = dir.path().join("workspace");
        let chain_path = ws_dir.join("seal.jsonl");
        let original_dir = std::fs::metadata(&ws_dir).unwrap().permissions();
        let original_file = std::fs::metadata(&chain_path).unwrap().permissions();
        let mut ro_file = original_file.clone();
        ro_file.set_mode(0o444);
        std::fs::set_permissions(&chain_path, ro_file).unwrap();
        let mut ro_dir = original_dir.clone();
        ro_dir.set_mode(0o555);
        std::fs::set_permissions(&ws_dir, ro_dir).unwrap();

        // Snapshot the journal contents BEFORE the failing emit.
        let before = read_journal(dir.path());

        let mut second = workspace_event();
        second.kind = "workspace.scope_default_changed_again".into();
        let result = emit_scope_event(&mut mgr, &mut journal, second);

        // Restore perms so tempdir cleanup succeeds.
        std::fs::set_permissions(&ws_dir, original_dir).unwrap();
        std::fs::set_permissions(&chain_path, original_file).unwrap();

        assert!(
            matches!(result, Err(SealError::Io(_))),
            "expected Io error, got {result:?}",
        );
        // Journal was not touched by the failing emit. Compare the file
        // contents before and after to confirm no new line was written.
        let after = read_journal(dir.path());
        assert_eq!(before, after, "journal must not be written on SEAL failure");
    }

    #[test]
    fn journal_failure_is_swallowed() {
        // JournalStore::append returns `()` and logs internally on
        // serialization or writer-thread failure. We can't easily
        // synthesize one of those failures from the outside, so the
        // pragmatic test is: a successful SealChain append + a noop
        // journal still yields Ok and the SealChain holds the record.
        // The fact that emit_scope_event's signature does not propagate
        // any journal-side failure is itself the load-bearing contract.
        let (_dir, mut mgr, mut journal) = setup();
        emit_scope_event(&mut mgr, &mut journal, project_event()).expect("ok");
        let chain = mgr.project_chain("test-project").expect("chain");
        assert_eq!(chain.len(), 1);
    }
}
