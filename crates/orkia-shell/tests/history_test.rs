// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

use orkia_shell::history::History;
use orkia_shell_types::HistoryType;
use std::sync::Mutex;
use tempfile::tempdir;

/// Serialize tests that mutate process-global env vars (`HOME`).
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Build a `History` rooted at an isolated tempdir while shielding it
/// from the dev machine's real `~/.zsh_history` / `~/.bash_history`
/// (the first-launch seed path reads them via `$HOME`).
fn isolated_history() -> (
    tempfile::TempDir,
    History,
    std::sync::MutexGuard<'static, ()>,
) {
    let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempdir().unwrap();
    // SAFETY: process-wide env mutation is serialized on `ENV_LOCK`
    // (held by `guard` above) so no other test reads/writes env
    // concurrently for the lifetime of this fixture.
    unsafe {
        std::env::set_var("HOME", dir.path());
        std::env::remove_var("HISTFILE");
    }
    let h = History::new(dir.path());
    (dir, h, guard)
}

#[test]
fn push_and_iterate_in_order() {
    let (_dir, mut h, _g) = isolated_history();
    h.push_typed(HistoryType::Shell, "git status", None, None);
    h.push_typed(
        HistoryType::Intent,
        "fix the auth bug",
        Some("faye".into()),
        None,
    );
    h.push_typed(HistoryType::Approval, "approve job:1", None, Some(1));

    let entries = h.entries();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].entry_type, HistoryType::Shell);
    assert_eq!(entries[1].agent.as_deref(), Some("faye"));
    assert_eq!(entries[2].job_id, Some(1));
    assert!(entries[0].seq < entries[2].seq);
}

#[test]
fn dedup_consecutive_duplicates() {
    let (_dir, mut h, _g) = isolated_history();
    h.push_typed(HistoryType::Shell, "ls", None, None);
    h.push_typed(HistoryType::Shell, "ls", None, None);
    assert_eq!(h.entries().len(), 1);
}

#[test]
fn persist_and_reload() {
    let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempdir().unwrap();
    // SAFETY: process-wide env mutation is serialized on `ENV_LOCK`
    // (held by `guard` above) so no other test reads/writes env
    // concurrently for the lifetime of this fixture.
    unsafe {
        std::env::set_var("HOME", dir.path());
        std::env::remove_var("HISTFILE");
    }
    {
        let mut h = History::new(dir.path());
        h.push_typed(HistoryType::Shell, "git status", None, None);
        h.push_typed(HistoryType::Builtin, "ps", None, None);
    }
    let h = History::new(dir.path());
    let entries = h.entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].line, "git status");
    assert_eq!(entries[1].entry_type, HistoryType::Builtin);
    drop(guard);
}

#[test]
fn filter_by_type() {
    let (_dir, mut h, _g) = isolated_history();
    h.push_typed(HistoryType::Shell, "ls", None, None);
    h.push_typed(HistoryType::Intent, "fix the bug", None, None);
    h.push_typed(
        HistoryType::AgentDelegation,
        "@faye go",
        Some("faye".into()),
        None,
    );

    let agents = h.filter(50, |e| {
        matches!(
            e.entry_type,
            HistoryType::Intent | HistoryType::AgentDelegation
        )
    });
    assert_eq!(agents.len(), 2);
}

#[test]
fn search_finds_substring() {
    let (_dir, mut h, _g) = isolated_history();
    h.push_typed(HistoryType::Shell, "git status", None, None);
    h.push_typed(HistoryType::Shell, "git log", None, None);
    h.push_typed(HistoryType::Shell, "ls", None, None);
    let hits = h.search("git");
    assert_eq!(hits.len(), 2);
}
